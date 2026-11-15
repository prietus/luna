//! Repository detail pane (BorgMac's `RepositoryDetailView`): header, busy
//! banner while a scheduled backup holds the repo lock, and the archives
//! table with its context menu.
use super::kit::{self, Tint, icons};
use super::state::{AppState, connect_while_alive, refresh_statuses_soon};
use super::{archive_browser, create_backup, prune, schedule_sheet, treemap, update_passphrase};
use crate::core::models::{Archive, BackupRunStatus, BackupSchedule, Repository};
use crate::core::{borg, format, schedule};
use adw::prelude::*;
use gtk::{gdk, gio, glib};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

pub struct DetailView {
    pub root: gtk::Widget,
    /// Toolbar items to place in the window header while this view is shown.
    pub toolbar: gtk::Widget,
}

struct Detail {
    app: Rc<AppState>,
    repo: Repository,
    root: gtk::Box,
    store: gio::ListStore,
    selection: gtk::SingleSelection,
    table: gtk::ColumnView,
    spinner: gtk::Spinner,
    banner: gtk::Revealer,
    banner_progress: gtk::ProgressBar,
    banner_detail: gtk::Label,
    /// Buttons disabled while the repo is busy.
    busy_sensitive: Vec<gtk::Widget>,
    refresh_button: gtk::Button,
    actions: gio::SimpleActionGroup,
    loading: Cell<bool>,
    breaking_lock: Cell<bool>,
    was_busy: Cell<bool>,
    busy_schedule: RefCell<Option<BackupSchedule>>,
    pulse: RefCell<Option<glib::SourceId>>,
}

#[derive(Clone, Copy)]
enum ArchiveKind {
    Manual,
    Scheduled,
    Unknown,
}

fn archive_kind(name: &str) -> ArchiveKind {
    if name.starts_with("manual-") {
        ArchiveKind::Manual
    } else if name.starts_with("scheduled-") {
        ArchiveKind::Scheduled
    } else {
        ArchiveKind::Unknown
    }
}

impl ArchiveKind {
    fn label(self) -> &'static str {
        match self {
            ArchiveKind::Manual => "Manual",
            ArchiveKind::Scheduled => "Scheduled",
            ArchiveKind::Unknown => "—",
        }
    }
    fn tint(self) -> Tint {
        match self {
            ArchiveKind::Manual => Tint::Blue,
            ArchiveKind::Scheduled => Tint::Orange,
            ArchiveKind::Unknown => Tint::Gray,
        }
    }
}

fn tool_button(icon: &str, tooltip: &str) -> gtk::Button {
    let b = gtk::Button::from_icon_name(icon);
    b.set_tooltip_text(Some(tooltip));
    b
}

fn pill(children: &[&gtk::Widget]) -> gtk::Box {
    let b = kit::hbox(0);
    b.add_css_class("pill-group");
    b.set_valign(gtk::Align::Center);
    for c in children {
        b.append(*c);
    }
    b
}

/// Column whose cells are built once by `setup` and filled by `bind`.
fn column<W: IsA<gtk::Widget>>(
    title: &str,
    setup: impl Fn() -> W + 'static,
    bind: impl Fn(&W, &Archive) + 'static,
) -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_, obj| {
        let cell = obj.downcast_ref::<gtk::ColumnViewCell>().expect("column view cell");
        let holder = kit::hbox(0);
        holder.add_css_class("borg-cell");
        holder.append(&setup());
        cell.set_child(Some(&holder));
    });
    factory.connect_bind(move |_, obj| {
        let cell = obj.downcast_ref::<gtk::ColumnViewCell>().expect("column view cell");
        let Some(holder) = cell.child() else { return };
        // The context menu maps a click back to its row through this name.
        holder.set_widget_name(&cell.position().to_string());
        let (Some(widget), Some(archive)) = (
            holder.first_child().and_then(|w| w.downcast::<W>().ok()),
            cell.item().and_then(|o| kit::unbox::<Archive>(&o)),
        ) else {
            return;
        };
        bind(&widget, &archive);
    });
    let col = gtk::ColumnViewColumn::new(Some(title), Some(factory));
    col.set_resizable(true);
    col
}

pub fn build(app: &Rc<AppState>, repo: &Repository) -> DetailView {
    // ---------------------------------------------------------------- header
    let header = kit::hbox(12);
    header.set_margin_top(16);
    header.set_margin_bottom(16);
    header.set_margin_start(16);
    header.set_margin_end(16);
    header.append(&kit::icon_styled(icons::DRIVE, &["detail-header-icon"]));
    let names = kit::vbox(2);
    names.set_valign(gtk::Align::Center);
    names.append(&kit::label(&repo.name, &["title-3"]));
    let url = kit::ellipsized(&repo.url, true, &["caption", "secondary"]);
    url.set_selectable(true);
    url.set_focusable(false);
    names.append(&url);
    names.set_hexpand(true);
    header.append(&names);
    let spinner = gtk::Spinner::new();
    spinner.set_valign(gtk::Align::Center);
    header.append(&spinner);

    // ----------------------------------------------------------- busy banner
    let banner_row = kit::hbox(12);
    banner_row.add_css_class("banner-orange");
    banner_row.append(&kit::icon_styled(icons::SYNC, &["c-orange"]));
    let banner_texts = kit::vbox(4);
    banner_texts.set_hexpand(true);
    let banner_title = kit::label("Backup in progress", &[]);
    banner_title.set_markup("<b>Backup in progress</b>");
    banner_texts.append(&banner_title);
    let banner_progress = gtk::ProgressBar::new();
    banner_progress.add_css_class("orange");
    banner_texts.append(&banner_progress);
    let banner_detail = kit::ellipsized("", true, &["caption", "secondary"]);
    banner_texts.append(&banner_detail);
    banner_row.append(&banner_texts);
    let stop = kit::icon_button(icons::XMARK_CIRCLE, "Stop backup (SIGTERM to borg — leaves the repo consistent)");
    stop.add_css_class("secondary");
    stop.set_valign(gtk::Align::Center);
    banner_row.append(&stop);
    let banner = gtk::Revealer::new();
    banner.set_child(Some(&banner_row));

    // ----------------------------------------------------------------- table
    let store = gio::ListStore::new::<glib::BoxedAnyObject>();
    let selection = gtk::SingleSelection::new(Some(store.clone()));
    selection.set_autoselect(false);
    selection.set_can_unselect(true);
    let table = gtk::ColumnView::new(Some(selection.clone()));
    table.add_css_class("mac-table");
    table.set_show_row_separators(false);

    let name_col = column(
        "Name",
        || kit::ellipsized("", false, &[]),
        |l: &gtk::Label, a| {
            l.set_text(&a.name);
            l.set_tooltip_text(Some(&a.name));
        },
    );
    name_col.set_expand(true);
    table.append_column(&name_col);
    let type_col = column(
        "Type",
        || kit::badge("", Tint::Gray),
        |l: &gtk::Label, a| {
            let kind = archive_kind(&a.name);
            kit::set_badge(l, kind.label(), kind.tint());
        },
    );
    type_col.set_fixed_width(110);
    table.append_column(&type_col);
    let date_col =
        column("Date", || kit::label("", &[]), |l: &gtk::Label, a| l.set_text(&format::borg_date(&a.start)));
    date_col.set_fixed_width(210);
    table.append_column(&date_col);
    let id_col = column(
        "ID",
        || kit::label("", &["mono"]),
        |l: &gtk::Label, a| l.set_text(&a.archive_id.chars().take(12).collect::<String>()),
    );
    id_col.set_fixed_width(170);
    table.append_column(&id_col);

    let scroll = kit::scrolled(&table);

    let root = kit::vbox(0);
    root.append(&header);
    root.append(&banner);
    root.append(&kit::divider());
    root.append(&scroll);

    // --------------------------------------------------------------- toolbar
    let new_backup = tool_button(icons::PLUS_CIRCLE, "New backup");
    let prune_btn = tool_button(icons::SCISSORS, "Prune");
    let schedule_btn = tool_button(icons::HISTORY, "Schedule");
    let passphrase_btn = tool_button(
        icons::KEY,
        "Update passphrase — re-save the repo passphrase in the keyring (needed by scheduled backups)",
    );
    let more_menu = gio::Menu::new();
    more_menu.append(Some("Break lock…"), Some("archive.break-lock"));
    let more = gtk::MenuButton::new();
    more.set_icon_name(icons::ELLIPSIS_CIRCLE);
    more.set_always_show_arrow(true);
    more.set_menu_model(Some(&more_menu));
    more.set_tooltip_text(Some("Recovery actions"));
    let refresh_button = tool_button(icons::REFRESH, "Refresh");

    let toolbar = kit::hbox(8);
    toolbar.append(&pill(&[
        new_backup.upcast_ref(),
        prune_btn.upcast_ref(),
        schedule_btn.upcast_ref(),
        passphrase_btn.upcast_ref(),
    ]));
    toolbar.append(&pill(&[more.upcast_ref()]));
    toolbar.append(&pill(&[refresh_button.upcast_ref()]));

    let actions = gio::SimpleActionGroup::new();
    root.insert_action_group("archive", Some(&actions));
    toolbar.insert_action_group("archive", Some(&actions));

    let d = Rc::new(Detail {
        app: app.clone(),
        repo: repo.clone(),
        root: root.clone(),
        store,
        selection,
        table: table.clone(),
        spinner,
        banner,
        banner_progress,
        banner_detail,
        busy_sensitive: vec![new_backup.clone().upcast(), prune_btn.clone().upcast(), passphrase_btn.clone().upcast()],
        refresh_button: refresh_button.clone(),
        actions,
        loading: Cell::new(false),
        breaking_lock: Cell::new(false),
        was_busy: Cell::new(false),
        busy_schedule: RefCell::new(None),
        pulse: RefCell::new(None),
    });

    // --------------------------------------------------------------- actions
    let add_action = |name: &str, f: Box<dyn Fn(&Rc<Detail>)>| {
        let action = gio::SimpleAction::new(name, None);
        let weak = Rc::downgrade(&d);
        action.connect_activate(move |_, _| {
            if let Some(d) = weak.upgrade() {
                f(&d);
            }
        });
        d.actions.add_action(&action);
    };
    add_action(
        "browse",
        Box::new(|d| {
            if let Some(a) = selected(d) {
                archive_browser::present(&d.root, &d.app, &d.repo, &a);
            }
        }),
    );
    add_action(
        "analyze",
        Box::new(|d| {
            if let Some(a) = selected(d) {
                treemap::present(&d.root, &d.app, &d.repo, &a);
            }
        }),
    );
    add_action("restore", Box::new(restore));
    add_action("mount", Box::new(mount));
    add_action("delete", Box::new(confirm_delete));
    add_action("break-lock", Box::new(confirm_break_lock));

    new_backup.connect_clicked(glib::clone!(
        #[weak]
        d,
        move |_| {
            let weak = Rc::downgrade(&d);
            create_backup::present(&d.root, &d.app, &d.repo, move || {
                if let Some(d) = weak.upgrade() {
                    refresh(&d);
                }
            });
        }
    ));
    prune_btn.connect_clicked(glib::clone!(
        #[weak]
        d,
        move |_| {
            let weak = Rc::downgrade(&d);
            prune::present(&d.root, &d.app, &d.repo, move || {
                if let Some(d) = weak.upgrade() {
                    refresh(&d);
                }
            });
        }
    ));
    schedule_btn.connect_clicked(glib::clone!(
        #[weak]
        d,
        move |_| schedule_sheet::present(&d.root, &d.app, &d.repo)
    ));
    passphrase_btn.connect_clicked(glib::clone!(
        #[weak]
        d,
        move |_| update_passphrase::present(&d.root, &d.app, &d.repo)
    ));
    refresh_button.connect_clicked(glib::clone!(
        #[weak]
        d,
        move |_| refresh(&d)
    ));
    stop.connect_clicked(glib::clone!(
        #[weak]
        d,
        move |_| confirm_stop(&d)
    ));

    // Double click / Return: browse.
    table.connect_activate(glib::clone!(
        #[weak]
        d,
        move |_, position| {
            if let Some(a) = d.store.item(position).and_then(|o| kit::unbox::<Archive>(&o)) {
                archive_browser::present(&d.root, &d.app, &d.repo, &a);
            }
        }
    ));

    // Context menu.
    let menu = gio::Menu::new();
    let open = gio::Menu::new();
    open.append(Some("Browse…"), Some("archive.browse"));
    open.append(Some("Analyze space…"), Some("archive.analyze"));
    menu.append_section(None, &open);
    let recover = gio::Menu::new();
    recover.append(Some("Restore…"), Some("archive.restore"));
    recover.append(Some("Mount…"), Some("archive.mount"));
    menu.append_section(None, &recover);
    let danger = gio::Menu::new();
    danger.append(Some("Delete archive…"), Some("archive.delete"));
    menu.append_section(None, &danger);
    let popover = gtk::PopoverMenu::from_model(Some(&menu));
    popover.set_parent(&table);
    popover.set_has_arrow(false);
    popover.set_halign(gtk::Align::Start);
    table.connect_destroy(glib::clone!(
        #[weak]
        popover,
        move |_| popover.unparent()
    ));
    let click = gtk::GestureClick::new();
    click.set_button(gdk::BUTTON_SECONDARY);
    click.connect_pressed(glib::clone!(
        #[weak]
        d,
        #[weak]
        popover,
        move |gesture, _, x, y| {
            let Some(position) = row_at(&d.table, x, y) else { return };
            gesture.set_state(gtk::EventSequenceState::Claimed);
            d.selection.set_selected(position);
            popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            popover.popup();
        }
    ));
    table.add_controller(click);

    connect_while_alive(
        app,
        |a| &a.statuses.changed,
        &root,
        glib::clone!(
            #[weak]
            d,
            move || update_busy(&d)
        ),
    );

    // Keep the controller alive with its widget tree.
    let keep = RefCell::new(Some(d.clone()));
    root.connect_destroy(move |_| {
        if let Some(d) = keep.borrow_mut().take() {
            if let Some(id) = d.pulse.borrow_mut().take() {
                id.remove();
            }
        }
    });

    update_busy(&d);
    refresh(&d);

    DetailView { root: root.upcast(), toolbar: toolbar.upcast() }
}

/// Row index under the pointer, found through the `borg-cell` holders.
fn row_at(table: &gtk::ColumnView, x: f64, y: f64) -> Option<u32> {
    let mut widget = table.pick(x, y, gtk::PickFlags::DEFAULT)?;
    loop {
        if widget.has_css_class("borg-cell") {
            return widget.widget_name().parse().ok();
        }
        if let Some(child) = widget.first_child().filter(|c| c.has_css_class("borg-cell")) {
            return child.widget_name().parse().ok();
        }
        if &widget == table.upcast_ref::<gtk::Widget>() {
            return None;
        }
        widget = widget.parent()?;
    }
}

fn selected(d: &Rc<Detail>) -> Option<Archive> {
    d.selection.selected_item().and_then(|o| kit::unbox::<Archive>(&o))
}

/// The schedule of this repo that is currently running, if any.
fn busy_context(d: &Rc<Detail>) -> Option<(BackupSchedule, BackupRunStatus)> {
    let repo = d.app.repos.get(&d.repo.id).unwrap_or_else(|| d.repo.clone());
    repo.schedules.into_iter().find_map(|s| d.app.statuses.status(&s.id).filter(|st| st.running).map(|st| (s, st)))
}

fn is_busy(d: &Rc<Detail>) -> bool {
    busy_context(d).is_some()
}

fn busy_detail_line(status: &BackupRunStatus) -> String {
    let mut parts: Vec<String> = vec![];
    if let Some(bytes) = status.progress_original_bytes.filter(|b| *b > 0) {
        parts.push(format::bytes_binary(bytes));
    }
    if let Some(n) = status.progress_file_count.filter(|n| *n > 0) {
        parts.push(format!("{n} {}", if n == 1 { "file" } else { "files" }));
    }
    if let Some(ratio) = status.progress_ratio() {
        parts.push(format!("{:.0}%", ratio * 100.0));
    }
    if let Some(path) = status.progress_current_path.as_ref().filter(|p| !p.is_empty()) {
        parts.push(path.clone());
    }
    if parts.is_empty() { "Waiting for first progress tick…".to_string() } else { parts.join(" · ") }
}

fn update_busy(d: &Rc<Detail>) {
    let context = busy_context(d);
    let busy = context.is_some();
    d.banner.set_reveal_child(busy);
    match &context {
        Some((sched, status)) => {
            *d.busy_schedule.borrow_mut() = Some(sched.clone());
            d.banner_detail.set_text(&busy_detail_line(status));
            match status.progress_ratio() {
                Some(ratio) => {
                    if let Some(id) = d.pulse.borrow_mut().take() {
                        id.remove();
                    }
                    d.banner_progress.set_fraction(ratio);
                }
                None => {
                    if d.pulse.borrow().is_none() {
                        let bar = d.banner_progress.downgrade();
                        let id = glib::timeout_add_local(std::time::Duration::from_millis(120), move || match bar.upgrade()
                        {
                            Some(bar) => {
                                bar.pulse();
                                glib::ControlFlow::Continue
                            }
                            None => glib::ControlFlow::Break,
                        });
                        *d.pulse.borrow_mut() = Some(id);
                    }
                }
            }
        }
        None => {
            *d.busy_schedule.borrow_mut() = None;
            if let Some(id) = d.pulse.borrow_mut().take() {
                id.remove();
            }
        }
    }

    for w in &d.busy_sensitive {
        w.set_sensitive(!busy);
    }
    d.refresh_button.set_sensitive(!busy && !d.loading.get());
    for name in ["restore", "mount", "delete"] {
        set_action_enabled(d, name, !busy);
    }
    set_action_enabled(d, "break-lock", !busy && !d.breaking_lock.get());

    // Busy → idle: the backup finished, pull the fresh archive list so the
    // new archive appears without user action.
    if d.was_busy.replace(busy) && !busy {
        refresh(d);
    }
}

fn set_action_enabled(d: &Rc<Detail>, name: &str, enabled: bool) {
    if let Some(action) = d.actions.lookup_action(name).and_then(|a| a.downcast::<gio::SimpleAction>().ok()) {
        action.set_enabled(enabled);
    }
}

fn set_loading(d: &Rc<Detail>, loading: bool) {
    d.loading.set(loading);
    d.spinner.set_spinning(loading);
    d.refresh_button.set_sensitive(!loading && !is_busy(d));
}

fn show_error(d: &Rc<Detail>, message: &str) {
    if d.root.root().is_some() {
        kit::alert(&d.root, "Error", message);
    }
}

fn set_archives(d: &Rc<Detail>, mut archives: Vec<Archive>) {
    archives.sort_by(|a, b| b.start.cmp(&a.start));
    let keep = selected(d).map(|a| a.name);
    d.store.remove_all();
    for a in &archives {
        d.store.append(&kit::boxed(a.clone()));
    }
    let position = keep.and_then(|name| archives.iter().position(|a| a.name == name));
    d.selection.set_selected(position.map(|p| p as u32).unwrap_or(gtk::INVALID_LIST_POSITION));
}

fn refresh(d: &Rc<Detail>) {
    // Borg holds an exclusive lock for the duration of `borg create`, and
    // `borg list` errors when it can't grab it. Skip silently while a
    // scheduled backup is in flight — the busy→idle transition triggers a
    // follow-up refresh once the lock is released.
    if is_busy(d) || d.loading.get() {
        return;
    }
    set_loading(d, true);
    let repo = d.repo.clone();
    let weak = Rc::downgrade(d);
    kit::bg(
        move || borg::list_archives(&repo),
        move |result| {
            let Some(d) = weak.upgrade() else { return };
            set_loading(&d, false);
            match result {
                Ok(archives) => set_archives(&d, archives),
                Err(e) => show_error(&d, &e),
            }
        },
    );
}

fn restore(d: &Rc<Detail>) {
    let Some(archive) = selected(d) else { return };
    let weak = Rc::downgrade(d);
    kit::choose_folder(&d.root, "Pick the directory to restore the archive into", "Restore here", move |dir| {
        let Some(d) = weak.upgrade() else { return };
        let repo = d.repo.clone();
        set_loading(&d, true);
        let weak = Rc::downgrade(&d);
        kit::bg(
            move || borg::extract(&repo, &archive.name, &dir),
            move |result| {
                let Some(d) = weak.upgrade() else { return };
                set_loading(&d, false);
                if let Err(e) = result {
                    show_error(&d, &e);
                }
            },
        );
    });
}

fn mount(d: &Rc<Detail>) {
    let Some(archive) = selected(d) else { return };
    let weak = Rc::downgrade(d);
    kit::choose_folder(
        &d.root,
        "Pick an empty directory to mount the archive (requires FUSE)",
        "Mount here",
        move |dir| {
            let Some(d) = weak.upgrade() else { return };
            let repo = d.repo.clone();
            set_loading(&d, true);
            let weak = Rc::downgrade(&d);
            let target = dir.clone();
            kit::bg(
                move || borg::mount(&repo, &archive.name, &dir),
                move |result| {
                    let Some(d) = weak.upgrade() else { return };
                    set_loading(&d, false);
                    match result {
                        Ok(()) => kit::open_path(&target),
                        Err(e) => show_error(&d, &e),
                    }
                },
            );
        },
    );
}

fn confirm_delete(d: &Rc<Detail>) {
    let Some(archive) = selected(d) else { return };
    let weak = Rc::downgrade(d);
    kit::confirm(
        &d.root,
        &format!("Delete archive {}?", archive.name),
        "This removes the archive from the repository immediately. The data it referenced stays on disk until you run Prune (which compacts) or a manual compact — space is not reclaimed until then.",
        "Delete archive",
        "Cancel",
        true,
        move || {
            let Some(d) = weak.upgrade() else { return };
            let (repo, name) = (d.repo.clone(), archive.name.clone());
            set_loading(&d, true);
            let weak = Rc::downgrade(&d);
            let removed = archive.name.clone();
            kit::bg(
                move || borg::delete_archive(&repo, &name),
                move |result| {
                    let Some(d) = weak.upgrade() else { return };
                    set_loading(&d, false);
                    match result {
                        Ok(()) => {
                            // Optimistic local removal so the row disappears
                            // before the full refresh completes.
                            let position = (0..d.store.n_items()).find(|i| {
                                d.store.item(*i).and_then(|o| kit::unbox::<Archive>(&o)).map(|a| a.name == removed)
                                    == Some(true)
                            });
                            if let Some(position) = position {
                                d.store.remove(position);
                            }
                            refresh(&d);
                        }
                        Err(e) => show_error(&d, &e),
                    }
                },
            );
        },
    );
}

fn confirm_stop(d: &Rc<Detail>) {
    let Some(sched) = d.busy_schedule.borrow().clone() else { return };
    let weak = Rc::downgrade(d);
    kit::confirm(
        &d.root,
        &format!("Stop backup for {}?", sched.display_name()),
        "Sends SIGTERM to borg. It writes a checkpoint and exits cleanly, so the repo stays consistent. The partial archive is discarded but the chunks already uploaded stay in the cache — the next run will reuse them.",
        "Stop backup",
        "Keep running",
        true,
        move || {
            let Some(d) = weak.upgrade() else { return };
            schedule::cancel(&d.repo.id, &sched.id);
            refresh_statuses_soon(&d.app);
        },
    );
}

fn confirm_break_lock(d: &Rc<Detail>) {
    let weak = Rc::downgrade(d);
    kit::confirm(
        &d.root,
        "Break the repository lock?",
        "Runs `borg break-lock` to release a stuck lock file — typically left behind when a previous operation was killed mid-run. Only do this if you're sure no other process (another computer, a cron job, a running backup) is currently writing to this repo. Breaking the lock while someone else is writing can corrupt the repository.",
        "Break lock",
        "Cancel",
        true,
        move || {
            let Some(d) = weak.upgrade() else { return };
            d.breaking_lock.set(true);
            update_busy(&d);
            let repo = d.repo.clone();
            let weak = Rc::downgrade(&d);
            kit::bg(
                move || borg::break_lock(&repo),
                move |result| {
                    let Some(d) = weak.upgrade() else { return };
                    d.breaking_lock.set(false);
                    update_busy(&d);
                    match result {
                        // Pull the archive list right after so the user can
                        // see whether the repo is back to normal.
                        Ok(()) => refresh(&d),
                        Err(e) => show_error(&d, &e),
                    }
                },
            );
        },
    );
}
