//! BorgBase remote panel (BorgMac's `BorgBaseSheet`): API-token entry, the
//! account's repositories (quota, region, encryption, last activity) and SSH
//! keys, with edit / compact / delete and one-click import.
use super::borg_wizard::Segmented;
use super::kit::{self, Tint, icons};
use super::state::{AppState, connect_while_alive};
use super::borgbase_edit;
use crate::core::borgbase::{self, BorgBaseRepo, BorgBaseSSHKey};
use crate::core::format;
use adw::prelude::*;
use chrono::{DateTime, Utc};
use gtk::{gio, glib};
use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::rc::Rc;

const ICON_REGION: &str = "mark-location-symbolic"; // mappin.and.ellipse
const ICON_ENCRYPTION: &str = "security-high-symbolic"; // lock.shield
const ICON_CLOCK: &str = "preferences-system-time-symbolic"; // clock

/// A server-side compaction we believe is still in flight.
struct CompactEntry {
    triggered_at: DateTime<Utc>,
    usage_at_trigger_mb: f64,
}

struct Panel {
    app: Rc<AppState>,
    dialog: glib::WeakRef<adw::Dialog>,
    root: gtk::Box,
    spinner: gtk::Spinner,
    disconnect: gtk::Button,
    refresh: gtk::Button,
    /// "token" (entry screen) or "content" (section picker + lists).
    pages: gtk::Stack,
    token: gtk::PasswordEntry,
    save_token: gtk::Button,
    section: Segmented,
    content: adw::Bin,
    on_import: Box<dyn Fn(String, String)>,

    has_token: Cell<bool>,
    loading: Cell<bool>,
    repos: RefCell<Vec<BorgBaseRepo>>,
    ssh_keys: RefCell<Vec<BorgBaseSSHKey>>,
    /// Tracks repos that have a server-side compaction in flight. BorgBase
    /// exposes no progress stream, so we keep this in-memory (per-session)
    /// and clear entries either when `currentUsage` drops below the value at
    /// trigger time, or after a 60-minute safety timeout.
    compacting: RefCell<HashMap<String, CompactEntry>>,
    /// The "Compacting · 3 min ago" pills on screen, re-labelled periodically.
    pills: RefCell<Vec<(String, gtk::Label)>>,
}

// ------------------------------------------------------------------ helpers

/// Formats a MB value (as returned by BorgBase's GraphQL) as a human-readable
/// string. Uses decimal units (1000-based) to match how BorgBase itself
/// reports sizes on its web dashboard.
fn format_mb(mb: f64) -> String {
    format::bytes_file((mb * 1_000_000.0) as i64)
}

/// `localizedStandardCompare`: case-insensitive, digit runs compare by value
/// ("repo2" < "repo10").
fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (a, b) = (a.to_lowercase(), b.to_lowercase());
    let (mut x, mut y) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (x.peek().copied(), y.peek().copied()) {
            (None, None) => return a.cmp(&b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(c), Some(d)) if c.is_ascii_digit() && d.is_ascii_digit() => {
                let take = |it: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut digits = String::new();
                    while let Some(ch) = it.peek().copied().filter(char::is_ascii_digit) {
                        digits.push(ch);
                        it.next();
                    }
                    digits.trim_start_matches('0').to_string()
                };
                let (m, n) = (take(&mut x), take(&mut y));
                let ord = m.len().cmp(&n.len()).then_with(|| m.cmp(&n));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(c), Some(d)) => {
                if c != d {
                    return c.cmp(&d);
                }
                x.next();
                y.next();
            }
        }
    }
}

/// Used fraction of the quota, when one is enforced.
fn quota_ratio(repo: &BorgBaseRepo) -> Option<f64> {
    let quota = repo.quota.unwrap_or(0.0);
    (repo.quota_enabled == Some(true) && quota > 0.0).then(|| (repo.current_usage.unwrap_or(0.0) / quota).min(1.0))
}

/// Drops compaction entries that either (a) completed — detected by a usage
/// drop vs the value at trigger time — or (b) hit the 60-minute safety
/// timeout so a stuck request doesn't linger forever.
fn prune_compact_entries(entries: &mut HashMap<String, CompactEntry>, fresh: &[BorgBaseRepo], now: DateTime<Utc>) {
    entries.retain(|id, entry| {
        if now.signed_duration_since(entry.triggered_at).num_seconds() > 3600 {
            return false;
        }
        let usage = fresh.iter().find(|r| &r.id == id).and_then(|r| r.current_usage);
        !matches!(usage, Some(new_usage) if new_usage < entry.usage_at_trigger_mb)
    });
}

/// Small icon + caption pair (SwiftUI `Label(text, systemImage:)`).
fn icon_label(icon_name: &str, text: &str, classes: &[&str]) -> gtk::Box {
    let b = kit::hbox(4);
    for c in classes {
        b.add_css_class(c);
    }
    b.append(&kit::icon_styled(icon_name, &["bb-small-icon"]));
    b.append(&kit::label(text, &["caption"]));
    b
}

fn list_row(child: &impl IsA<gtk::Widget>) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();
    row.set_activatable(false);
    row.set_selectable(false);
    row.set_child(Some(child));
    row
}

fn inset_list() -> gtk::ListBox {
    let list = gtk::ListBox::new();
    list.add_css_class("mac-list");
    list.add_css_class("bb-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    list
}

// -------------------------------------------------------------------- views

impl Panel {
    /// Parent for alerts, confirmations and nested sheets.
    fn anchor(&self) -> &gtk::Box {
        &self.root
    }

    fn show_error(&self, message: &str) {
        kit::alert(self.anchor(), "Error", message);
    }

    fn repo_already_imported(&self, repo: &BorgBaseRepo) -> bool {
        let Some(path) = repo.repo_path.as_deref() else { return false };
        self.app.repos.repositories().iter().any(|r| r.url == path)
    }

    /// Syncs every widget with the current state.
    fn render(self: &Rc<Self>) {
        let has_token = self.has_token.get();
        let loading = self.loading.get();
        self.spinner.set_visible(loading);
        self.spinner.set_spinning(loading);
        self.disconnect.set_visible(has_token);
        self.refresh.set_visible(has_token);
        self.refresh.set_sensitive(!loading);
        self.pages.set_visible_child_name(if has_token { "content" } else { "token" });
        self.save_token.set_sensitive(!self.token.text().trim().is_empty());
        if let Some(dialog) = self.dialog.upgrade() {
            dialog.set_default_widget(if has_token { None } else { Some(&self.save_token) });
        }

        self.pills.borrow_mut().clear();
        let view: gtk::Widget = if self.section.selected() == 0 { self.repos_view() } else { self.keys_view() };
        self.content.set_child(Some(&view));
    }

    fn repos_view(self: &Rc<Self>) -> gtk::Widget {
        let repos = self.repos.borrow();
        if repos.is_empty() {
            let loading = self.loading.get();
            return kit::empty_state(
                icons::DRIVE,
                if loading { "Loading…" } else { "No repositories" },
                if loading {
                    "Querying BorgBase…"
                } else {
                    "Your account has no repositories or the token lacks permissions."
                },
            )
            .upcast();
        }
        let page = kit::vbox(0);
        page.append(&self.aggregate_strip(&repos));
        page.append(&kit::divider());
        let list = inset_list();
        for repo in repos.iter() {
            list.append(&list_row(&self.repo_row(repo)));
        }
        page.append(&kit::scrolled(&list));
        page.upcast()
    }

    /// Compact summary strip above the repo list. Uses only data we already
    /// fetch (currentUsage, quota, alertDays, lastModified), so it adds no
    /// new queries.
    fn aggregate_strip(&self, repos: &[BorgBaseRepo]) -> gtk::Box {
        let total_used: f64 = repos.iter().map(|r| r.current_usage.unwrap_or(0.0)).sum();
        let near_quota = repos
            .iter()
            .filter(|r| {
                let quota = r.quota.unwrap_or(0.0);
                r.quota_enabled == Some(true)
                    && quota > 0.0
                    && r.current_usage.is_some_and(|used| used / quota > 0.75)
            })
            .count();
        let stale = repos.iter().filter(|r| r.is_stale()).count();

        let item = |icon_name: &str, value: &str, label: &str, tint: Option<Tint>| {
            let b = kit::hbox(5);
            let tint_class = tint.map(|t| t.fg_class());
            let icon = kit::icon_styled(icon_name, &["bb-small-icon"]);
            let value = kit::label(value, &["bb-aggregate-value"]);
            match &tint_class {
                Some(class) => {
                    icon.add_css_class(class);
                    value.add_css_class(class);
                }
                None => icon.add_css_class("secondary"),
            }
            b.append(&icon);
            b.append(&value);
            b.append(&kit::label(label, &["caption", "secondary"]));
            b
        };

        let strip = kit::hbox(14);
        strip.add_css_class("bb-strip");
        strip.append(&item(icons::DRIVE, &repos.len().to_string(), if repos.len() == 1 { "repo" } else { "repos" }, None));
        strip.append(&item(icons::CHART, &format_mb(total_used), "used", None));
        if near_quota > 0 {
            strip.append(&item(icons::WARNING, &near_quota.to_string(), "near quota", Some(Tint::Orange)));
        }
        if stale > 0 {
            strip.append(&item(icons::CLOCK_QUESTION, &stale.to_string(), "stale", Some(Tint::Orange)));
        }
        strip
    }

    fn quota_bar(&self, repo: &BorgBaseRepo) -> gtk::Box {
        // BorgBase's `currentUsage` and `quota` are reported in **megabytes**
        // (decimal MB, float). The web dashboard does the unit conversion for
        // display; raw API values look GB-sized but are ~1000× smaller.
        let ratio = quota_ratio(repo);
        let used_text = format_mb(repo.current_usage.unwrap_or(0.0));
        let total_text = match ratio {
            Some(_) => format_mb(repo.quota.unwrap_or(0.0)),
            None => "∞".to_string(),
        };
        let value = ratio.unwrap_or(0.0);
        let tint_class = if value > 0.9 {
            "c-red"
        } else if value > 0.75 {
            "c-orange"
        } else {
            "tint"
        };

        let b = kit::vbox(2);
        let line = kit::hbox(6);
        line.append(&kit::icon_styled(icons::CHART, &["bb-tiny-icon", "secondary"]));
        let usage = kit::label(&format!("{used_text} / {total_text}"), &["caption", "bb-digits"]);
        usage.add_css_class(if value > 0.85 { tint_class } else { "secondary" });
        line.append(&usage);
        if ratio.is_some() {
            line.append(&kit::label(&format!("· {:.0}%", value * 100.0), &["caption", "bb-digits", "secondary"]));
        }
        b.append(&line);
        if ratio.is_some() {
            let bar = gtk::ProgressBar::new();
            bar.set_fraction(value);
            bar.set_size_request(260, -1);
            bar.set_halign(gtk::Align::Start);
            match tint_class {
                "c-red" => bar.add_css_class("red"),
                "c-orange" => bar.add_css_class("orange"),
                _ => {}
            }
            b.append(&bar);
        }
        b
    }

    fn repo_row(self: &Rc<Self>, repo: &BorgBaseRepo) -> gtk::Box {
        let row = kit::hbox(12);
        let drive = kit::icon_styled(icons::DRIVE, &["tint", "big-icon"]);
        drive.set_valign(gtk::Align::Center);
        row.append(&drive);

        let info = kit::vbox(4);
        info.set_hexpand(true);
        info.append(&kit::ellipsized(&repo.name, false, &["heading"]));
        if let Some(path) = repo.repo_path.as_deref() {
            let l = kit::ellipsized(path, true, &["mono", "bb-path", "secondary"]);
            l.set_selectable(true);
            info.append(&l);
        }
        info.append(&self.quota_bar(repo));
        let meta = kit::hbox(12);
        if let Some(region) = repo.region.as_deref() {
            meta.append(&icon_label(ICON_REGION, region, &["secondary"]));
        }
        if let Some(encryption) = repo.encryption.as_deref().filter(|e| !e.is_empty()) {
            meta.append(&icon_label(ICON_ENCRYPTION, encryption, &["secondary"]));
        }
        let stale = repo.is_stale();
        // Brand-new repos that never received a backup have no parseable date.
        if let Some(date) = repo.last_modified_date() {
            let activity =
                icon_label(ICON_CLOCK, &format::relative_short(&date), &[if stale { "c-orange" } else { "secondary" }]);
            activity.set_tooltip_text(Some(&if stale {
                format!(
                    "This repo hasn't been modified within its alert window ({} days).",
                    repo.alert_days.unwrap_or(0)
                )
            } else {
                "Last activity reported by BorgBase.".to_string()
            }));
            meta.append(&activity);
        }
        if stale {
            meta.append(&kit::badge("Stale", Tint::Orange));
        }
        info.append(&meta);
        row.append(&info);

        // ------------------------------------------------------------ import
        let imported = self.repo_already_imported(repo);
        let import = kit::button("");
        let import_content = kit::hbox(6);
        import_content.append(&kit::icon(icons::DOWNLOAD));
        import_content.append(&gtk::Label::new(Some("Import")));
        import.set_child(Some(&import_content));
        import.set_valign(gtk::Align::Center);
        let path = repo.repo_path.clone().unwrap_or_default();
        import.set_sensitive(!path.is_empty() && !imported);
        import.set_tooltip_text(Some(if imported {
            "This repository is already added locally."
        } else {
            "Prefill a new local repository with this URL."
        }));
        let name = repo.name.clone();
        import.connect_clicked(glib::clone!(
            #[weak(rename_to = panel)]
            self,
            move |_| {
                if path.is_empty() {
                    return;
                }
                if let Some(dialog) = panel.dialog.upgrade() {
                    dialog.close();
                }
                (panel.on_import)(name.clone(), path.clone());
            }
        ));
        row.append(&import);

        let compacting = self.compacting.borrow().get(&repo.id).map(|e| e.triggered_at);
        if let Some(since) = compacting {
            let pill = kit::hbox(4);
            pill.add_css_class("bb-pill");
            pill.set_valign(gtk::Align::Center);
            pill.append(&kit::icon_styled(icons::HOURGLASS, &["bb-tiny-icon"]));
            let text = kit::label(&format!("Compacting · {}", format::relative_short(&since)), &[]);
            pill.append(&text);
            pill.set_tooltip_text(Some(
                "BorgBase is compacting this repo in the background. The pill disappears when usage drops or after 60 minutes.",
            ));
            self.pills.borrow_mut().push((repo.id.clone(), text));
            row.append(&pill);
        }

        // -------------------------------------------------------------- menu
        let menu = gio::Menu::new();
        menu.append(Some("Edit…"), Some("repo.edit"));
        menu.append(Some("Compact on BorgBase…"), Some("repo.compact"));
        let danger = gio::Menu::new();
        danger.append(Some("Delete repo…"), Some("repo.delete"));
        menu.append_section(None, &danger);
        let more = gtk::MenuButton::new();
        more.set_icon_name(icons::ELLIPSIS_CIRCLE);
        more.set_always_show_arrow(true);
        more.set_menu_model(Some(&menu));
        more.add_css_class("flat");
        more.add_css_class("secondary");
        more.set_valign(gtk::Align::Center);
        more.set_tooltip_text(Some("More actions"));

        let actions = gio::SimpleActionGroup::new();
        let add = |name: &str, enabled: bool, f: fn(&Rc<Panel>, BorgBaseRepo)| {
            let action = gio::SimpleAction::new(name, None);
            action.set_enabled(enabled);
            let repo = repo.clone();
            action.connect_activate(glib::clone!(
                #[weak(rename_to = panel)]
                self,
                move |_, _| f(&panel, repo.clone())
            ));
            actions.add_action(&action);
        };
        add("edit", true, Panel::edit_repo);
        add("compact", compacting.is_none(), Panel::confirm_compact);
        add("delete", true, Panel::confirm_delete_repo);
        more.insert_action_group("repo", Some(&actions));
        row.append(&more);
        row
    }

    fn keys_view(self: &Rc<Self>) -> gtk::Widget {
        let keys = self.ssh_keys.borrow();
        if keys.is_empty() {
            let loading = self.loading.get();
            return kit::empty_state(
                icons::KEY,
                if loading { "Loading…" } else { "No SSH keys" },
                if loading { "Querying BorgBase…" } else { "No keys registered in your account." },
            )
            .upcast();
        }
        let list = inset_list();
        for key in keys.iter() {
            let row = kit::hbox(12);
            let icon = kit::icon_styled(icons::KEY, &["tint", "bb-key-icon"]);
            icon.set_valign(gtk::Align::Center);
            row.append(&icon);
            let info = kit::vbox(2);
            info.set_hexpand(true);
            info.append(&kit::ellipsized(&key.name, false, &["heading"]));
            let details = kit::hbox(10);
            if let Some(kind) = key.key_type.as_deref() {
                details.append(&kit::label(kind, &["caption", "secondary"]));
            }
            if let Some(bits) = key.bits {
                details.append(&kit::label(&format!("{bits} bits"), &["caption", "secondary"]));
            }
            if let Some(hash) = key.hash_md5.as_deref() {
                details.append(&kit::ellipsized(hash, true, &["caption", "mono", "tertiary"]));
            }
            info.append(&details);
            row.append(&info);

            let delete = kit::icon_button(icons::TRASH, "Delete SSH key from BorgBase");
            delete.add_css_class("secondary");
            delete.set_valign(gtk::Align::Center);
            let key = key.clone();
            delete.connect_clicked(glib::clone!(
                #[weak(rename_to = panel)]
                self,
                move |_| panel.confirm_delete_key(key.clone())
            ));
            row.append(&delete);
            list.append(&list_row(&row));
        }
        kit::scrolled(&list).upcast()
    }

    // ---------------------------------------------------------------- actions

    fn reload(self: &Rc<Self>) {
        self.loading.set(true);
        self.render();
        let weak = Rc::downgrade(self);
        kit::bg(
            || {
                // Both queries at once, like the Swift `async let` pair.
                std::thread::scope(|scope| {
                    let keys = scope.spawn(borgbase::ssh_keys);
                    let repos = borgbase::repos();
                    let keys = keys.join().unwrap_or_else(|_| Err(borgbase::ERR_INVALID_RESPONSE.to_string()));
                    Ok::<_, String>((repos?, keys?))
                })
            },
            move |result| {
                let Some(panel) = weak.upgrade() else { return };
                panel.loading.set(false);
                match result {
                    Ok((mut repos, mut keys)) => {
                        repos.sort_by(|a, b| natural_cmp(&a.name, &b.name));
                        keys.sort_by(|a, b| natural_cmp(&a.name, &b.name));
                        prune_compact_entries(&mut panel.compacting.borrow_mut(), &repos, Utc::now());
                        *panel.repos.borrow_mut() = repos;
                        *panel.ssh_keys.borrow_mut() = keys;
                        panel.render();
                    }
                    Err(e) => {
                        panel.render();
                        panel.show_error(&e);
                    }
                }
            },
        );
    }

    fn save_token(self: &Rc<Self>) {
        let trimmed = self.token.text().trim().to_string();
        if trimmed.is_empty() {
            return;
        }
        self.save_token.set_sensitive(false);
        let weak = Rc::downgrade(self);
        kit::bg(
            move || borgbase::save_token(&trimmed),
            move |result| {
                let Some(panel) = weak.upgrade() else { return };
                match result {
                    Ok(()) => {
                        panel.has_token.set(true);
                        panel.reload();
                    }
                    Err(e) => {
                        panel.render();
                        panel.show_error(&e);
                    }
                }
            },
        );
    }

    fn disconnect(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        kit::bg(borgbase::forget_token, move |()| {
            let Some(panel) = weak.upgrade() else { return };
            panel.has_token.set(false);
            panel.repos.borrow_mut().clear();
            panel.ssh_keys.borrow_mut().clear();
            panel.token.set_text("");
            panel.render();
        });
    }

    fn edit_repo(self: &Rc<Self>, repo: BorgBaseRepo) {
        let keys = self.ssh_keys.borrow().clone();
        let weak = Rc::downgrade(self);
        borgbase_edit::present(self.anchor(), &self.app, &repo, &keys, move || {
            if let Some(panel) = weak.upgrade() {
                panel.reload();
            }
        });
    }

    fn confirm_compact(self: &Rc<Self>, repo: BorgBaseRepo) {
        let weak = Rc::downgrade(self);
        kit::confirm(
            self.anchor(),
            &format!("Compact {}?", repo.name),
            "Runs `borg compact` on BorgBase's side to reclaim space freed by pruned archives. The repo stays usable but writes may be slower while it runs.",
            "Compact now",
            "Cancel",
            false,
            move || {
                if let Some(panel) = weak.upgrade() {
                    panel.compact_repo(repo.clone());
                }
            },
        );
    }

    fn compact_repo(self: &Rc<Self>, repo: BorgBaseRepo) {
        let weak = Rc::downgrade(self);
        let id = repo.id.clone();
        kit::bg(
            move || borgbase::compact_repo(&id),
            move |result| {
                let Some(panel) = weak.upgrade() else { return };
                match result {
                    Ok(()) => {
                        panel.compacting.borrow_mut().insert(
                            repo.id.clone(),
                            CompactEntry { triggered_at: Utc::now(), usage_at_trigger_mb: repo.current_usage.unwrap_or(0.0) },
                        );
                        panel.render();
                        kit::alert(
                            panel.anchor(),
                            "BorgBase",
                            &format!(
                                "Compaction started on BorgBase for “{}”. It runs in the background — usage should drop over the next minutes.",
                                repo.name
                            ),
                        );
                    }
                    Err(e) => panel.show_error(&e),
                }
            },
        );
    }

    fn confirm_delete_repo(self: &Rc<Self>, repo: BorgBaseRepo) {
        let weak = Rc::downgrade(self);
        kit::confirm(
            self.anchor(),
            &format!("Delete repo {}?", repo.name),
            "This deletes the repository and all its archives on BorgBase. It cannot be undone.",
            "Delete permanently",
            "Cancel",
            true,
            move || {
                if let Some(panel) = weak.upgrade() {
                    panel.delete_repo(repo.clone());
                }
            },
        );
    }

    fn delete_repo(self: &Rc<Self>, repo: BorgBaseRepo) {
        let weak = Rc::downgrade(self);
        let id = repo.id.clone();
        kit::bg(
            move || borgbase::delete_repo(&id),
            move |result| {
                let Some(panel) = weak.upgrade() else { return };
                match result {
                    Ok(()) => {
                        panel.remove_local_counterpart(&repo);
                        panel.reload();
                    }
                    Err(e) => panel.show_error(&e),
                }
            },
        );
    }

    /// Removes any local `Repository` whose URL matches the just-deleted
    /// BorgBase repo's path, so the main sidebar doesn't keep a dangling
    /// entry whose upstream no longer exists.
    fn remove_local_counterpart(&self, repo: &BorgBaseRepo) {
        let Some(path) = repo.repo_path.as_deref().filter(|p| !p.is_empty()) else { return };
        for local in self.app.repos.repositories().iter().filter(|r| r.url == path) {
            self.app.repos.remove(local);
        }
    }

    fn confirm_delete_key(self: &Rc<Self>, key: BorgBaseSSHKey) {
        let weak = Rc::downgrade(self);
        kit::confirm(
            self.anchor(),
            &format!("Delete key {}?", key.name),
            "Repos whose only access was this key will no longer be reachable.",
            "Delete",
            "Cancel",
            true,
            move || {
                let Some(panel) = weak.upgrade() else { return };
                let weak = Rc::downgrade(&panel);
                let id = key.id.clone();
                kit::bg(
                    move || borgbase::delete_ssh_key(&id),
                    move |result| {
                        let Some(panel) = weak.upgrade() else { return };
                        match result {
                            Ok(()) => panel.reload(),
                            Err(e) => panel.show_error(&e),
                        }
                    },
                );
            },
        );
    }
}

/// `on_import(name, url)` is called when the user imports a BorgBase repo;
/// the panel closes itself first.
pub fn present(parent: &impl IsA<gtk::Widget>, app: &Rc<AppState>, on_import: impl Fn(String, String) + 'static) {
    let (dialog, root) = kit::bare_sheet("BorgBase", 780, 560);

    // --------------------------------------------------------------- header
    let spinner = gtk::Spinner::new();
    let disconnect = kit::button("Disconnect");
    disconnect.set_tooltip_text(Some("Remove the BorgBase API token from this computer"));
    root.append(&kit::panel_header(
        icons::CLOUD,
        "BorgBase",
        "Remote panel: repositories and SSH keys from your account.",
        &[spinner.upcast_ref(), disconnect.upcast_ref()],
    ));
    root.append(&kit::divider());

    // ---------------------------------------------------------- token entry
    let token_page = kit::vbox(14);
    token_page.add_css_class("bb-token-page");
    token_page.append(&kit::label("Enter your BorgBase API token", &["heading"]));
    token_page.append(&kit::caption(
        "You can generate one at https://www.borgbase.com/settings/access_tokens with read scope to list repos and keys. It is stored in the local keyring.",
    ));
    let token = gtk::PasswordEntry::builder().placeholder_text("Paste your token here").activates_default(true).build();
    token_page.append(&token);
    let save_token = kit::primary_button("Save and connect");
    token_page.append(&kit::button_row(None, &[&save_token]));

    // -------------------------------------------------------------- content
    let content_page = kit::vbox(0);
    let picker_row = kit::hbox(8);
    picker_row.add_css_class("bb-section-picker");
    picker_row.set_halign(gtk::Align::Center);
    picker_row.append(&kit::label("Section", &[]));
    let section = Segmented::new(&["Repositories", "SSH Keys"], 0);
    picker_row.append(&section.widget);
    content_page.append(&picker_row);
    content_page.append(&kit::divider());
    let content = adw::Bin::new();
    content.set_vexpand(true);
    content_page.append(&content);

    let pages = gtk::Stack::new();
    pages.set_vexpand(true);
    pages.add_named(&token_page, Some("token"));
    pages.add_named(&content_page, Some("content"));
    root.append(&pages);
    root.append(&kit::divider());

    // --------------------------------------------------------------- footer
    let refresh = kit::button("");
    let refresh_content = kit::hbox(6);
    refresh_content.append(&kit::icon(icons::REFRESH));
    refresh_content.append(&gtk::Label::new(Some("Refresh")));
    refresh.set_child(Some(&refresh_content));
    let close = kit::button("Close");
    let footer = kit::button_row(Some(refresh.upcast_ref()), &[&close]);
    footer.add_css_class("bb-footer");
    root.append(&footer);

    let panel = Rc::new(Panel {
        app: app.clone(),
        dialog: dialog.downgrade(),
        root: root.clone(),
        spinner,
        disconnect,
        refresh,
        pages,
        token,
        save_token,
        section,
        content,
        on_import: Box::new(on_import),
        has_token: Cell::new(false),
        loading: Cell::new(false),
        repos: RefCell::new(vec![]),
        ssh_keys: RefCell::new(vec![]),
        compacting: RefCell::new(HashMap::new()),
        pills: RefCell::new(vec![]),
    });

    panel.disconnect.connect_clicked(glib::clone!(
        #[weak]
        panel,
        move |_| panel.disconnect()
    ));
    panel.refresh.connect_clicked(glib::clone!(
        #[weak]
        panel,
        move |_| panel.reload()
    ));
    panel.token.connect_changed(glib::clone!(
        #[weak]
        panel,
        move |token| panel.save_token.set_sensitive(!token.text().trim().is_empty())
    ));
    panel.save_token.connect_clicked(glib::clone!(
        #[weak]
        panel,
        move |_| panel.save_token()
    ));
    panel.section.connect_changed(glib::clone!(
        #[weak]
        panel,
        move |_| panel.render()
    ));
    close.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| {
            dialog.close();
        }
    ));
    // The "Import" buttons depend on which repositories exist locally.
    connect_while_alive(
        app,
        |a| &a.repos.changed,
        &root,
        glib::clone!(
            #[weak]
            panel,
            move || panel.render()
        ),
    );

    // Keeps the "Compacting · 3 min ago" pills current. Stops by itself once
    // the panel is gone.
    let weak = Rc::downgrade(&panel);
    glib::timeout_add_seconds_local(30, move || {
        let Some(panel) = weak.upgrade() else { return glib::ControlFlow::Break };
        let compacting = panel.compacting.borrow();
        for (id, label) in panel.pills.borrow().iter() {
            if let Some(entry) = compacting.get(id) {
                label.set_text(&format!("Compacting · {}", format::relative_short(&entry.triggered_at)));
            }
        }
        glib::ControlFlow::Continue
    });

    panel.render();
    // The keyring lookup may block on an unlock prompt: keep it off the main thread.
    kit::bg(
        borgbase::has_token,
        glib::clone!(
            #[weak]
            panel,
            move |has_token| {
                if has_token {
                    panel.has_token.set(true);
                    panel.reload();
                }
            }
        ),
    );

    // The dialog owns the panel state; every handler holds a weak reference.
    // (Hung on the dialog rather than on `root`, which the panel itself owns.)
    let keep = RefCell::new(Some(panel));
    dialog.connect_closed(move |_| {
        keep.borrow_mut().take();
    });
    dialog.present(Some(parent));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(json: &str) -> BorgBaseRepo {
        serde_json::from_str(json).expect("valid repo json")
    }

    #[test]
    fn natural_ordering() {
        let mut names = vec!["repo10", "Repo2", "alpha", "repo1", "Beta"];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(names, ["alpha", "Beta", "repo1", "Repo2", "repo10"]);
    }

    #[test]
    fn sizes_and_ratios() {
        assert_eq!(format_mb(3550.0), "3.55 GB");
        assert_eq!(format_mb(589.7), "589.7 MB");
        assert_eq!(quota_ratio(&repo(r#"{"id":"1","currentUsage":50,"quota":100,"quotaEnabled":true}"#)), Some(0.5));
        assert_eq!(quota_ratio(&repo(r#"{"id":"1","currentUsage":500,"quota":100,"quotaEnabled":true}"#)), Some(1.0));
        assert_eq!(quota_ratio(&repo(r#"{"id":"1","currentUsage":50,"quota":100,"quotaEnabled":false}"#)), None);
        assert_eq!(quota_ratio(&repo(r#"{"id":"1","currentUsage":50}"#)), None);
    }

    #[test]
    fn compaction_entries_expire() {
        let now = Utc::now();
        let entry = |minutes: i64, usage: f64| CompactEntry {
            triggered_at: now - chrono::Duration::minutes(minutes),
            usage_at_trigger_mb: usage,
        };
        let mut entries = HashMap::new();
        entries.insert("running".to_string(), entry(5, 100.0));
        entries.insert("dropped".to_string(), entry(5, 100.0));
        entries.insert("timed-out".to_string(), entry(61, 100.0));
        entries.insert("gone".to_string(), entry(5, 100.0));
        let fresh = vec![
            repo(r#"{"id":"running","currentUsage":100}"#),
            repo(r#"{"id":"dropped","currentUsage":80}"#),
            repo(r#"{"id":"timed-out","currentUsage":100}"#),
        ];
        prune_compact_entries(&mut entries, &fresh, now);
        let mut left: Vec<&str> = entries.keys().map(String::as_str).collect();
        left.sort();
        assert_eq!(left, ["gone", "running"]);
    }
}
