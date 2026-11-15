//! "Stale alerts" sheet for one BorgBox repo and the alert editor (BorgMac's
//! `BorgBoxAlertsSheet` + `BorgBoxAlertEditorSheet`): webhook alerts the
//! daemon fires when a repo hasn't received a backup in a while.
use super::kit::{self, Tint, icons};
use super::state::AppState;
use crate::core::borgbox::{self, BorgBoxAlert, BorgBoxAlertUpdate};
use crate::core::format;
use crate::core::models::BorgBoxServer;
use adw::prelude::*;
use gtk::{gio, glib};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// Header shared by the BorgBox sheets: tinted icon, bold title and a caption
/// (monospaced when it names a repo). Returns the row and the caption label.
pub(super) fn sheet_header(icon: &str, title: &str, subtitle: &str, mono: bool) -> (gtk::Box, gtk::Label) {
    let row = kit::hbox(10);
    let image = kit::icon_styled(icon, &["tint", "bb2-header-icon"]);
    image.set_valign(gtk::Align::Center);
    row.append(&image);
    let texts = kit::vbox(2);
    texts.set_valign(gtk::Align::Center);
    texts.set_hexpand(true);
    texts.append(&kit::label(title, &["bb2-title3"]));
    let caption = kit::caption(subtitle);
    if mono {
        caption.add_css_class("mono");
    }
    texts.append(&caption);
    row.append(&texts);
    (row, caption)
}

// MARK: - List sheet (entry point from the BorgBox server panel repo row)

#[derive(Clone, PartialEq)]
enum TestOutcome {
    Ok,
    Failed(String),
}

struct Sheet {
    server: BorgBoxServer,
    repo: String,
    /// Content box of the sheet; nested sheets and alerts present over it.
    host: gtk::Box,
    stack: gtk::Stack,
    list: gtk::ListBox,
    error: gtk::Label,
    refresh: gtk::Button,
    alerts: RefCell<Vec<BorgBoxAlert>>,
    loading: Cell<bool>,
    /// Ids of the alerts with a webhook test in flight.
    testing: RefCell<HashSet<String>>,
    /// The last test result per alert, shown inline in its row. Keyed by
    /// alert so a retest on another alert doesn't stomp this one's state.
    outcomes: RefCell<HashMap<String, TestOutcome>>,
}

pub fn present(parent: &impl IsA<gtk::Widget>, _app: &Rc<AppState>, server: &BorgBoxServer, repo: &str) {
    let (dialog, content) = kit::sheet("", 560, 520);
    dialog.set_title("Stale alerts");

    let (header, _) = sheet_header(icons::BELL, "Stale alerts", repo, true);
    let refresh = kit::icon_button(icons::REFRESH, "Refresh");
    refresh.add_css_class("plain");
    refresh.set_valign(gtk::Align::Center);
    header.append(&refresh);
    let add = kit::button("");
    let add_label = kit::hbox(4);
    add_label.append(&kit::icon(icons::PLUS));
    add_label.append(&gtk::Label::new(Some("Add")));
    add.set_child(Some(&add_label));
    add.set_tooltip_text(Some("Add"));
    add.set_valign(gtk::Align::Center);
    header.append(&add);
    content.append(&header);
    content.append(&kit::divider());

    let stack = gtk::Stack::new();
    stack.set_vexpand(true);

    let loading = kit::vbox(8);
    loading.set_valign(gtk::Align::Center);
    loading.set_halign(gtk::Align::Center);
    loading.append(&kit::spinner());
    loading.append(&kit::label("Loading…", &["secondary"]));
    stack.add_named(&loading, Some("loading"));

    let empty = kit::vbox(8);
    empty.set_valign(gtk::Align::Center);
    let empty_icon = kit::icon_styled(icons::BELL_SLASH, &["secondary", "bb2-empty-icon"]);
    empty.append(&empty_icon);
    let empty_title = kit::label("No alerts configured", &["bb2-headline", "secondary"]);
    empty_title.set_halign(gtk::Align::Center);
    empty.append(&empty_title);
    let empty_text =
        kit::caption("Get notified via a webhook when this repo hasn't received a backup in a while.");
    empty_text.set_justify(gtk::Justification::Center);
    empty_text.set_xalign(0.5);
    empty_text.set_margin_start(24);
    empty_text.set_margin_end(24);
    empty.append(&empty_text);
    stack.add_named(&empty, Some("empty"));

    let list = gtk::ListBox::new();
    list.add_css_class("mac-list");
    list.add_css_class("bb2-alert-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    stack.add_named(&kit::scrolled(&list), Some("list"));
    content.append(&stack);

    let error = kit::error_label();
    content.append(&error);

    let close = kit::button("Close");
    content.append(&kit::button_row(None, &[&close]));

    let sheet = Rc::new(Sheet {
        server: server.clone(),
        repo: repo.to_string(),
        host: content.clone(),
        stack,
        list: list.clone(),
        error,
        refresh: refresh.clone(),
        alerts: RefCell::new(vec![]),
        loading: Cell::new(false),
        testing: RefCell::new(HashSet::new()),
        outcomes: RefCell::new(HashMap::new()),
    });

    close.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| {
            dialog.close();
        }
    ));
    refresh.connect_clicked(glib::clone!(
        #[weak]
        sheet,
        move |_| reload(&sheet)
    ));
    add.connect_clicked(glib::clone!(
        #[weak]
        sheet,
        move |_| edit(&sheet, None)
    ));
    // Tapping a row opens it in the editor.
    list.connect_row_activated(glib::clone!(
        #[weak]
        sheet,
        move |_, row| {
            let alert = sheet.alerts.borrow().get(row.index() as usize).cloned();
            if let Some(alert) = alert {
                edit(&sheet, Some(&alert));
            }
        }
    ));

    // Handlers only hold weak references; the sheet state lives until the
    // dialog is closed.
    let keep = RefCell::new(Some(sheet.clone()));
    dialog.connect_closed(move |_| {
        keep.borrow_mut().take();
    });

    reload(&sheet);
    dialog.present(Some(parent));
}

fn show_error(sheet: &Sheet, message: Option<&str>) {
    kit::set_error(&sheet.error, message);
}

fn reload(sheet: &Rc<Sheet>) {
    sheet.loading.set(true);
    sheet.refresh.set_sensitive(false);
    show_error(sheet, None);
    rebuild(sheet);
    let (server, repo) = (sheet.server.clone(), sheet.repo.clone());
    let weak = Rc::downgrade(sheet);
    kit::bg(
        move || borgbox::alerts_for_repo(&server, &repo),
        move |result| {
            let Some(sheet) = weak.upgrade() else { return };
            sheet.loading.set(false);
            sheet.refresh.set_sensitive(true);
            match result {
                Ok(alerts) => *sheet.alerts.borrow_mut() = alerts,
                Err(e) if e.starts_with("BorgBox returned HTTP 404:") => show_error(
                    &sheet,
                    Some("The daemon doesn't implement alerts yet. Update BorgBox to use them."),
                ),
                Err(e) => show_error(&sheet, Some(&e)),
            }
            rebuild(&sheet);
        },
    );
}

fn rebuild(sheet: &Rc<Sheet>) {
    let alerts = sheet.alerts.borrow().clone();
    sheet.list.remove_all();
    for alert in &alerts {
        sheet.list.append(&row(sheet, alert));
    }
    let page = if alerts.is_empty() {
        if sheet.loading.get() { "loading" } else { "empty" }
    } else {
        "list"
    };
    sheet.stack.set_visible_child_name(page);
}

fn edit(sheet: &Rc<Sheet>, existing: Option<&BorgBoxAlert>) {
    let weak = Rc::downgrade(sheet);
    present_editor(&sheet.host, &sheet.server, &sheet.repo, existing, move |_| {
        if let Some(sheet) = weak.upgrade() {
            reload(&sheet);
        }
    });
}

fn toggle(sheet: &Rc<Sheet>, alert: &BorgBoxAlert, enabled: bool) {
    let (server, id) = (sheet.server.clone(), alert.id.clone());
    let weak = Rc::downgrade(sheet);
    kit::bg(
        move || {
            let update = BorgBoxAlertUpdate { enabled: Some(enabled), ..Default::default() };
            borgbox::update_alert(&server, &id, &update)
        },
        move |result| {
            let Some(sheet) = weak.upgrade() else { return };
            match result {
                Ok(_) => reload(&sheet),
                Err(e) => {
                    show_error(&sheet, Some(&e));
                    // Put the switch back to the daemon's state.
                    rebuild(&sheet);
                }
            }
        },
    );
}

fn delete(sheet: &Rc<Sheet>, alert: &BorgBoxAlert) {
    let (server, id) = (sheet.server.clone(), alert.id.clone());
    let weak = Rc::downgrade(sheet);
    kit::bg(
        move || borgbox::delete_alert(&server, &id),
        move |result| {
            let Some(sheet) = weak.upgrade() else { return };
            match result {
                Ok(()) => reload(&sheet),
                Err(e) => show_error(&sheet, Some(&e)),
            }
        },
    );
}

fn test(sheet: &Rc<Sheet>, alert: &BorgBoxAlert) {
    if !sheet.testing.borrow_mut().insert(alert.id.clone()) {
        return;
    }
    let (server, id) = (sheet.server.clone(), alert.id.clone());
    let weak = Rc::downgrade(sheet);
    let alert_id = alert.id.clone();
    kit::bg(
        move || borgbox::test_alert(&server, &id),
        move |result| {
            let Some(sheet) = weak.upgrade() else { return };
            sheet.testing.borrow_mut().remove(&alert_id);
            let outcome = match result {
                Ok(()) => TestOutcome::Ok,
                Err(e) => TestOutcome::Failed(test_failure_message(&e)),
            };
            sheet.outcomes.borrow_mut().insert(alert_id, outcome);
            rebuild(&sheet);
        },
    );
}

/// The daemon's own reason (`webhook returned 404`) without the client's
/// "BorgBox: " prefix; plain HTTP failures read "HTTP <code>: <body>".
fn test_failure_message(error: &str) -> String {
    if let Some(reason) = error.strip_prefix("BorgBox: ") {
        reason.to_string()
    } else if let Some(http) = error.strip_prefix("BorgBox returned ") {
        http.to_string()
    } else {
        error.to_string()
    }
}

// MARK: - Row

fn summary_line(alert: &BorgBoxAlert) -> String {
    let mut parts = vec![format!("stale after {}h", alert.stale_after_hours)];
    if alert.renotify_hours > 0 {
        parts.push(format!("re-notify every {}h", alert.renotify_hours));
    } else {
        parts.push("edge-triggered".to_string());
    }
    if let Some(date) = alert.last_check_date() {
        parts.push(format!("checked {}", format::relative_short(&date)));
    }
    parts.join(" · ")
}

fn state_icon(alert: &BorgBoxAlert) -> gtk::Image {
    // Anything but "ok"/"stale" (never checked, or a state a future daemon
    // might emit) falls back to the neutral bell.
    let image = match alert.last_state.as_deref() {
        Some("ok") => kit::icon_styled(icons::CHECK_CIRCLE, &["c-green"]),
        Some("stale") => kit::icon_styled(icons::WARNING, &["c-orange"]),
        _ => kit::icon_styled(icons::BELL, &["secondary"]),
    };
    image.add_css_class("bb2-state-icon");
    image.set_size_request(28, -1);
    image.set_valign(gtk::Align::Start);
    image
}

fn outcome_label(outcome: &TestOutcome) -> gtk::Box {
    let (icon, text, class) = match outcome {
        TestOutcome::Ok => (icons::CHECK_CIRCLE, "Webhook responded OK", "c-green"),
        TestOutcome::Failed(message) => (icons::XMARK_CIRCLE, message.as_str(), "c-red"),
    };
    let row = kit::hbox(4);
    row.add_css_class(class);
    row.add_css_class("bb2-outcome");
    row.append(&kit::icon(icon));
    let label = kit::label(text, &["caption"]);
    label.set_wrap(true);
    label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    row.append(&label);
    row
}

fn row(sheet: &Rc<Sheet>, alert: &BorgBoxAlert) -> gtk::ListBoxRow {
    let content = kit::hbox(12);
    content.set_margin_top(4);
    content.set_margin_bottom(4);
    content.append(&state_icon(alert));

    let texts = kit::vbox(3);
    texts.set_hexpand(true);
    let first = kit::hbox(6);
    let url = kit::label(&alert.webhook_url, &["mono"]);
    url.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    first.append(&url);
    if !alert.enabled {
        first.append(&kit::badge("Disabled", Tint::Gray));
    }
    if alert.has_secret {
        let signed = kit::hbox(3);
        signed.add_css_class("badge");
        signed.add_css_class("b-blue");
        signed.set_valign(gtk::Align::Center);
        signed.append(&kit::icon(icons::LOCK));
        signed.append(&gtk::Label::new(Some("Signed")));
        signed.set_tooltip_text(Some("Webhook deliveries are HMAC-signed with this alert's secret."));
        first.append(&signed);
    }
    texts.append(&first);
    texts.append(&kit::caption(&summary_line(alert)));
    if let Some(outcome) = sheet.outcomes.borrow().get(&alert.id) {
        texts.append(&outcome_label(outcome));
    }
    if !alert.last_error.is_empty() {
        let error = kit::label(&alert.last_error, &["caption", "error-text"]);
        error.set_wrap(true);
        error.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        error.set_lines(2);
        error.set_ellipsize(gtk::pango::EllipsizeMode::End);
        texts.append(&error);
    }
    content.append(&texts);

    let switch = gtk::Switch::new();
    switch.set_active(alert.enabled);
    switch.set_valign(gtk::Align::Center);
    switch.set_tooltip_text(Some(if alert.enabled { "Disable this alert" } else { "Enable this alert" }));
    switch.connect_state_set(glib::clone!(
        #[strong]
        alert,
        #[weak]
        sheet,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_, enabled| {
            toggle(&sheet, &alert, enabled);
            glib::Propagation::Proceed
        }
    ));
    content.append(&switch);

    let menu = gio::Menu::new();
    let main = gio::Menu::new();
    main.append(Some("Edit"), Some("alert.edit"));
    main.append(Some("Test webhook"), Some("alert.test"));
    menu.append_section(None, &main);
    let danger = gio::Menu::new();
    danger.append(Some("Delete"), Some("alert.delete"));
    menu.append_section(None, &danger);
    let more = gtk::MenuButton::new();
    more.set_icon_name(icons::ELLIPSIS_CIRCLE);
    more.add_css_class("flat");
    more.set_valign(gtk::Align::Center);
    more.set_menu_model(Some(&menu));
    content.append(&more);

    let group = gio::SimpleActionGroup::new();
    let add_action = |name: &str, f: fn(&Rc<Sheet>, &BorgBoxAlert)| {
        let action = gio::SimpleAction::new(name, None);
        action.connect_activate(glib::clone!(
            #[weak]
            sheet,
            #[strong]
            alert,
            move |_, _| f(&sheet, &alert)
        ));
        group.add_action(&action);
        action
    };
    add_action("edit", |sheet, alert| edit(sheet, Some(alert)));
    let test_action = add_action("test", |sheet, alert| test(sheet, alert));
    test_action.set_enabled(!sheet.testing.borrow().contains(&alert.id));
    // Disable the item right away; the row is rebuilt when the test ends.
    test_action.connect_activate(|action, _| action.set_enabled(false));
    add_action("delete", |sheet, alert| {
        let weak = Rc::downgrade(sheet);
        let alert = alert.clone();
        kit::confirm(
            &sheet.host,
            "Delete this alert?",
            "The webhook URL won't receive any further stale notifications for this repo.",
            "Delete alert",
            "Cancel",
            true,
            move || {
                if let Some(sheet) = weak.upgrade() {
                    delete(&sheet, &alert);
                }
            },
        );
    });

    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&content));
    row.insert_action_group("alert", Some(&group));
    row
}

// MARK: - Editor sheet (create or edit a single alert)

fn is_valid(stale_after_hours: i64, webhook_url: &str) -> bool {
    let trimmed = webhook_url.trim();
    if stale_after_hours <= 0 || trimmed.is_empty() || trimmed.chars().any(char::is_whitespace) {
        return false;
    }
    matches!(trimmed.split_once(':'), Some(("http" | "https", _)))
}

pub fn present_editor(
    parent: &impl IsA<gtk::Widget>,
    server: &BorgBoxServer,
    repo: &str,
    existing: Option<&BorgBoxAlert>,
    on_saved: impl Fn(BorgBoxAlert) + 'static,
) {
    let title = if existing.is_none() { "New alert" } else { "Edit alert" };
    let (dialog, content) = kit::sheet("", 520, 560);
    dialog.set_title(title);
    let (header, _) = sheet_header(icons::BELL, title, repo, true);
    content.append(&header);
    content.append(&kit::divider());

    let form = kit::vbox(10);
    form.set_margin_end(4);

    let trigger = kit::form_group();
    trigger.set_title("Trigger");
    let stale = kit::spin_row("", existing.map_or(26, |a| a.stale_after_hours), 1, 720);
    let renotify = kit::spin_row("", existing.map_or(0, |a| a.renotify_hours), 0, 720);
    let enabled = adw::SwitchRow::builder().title("Enabled").active(existing.is_none_or(|a| a.enabled)).build();
    let stale_title = |row: &adw::SpinRow| row.set_title(&format!("Stale after {} h", row.value() as i64));
    let renotify_title = |row: &adw::SpinRow| {
        let hours = row.value() as i64;
        if hours == 0 {
            row.set_title("Re-notify: never (edge-triggered)");
        } else {
            row.set_title(&format!("Re-notify every {hours} h"));
        }
    };
    stale_title(&stale);
    renotify_title(&renotify);
    renotify.connect_value_notify(renotify_title);
    trigger.add(&stale);
    trigger.add(&renotify);
    trigger.add(&enabled);
    form.append(&trigger);

    let webhook_group = kit::form_group();
    let webhook = adw::EntryRow::builder()
        .title("Webhook URL (https://ntfy.sh/your-topic)")
        .text(existing.map_or("", |a| a.webhook_url.as_str()))
        .build();
    webhook.set_input_hints(gtk::InputHints::NO_SPELLCHECK);
    webhook_group.add(&webhook);
    webhook_group.set_margin_top(8);
    form.append(&webhook_group);
    form.append(&kit::caption_markup(
        "The daemon POSTs a JSON event (<tt>stale</tt> / <tt>recovered</tt> / <tt>test</tt>) to this URL. ntfy.sh topics work out of the box; any URL that accepts POST and returns 2xx is fine.",
    ));

    // Signing secret UI. Three shapes depending on mode:
    //  • Create                → plain secret field, "optional" prompt.
    //  • Edit, no secret yet   → same as create (non-destructive to enable).
    //  • Edit, already signed  → the prompt explains that leaving it empty
    //    keeps the current secret; a "Stop signing" switch below lets the
    //    user explicitly clear it on save.
    let already_signed = existing.is_some_and(|a| a.has_secret);
    let signing = kit::form_group();
    signing.set_title("Webhook signing");
    signing.set_margin_top(8);
    let secret = adw::PasswordEntryRow::builder()
        .title(if already_signed { "New secret (leave empty to keep current)" } else { "Signing secret (optional)" })
        .build();
    signing.add(&secret);
    let clear_signing = adw::SwitchRow::builder().title("Stop signing (clear current secret)").build();
    if already_signed {
        signing.add(&clear_signing);
        clear_signing.connect_active_notify(glib::clone!(
            #[weak]
            secret,
            move |row| secret.set_sensitive(!row.is_active())
        ));
    }
    form.append(&signing);
    form.append(&kit::caption_markup(
        "If set, BorgBox adds <tt>X-BorgBox-Signature: sha256=&lt;hmac&gt;</tt> to every webhook POST so the receiver can verify the call came from your daemon. Leave empty for unsigned deliveries.",
    ));
    content.append(&kit::scrolled(&form));

    let error = kit::error_label();
    content.append(&error);

    let cancel = kit::button("Cancel");
    let save = kit::primary_button(if existing.is_none() { "Create" } else { "Save" });
    content.append(&kit::button_row(None, &[&cancel, &save]));
    dialog.set_default_widget(Some(&save));

    let busy = Rc::new(Cell::new(false));
    let update = Rc::new(glib::clone!(
        #[weak]
        stale,
        #[weak]
        webhook,
        #[weak]
        save,
        #[strong]
        busy,
        move || save.set_sensitive(!busy.get() && is_valid(stale.value() as i64, &webhook.text()))
    ));
    update();
    stale.connect_value_notify(glib::clone!(
        #[strong]
        update,
        move |row| {
            stale_title(row);
            update();
        }
    ));
    webhook.connect_changed(glib::clone!(
        #[strong]
        update,
        move |_| update()
    ));

    cancel.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| {
            dialog.close();
        }
    ));

    let server = server.clone();
    let repo = repo.to_string();
    let existing = existing.cloned();
    let on_saved = Rc::new(on_saved);
    save.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        #[weak]
        error,
        move |_| {
            busy.set(true);
            update();
            kit::set_error(&error, None);

            let stale_after_hours = stale.value() as i64;
            let renotify_hours = renotify.value() as i64;
            let is_enabled = enabled.is_active();
            let trimmed_url = webhook.text().trim().to_string();
            // Not trimmed — spaces could be intentional in a secret.
            let typed_secret = secret.text().to_string();
            let clear = already_signed && clear_signing.is_active();
            let (server, repo, existing) = (server.clone(), repo.clone(), existing.clone());
            let (busy, update, on_saved) = (busy.clone(), update.clone(), on_saved.clone());
            kit::bg(
                move || match existing {
                    Some(existing) => {
                        // PATCH: three wire states for the secret.
                        //  None     → omit → daemon keeps the current secret.
                        //  Some("") → clear → daemon stops signing.
                        //  Some(x)  → set/rotate.
                        let secret = if clear {
                            Some(String::new())
                        } else if !typed_secret.is_empty() {
                            Some(typed_secret)
                        } else {
                            None
                        };
                        let update = BorgBoxAlertUpdate {
                            stale_after_hours: Some(stale_after_hours),
                            webhook_url: Some(trimmed_url),
                            enabled: Some(is_enabled),
                            renotify_hours: Some(renotify_hours),
                            secret,
                        };
                        borgbox::update_alert(&server, &existing.id, &update)
                    }
                    None => borgbox::create_alert(
                        &server,
                        &repo,
                        stale_after_hours,
                        &trimmed_url,
                        is_enabled,
                        renotify_hours,
                        Some(typed_secret.as_str()).filter(|s| !s.is_empty()),
                    ),
                },
                move |result| {
                    busy.set(false);
                    update();
                    match result {
                        Ok(saved) => {
                            on_saved(saved);
                            dialog.close();
                        }
                        Err(e) => kit::set_error(&error, Some(&e)),
                    }
                },
            );
        }
    ));

    dialog.present(Some(parent));
}
