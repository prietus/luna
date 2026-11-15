//! Schedules of a repository (BorgMac's `ScheduleSheet`: the list, entry
//! point from the repository detail view) and the editor for a single
//! schedule (`ScheduleEditorSheet`). Schedules are systemd user timers here.
use super::kit::{self, icons};
use super::state::{AppState, refresh_statuses, refresh_statuses_soon};
use crate::core::models::{BackupRunStatus, BackupSchedule, Frequency, Repository};
use crate::core::{format, schedule, status};
use adw::prelude::*;
use chrono::{Local, Utc};
use gtk::{gdk, gio, glib};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use uuid::Uuid;

const ICON_CLOCK: &str = "preferences-system-time-symbolic"; // clock
const ICON_SUN: &str = "weather-clear-symbolic"; // sun.max
const ICON_CALENDAR: &str = "x-office-calendar-symbolic"; // calendar
const ICON_LOG: &str = "text-x-generic-symbolic"; // doc.text

// ============================================================ list sheet

struct Row {
    schedule: BackupSchedule,
    installed: bool,
    progress: gtk::ProgressBar,
    status_line: gtk::Label,
    toggle: gtk::Switch,
    spinner: gtk::Spinner,
    stop: gtk::Button,
    run: gtk::Button,
    /// True while the bar is indeterminate (no previous total to divide by).
    pulsing: Cell<bool>,
}

struct ListSheet {
    app: Rc<AppState>,
    repo: Repository,
    parent: glib::WeakRef<gtk::Widget>,
    dialog: adw::Dialog,
    stack: gtk::Stack,
    list: gtk::ListBox,
    rows: RefCell<Vec<Rc<Row>>>,
    /// What `rows` was built from; rows are rebuilt only when this changes so
    /// status ticks don't reset spinners and hover state.
    built: RefCell<Option<Vec<(BackupSchedule, bool)>>>,
}

impl ListSheet {
    /// The schedules live on the repository in the store, not on local
    /// state — we always resolve the latest snapshot by repo id so edits
    /// from the editor sheet become visible immediately.
    fn schedules(&self) -> Vec<BackupSchedule> {
        self.app.repos.get(&self.repo.id).map(|r| r.schedules).unwrap_or_default()
    }

    fn row(&self, schedule_id: &Uuid) -> Option<Rc<Row>> {
        self.rows.borrow().iter().find(|r| &r.schedule.id == schedule_id).cloned()
    }
}

pub fn present(parent: &impl IsA<gtk::Widget>, app: &Rc<AppState>, repo: &Repository) {
    present_list(parent, app, repo);
}

fn present_list(parent: &impl IsA<gtk::Widget>, app: &Rc<AppState>, repo: &Repository) -> Rc<ListSheet> {
    let (dialog, content) = kit::sheet("", 540, 480);
    dialog.set_title("Schedules");

    let header = kit::hbox(10);
    let clock = kit::icon_styled(icons::HISTORY, &["tint", "sched-header-icon"]);
    clock.set_valign(gtk::Align::Center);
    header.append(&clock);
    let titles = kit::vbox(2);
    titles.append(&kit::label("Schedules", &["sched-title3"]));
    titles.append(&kit::ellipsized(&repo.name, false, &["caption", "secondary"]));
    header.append(&titles);
    let add = gtk::Button::new();
    add.set_child(Some(&adw::ButtonContent::builder().icon_name(icons::PLUS).label("Add").build()));
    add.add_css_class("mac");
    add.add_css_class("sched-add");
    add.set_tooltip_text(Some("Add a new schedule"));
    add.set_valign(gtk::Align::Center);
    header.append(&add);
    content.append(&header);
    content.append(&kit::divider());

    let empty = kit::vbox(8);
    empty.set_valign(gtk::Align::Center);
    empty.set_vexpand(true);
    empty.append(&kit::icon_styled(icons::CALENDAR_CLOCK, &["secondary", "sched-empty-icon"]));
    let headline = kit::label("No schedules configured", &["heading", "secondary"]);
    headline.set_xalign(0.5);
    empty.append(&headline);
    let hint = kit::caption("Create one to back up a set of paths on its own frequency.");
    hint.set_xalign(0.5);
    hint.set_justify(gtk::Justification::Center);
    empty.append(&hint);

    let list = gtk::ListBox::new();
    list.add_css_class("mac-list");
    list.add_css_class("sched-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    list.set_valign(gtk::Align::Start);

    let stack = gtk::Stack::new();
    stack.set_vexpand(true);
    stack.add_named(&empty, Some("empty"));
    stack.add_named(&kit::scrolled(&list), Some("list"));
    content.append(&stack);

    let close = kit::button("Close");
    content.append(&kit::button_row(None, &[&close]));

    let view = Rc::new(ListSheet {
        app: app.clone(),
        repo: repo.clone(),
        parent: parent.upcast_ref::<gtk::Widget>().downgrade(),
        dialog: dialog.clone(),
        stack,
        list: list.clone(),
        rows: RefCell::new(vec![]),
        built: RefCell::new(None),
    });

    close.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| {
            dialog.close();
        }
    ));
    add.connect_clicked(glib::clone!(
        #[weak]
        view,
        move |_| {
            edit(&view, None);
        }
    ));
    list.connect_row_activated(glib::clone!(
        #[weak]
        view,
        move |_, row| {
            let schedule = view.rows.borrow().get(row.index() as usize).map(|r| r.schedule.clone());
            if let Some(schedule) = schedule {
                edit(&view, Some(schedule));
            }
        }
    ));

    let click = gtk::GestureClick::new();
    click.set_button(gdk::BUTTON_SECONDARY);
    click.connect_pressed(glib::clone!(
        #[weak]
        view,
        move |gesture, _, x, y| {
            let Some(row) = view.list.row_at_y(y as i32) else { return };
            let Some(row) = view.rows.borrow().get(row.index() as usize).cloned() else { return };
            gesture.set_state(gtk::EventSequenceState::Claimed);
            show_context_menu(&view, &row, x, y);
        }
    ));
    list.add_controller(click);

    // Live updates: schedule edits come through the repository store, run
    // state and progress through the status store.
    let on_change = glib::clone!(
        #[weak]
        view,
        move || sync(&view)
    );
    let repos_handler = app.repos.changed.connect(on_change.clone());
    let statuses_handler = app.statuses.changed.connect(on_change);

    // Animates indeterminate bars and keeps the relative "last … · next …"
    // texts fresh. Stops by itself once the sheet is gone.
    let ticks = Cell::new(0u32);
    glib::timeout_add_local(
        std::time::Duration::from_millis(150),
        glib::clone!(
            #[weak]
            view,
            #[upgrade_or]
            glib::ControlFlow::Break,
            move || {
                for row in view.rows.borrow().iter().filter(|r| r.pulsing.get()) {
                    row.progress.pulse();
                }
                ticks.set(ticks.get() + 1);
                if ticks.get() % 100 == 0 {
                    update_rows(&view);
                }
                glib::ControlFlow::Continue
            }
        ),
    );

    // The dialog owns the controller until it closes.
    let keep = RefCell::new(Some(view.clone()));
    dialog.connect_closed(move |_| {
        if let Some(view) = keep.borrow_mut().take() {
            view.app.repos.changed.disconnect(repos_handler);
            view.app.statuses.changed.disconnect(statuses_handler);
        }
    });

    sync(&view);
    dialog.present(Some(parent));
    // Pick up a run that started while the app was polling at idle cadence.
    refresh_statuses(app);
    view
}

/// Rebuilds the rows when the schedules (or their installed state) changed,
/// then refreshes every row's run state.
fn sync(view: &Rc<ListSheet>) {
    let key: Vec<(BackupSchedule, bool)> = view
        .schedules()
        .into_iter()
        .map(|s| {
            let installed = schedule::is_installed(&view.repo.id, &s.id);
            (s, installed)
        })
        .collect();
    if view.built.borrow().as_ref() != Some(&key) {
        view.list.remove_all();
        let rows: Vec<Rc<Row>> = key
            .iter()
            .map(|(schedule, installed)| {
                let (widget, row) = build_row(view, schedule, *installed);
                view.list.append(&widget);
                row
            })
            .collect();
        *view.rows.borrow_mut() = rows;
        view.stack.set_visible_child_name(if key.is_empty() { "empty" } else { "list" });
        *view.built.borrow_mut() = Some(key);
    }
    update_rows(view);
}

fn update_rows(view: &Rc<ListSheet>) {
    for row in view.rows.borrow().iter() {
        update_row(view, row);
    }
}

fn frequency_icon(schedule: &BackupSchedule) -> &'static str {
    match schedule.frequency {
        Frequency::Hourly => ICON_CLOCK,
        Frequency::Daily => ICON_SUN,
        Frequency::Weekly => ICON_CALENDAR,
    }
}

fn summary_line(schedule: &BackupSchedule) -> String {
    let freq = match schedule.frequency {
        Frequency::Hourly => "every hour".to_string(),
        Frequency::Daily => format!("every day at {:02}:{:02}", schedule.hour, schedule.minute),
        Frequency::Weekly => {
            const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
            let day = DAYS[schedule.weekday.clamp(0, 6) as usize];
            format!("every {day} at {:02}:{:02}", schedule.hour, schedule.minute)
        }
    };
    let count = schedule.paths.len();
    let paths = if count == 1 { "1 path".to_string() } else { format!("{count} paths") };
    format!("{freq} · {paths}")
}

fn build_row(view: &Rc<ListSheet>, schedule: &BackupSchedule, installed: bool) -> (gtk::ListBoxRow, Rc<Row>) {
    let line = kit::hbox(12);
    line.set_margin_top(4);
    line.set_margin_bottom(4);

    // Frequency icon with the delete badge on its top-trailing corner.
    let badge_host = gtk::Overlay::new();
    badge_host.set_size_request(32, 28);
    badge_host.set_valign(gtk::Align::Center);
    let icon = kit::icon_styled(frequency_icon(schedule), &["tint", "sched-row-icon"]);
    icon.set_halign(gtk::Align::Center);
    icon.set_valign(gtk::Align::Center);
    badge_host.set_child(Some(&icon));
    let delete = gtk::Button::from_icon_name(icons::XMARK_CIRCLE);
    delete.add_css_class("plain");
    delete.add_css_class("sched-delete-badge");
    delete.set_tooltip_text(Some("Delete schedule"));
    delete.set_halign(gtk::Align::End);
    delete.set_valign(gtk::Align::Start);
    badge_host.add_overlay(&delete);
    line.append(&badge_host);

    let texts = kit::vbox(2);
    texts.set_hexpand(true);
    texts.set_valign(gtk::Align::Center);
    texts.append(&kit::ellipsized(&schedule.display_name(), false, &["sched-row-name"]));
    texts.append(&kit::ellipsized(&summary_line(schedule), true, &["caption", "secondary"]));
    let progress = gtk::ProgressBar::new();
    progress.add_css_class("orange");
    progress.add_css_class("sched-progress");
    progress.set_pulse_step(0.12);
    progress.set_margin_top(2);
    progress.set_visible(false);
    texts.append(&progress);
    let status_line = kit::ellipsized("", false, &["caption2"]);
    status_line.set_visible(false);
    texts.append(&status_line);
    line.append(&texts);

    let toggle = gtk::Switch::new();
    toggle.add_css_class("sched-switch");
    toggle.set_valign(gtk::Align::Center);
    toggle.set_active(installed);
    line.append(&toggle);

    let spinner = kit::spinner();
    spinner.set_valign(gtk::Align::Center);
    line.append(&spinner);
    let stop = gtk::Button::from_icon_name(icons::XMARK_CIRCLE);
    stop.add_css_class("plain");
    stop.add_css_class("sched-stop");
    stop.set_tooltip_text(Some("Stop backup (SIGTERM to borg)"));
    stop.set_valign(gtk::Align::Center);
    line.append(&stop);
    let run = gtk::Button::from_icon_name(icons::PLAY_CIRCLE);
    run.add_css_class("plain");
    run.add_css_class("sched-run");
    run.set_tooltip_text(Some("Run now"));
    run.set_valign(gtk::Align::Center);
    line.append(&run);

    let row = Rc::new(Row {
        schedule: schedule.clone(),
        installed,
        progress,
        status_line,
        toggle: toggle.clone(),
        spinner,
        stop: stop.clone(),
        run: run.clone(),
        pulsing: Cell::new(false),
    });

    delete.connect_clicked(glib::clone!(
        #[weak]
        view,
        #[strong]
        schedule,
        move |_| confirm_delete(&view, &schedule)
    ));
    run.connect_clicked(glib::clone!(
        #[weak]
        view,
        #[strong]
        schedule,
        move |_| run_now(&view, &schedule)
    ));
    stop.connect_clicked(glib::clone!(
        #[weak]
        view,
        #[strong]
        schedule,
        move |_| confirm_stop(&view, &schedule)
    ));
    toggle.connect_state_set(glib::clone!(
        #[weak]
        view,
        #[strong]
        schedule,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_, enabled| {
            set_enabled(&view, &schedule, enabled);
            glib::Propagation::Proceed
        }
    ));

    let widget = gtk::ListBoxRow::new();
    widget.set_child(Some(&line));
    (widget, row)
}

/// "4.2 GB · 12,345 files · 42% · ~3m 10s left · /foo" beneath the progress
/// bar. Each piece is optional so early ticks (borg hasn't reported numbers
/// yet) degrade gracefully.
fn progress_line(status: &BackupRunStatus) -> String {
    let mut parts: Vec<String> = vec![];
    if let Some(bytes) = status.progress_original_bytes.filter(|b| *b > 0) {
        parts.push(format::bytes_binary(bytes));
    }
    if let Some(n) = status.progress_file_count.filter(|n| *n > 0) {
        parts.push(format!("{n} {}", if n == 1 { "file" } else { "files" }));
    }
    if let Some(ratio) = status.progress_ratio() {
        parts.push(format!("{:.0}%", ratio * 100.0));
        if let Some(left) = eta(status, ratio) {
            parts.push(left);
        }
    }
    if let (Some(attempt), Some(max)) = (status.attempt, status.max_attempts) {
        if attempt > 1 {
            parts.push(format!("retry {attempt}/{max}"));
        }
    }
    if let Some(path) = status.progress_current_path.as_ref().filter(|p| !p.is_empty()) {
        parts.push(tail_of(path, 34));
    }
    if parts.is_empty() { "starting…".to_string() } else { parts.join(" · ") }
}

/// Keeps the end of a long path (the file name is the interesting part) so
/// the label's own truncation never has to eat the numbers before it.
fn tail_of(path: &str, max_chars: usize) -> String {
    let count = path.chars().count();
    if count <= max_chars {
        return path.to_string();
    }
    let tail: String = path.chars().skip(count - max_chars + 1).collect();
    format!("…{tail}")
}

/// Linear extrapolation from the time this attempt has been running; only
/// shown once there is enough signal for it not to jump around wildly.
fn eta(status: &BackupRunStatus, ratio: f64) -> Option<String> {
    let started = status.progress_started_at?;
    let elapsed = Utc::now().signed_duration_since(started).num_seconds();
    if elapsed < 5 || ratio < 0.02 || ratio >= 1.0 {
        return None;
    }
    let remaining = (elapsed as f64 * (1.0 - ratio) / ratio).round() as i64;
    Some(format!("~{} left", format::duration(remaining)))
}

fn schedule_line(schedule: &BackupSchedule, status: Option<&BackupRunStatus>) -> String {
    let mut parts: Vec<String> = vec![];
    if let Some(last) = status.and_then(|s| s.last_run) {
        parts.push(format!("last {}", format::relative_short(&last)));
    }
    if let Some(next) = schedule::next_fire_date(schedule, Local::now()) {
        parts.push(format!("next {}", format::relative_short(&next)));
    }
    parts.join(" · ")
}

fn update_row(view: &Rc<ListSheet>, row: &Rc<Row>) {
    let status = view.app.statuses.status(&row.schedule.id);
    let running = status.as_ref().map(|s| s.running).unwrap_or(false);

    row.spinner.set_visible(running);
    row.stop.set_visible(running);
    row.run.set_visible(!running);
    row.run.set_sensitive(row.installed);
    // Disabling removes the units, which would also stop the run in flight;
    // that goes through the explicit "Stop backup" confirmation instead.
    row.toggle.set_sensitive(!running);
    row.toggle.set_tooltip_text(Some(if row.installed {
        "Enabled — the systemd timer is installed"
    } else {
        "Disabled — the systemd timer is not installed"
    }));

    row.progress.set_visible(running);
    match status.as_ref().filter(|_| running).and_then(|s| s.progress_ratio()) {
        Some(ratio) => {
            row.pulsing.set(false);
            row.progress.set_fraction(ratio);
        }
        None => row.pulsing.set(running),
    }

    let label = &row.status_line;
    for class in ["tertiary", "error-text", "c-orange"] {
        label.remove_css_class(class);
    }
    let error = status.as_ref().and_then(|s| s.last_error.clone()).filter(|e| !e.is_empty());
    let (text, class, tooltip) = match (&status, running) {
        (Some(st), true) => (progress_line(st), "tertiary", None),
        _ if !row.installed => ("disabled — not scheduled".to_string(), "c-orange", None),
        _ => match error {
            Some(e) => (e.lines().next().unwrap_or("").to_string(), "error-text", Some(e)),
            None => (schedule_line(&row.schedule, status.as_ref()), "tertiary", None),
        },
    };
    label.set_text(&text);
    label.add_css_class(class);
    label.set_tooltip_text(tooltip.as_deref());
    label.set_visible(!text.is_empty());
}

// ------------------------------------------------------------ list actions

fn parent_of(view: &Rc<ListSheet>) -> gtk::Widget {
    view.parent.upgrade().unwrap_or_else(|| view.dialog.clone().upcast())
}

fn edit(view: &Rc<ListSheet>, existing: Option<BackupSchedule>) -> Rc<Editor> {
    present_editor(&parent_of(view), &view.app, view.repo.id, existing)
}

fn show_context_menu(view: &Rc<ListSheet>, row: &Rc<Row>, x: f64, y: f64) {
    let running = view.app.statuses.status(&row.schedule.id).map(|s| s.running).unwrap_or(false);
    let menu = gio::Menu::new();
    let main = gio::Menu::new();
    if running {
        main.append(Some("Stop backup"), Some("sched.stop"));
    } else if row.installed {
        main.append(Some("Run now"), Some("sched.run"));
    }
    main.append(Some("Edit"), Some("sched.edit"));
    main.append(Some("Show log"), Some("sched.log"));
    if !running {
        main.append(Some(if row.installed { "Disable" } else { "Enable" }), Some("sched.toggle"));
    }
    menu.append_section(None, &main);
    let danger = gio::Menu::new();
    danger.append(Some("Delete"), Some("sched.delete"));
    menu.append_section(None, &danger);

    let group = gio::SimpleActionGroup::new();
    let add_action = |name: &str, f: fn(&Rc<ListSheet>, &Rc<Row>)| {
        let action = gio::SimpleAction::new(name, None);
        action.connect_activate(glib::clone!(
            #[weak]
            view,
            #[strong]
            row,
            move |_, _| f(&view, &row)
        ));
        group.add_action(&action);
    };
    add_action("run", |v, r| run_now(v, &r.schedule));
    add_action("stop", |v, r| confirm_stop(v, &r.schedule));
    add_action("edit", |v, r| {
        edit(v, Some(r.schedule.clone()));
    });
    add_action("log", |v, r| show_log(v, &r.schedule));
    add_action("toggle", |v, r| set_enabled(v, &r.schedule, !r.installed));
    add_action("delete", |v, r| confirm_delete(v, &r.schedule));
    view.list.insert_action_group("sched", Some(&group));

    let popover = gtk::PopoverMenu::from_model(Some(&menu));
    popover.set_parent(&view.list);
    popover.set_has_arrow(false);
    popover.set_halign(gtk::Align::Start);
    popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
    popover.connect_closed(|popover| {
        // Unparent after the chosen action has been dispatched.
        let popover = popover.clone();
        glib::idle_add_local_once(move || popover.unparent());
    });
    popover.popup();
}

fn run_now(view: &Rc<ListSheet>, schedule: &BackupSchedule) {
    let (repo_id, schedule_id) = (view.repo.id, schedule.id);
    let weak = Rc::downgrade(view);
    kit::bg(
        move || schedule::kickstart(&repo_id, &schedule_id),
        move |result| {
            let Some(view) = weak.upgrade() else { return };
            match result {
                // The runner will write status to disk shortly; nudge the
                // store so the spinner shows up without waiting for the 60 s
                // poll tick.
                Ok(()) => refresh_statuses_soon(&view.app),
                // systemctl errors typically mean the unit isn't loaded —
                // the schedule list shouldn't be in this state.
                Err(e) => {
                    eprintln!("kickstart failed: {e}");
                    kit::alert(&parent_of(&view), "Could not start the backup", &e);
                }
            }
        },
    );
}

fn confirm_stop(view: &Rc<ListSheet>, schedule: &BackupSchedule) {
    let weak = Rc::downgrade(view);
    let (repo_id, schedule_id) = (view.repo.id, schedule.id);
    kit::confirm(
        &parent_of(view),
        &format!("Stop backup for {}?", schedule.display_name()),
        "Sends SIGTERM to borg. It writes a checkpoint and exits cleanly, so the repo stays consistent. The partial archive is discarded but the chunks already uploaded stay in the cache — the next run will reuse them.",
        "Stop backup",
        "Keep running",
        true,
        move || stop(&weak, repo_id, schedule_id),
    );
}

fn stop(view: &std::rc::Weak<ListSheet>, repo_id: Uuid, schedule_id: Uuid) {
    let weak = view.clone();
    kit::bg(
        move || schedule::cancel(&repo_id, &schedule_id),
        move |_| {
            if let Some(view) = weak.upgrade() {
                refresh_statuses_soon(&view.app);
            }
        },
    );
}

fn show_log(view: &Rc<ListSheet>, schedule: &BackupSchedule) {
    let log = schedule::log_file(&view.repo.id, &schedule.id);
    if log.exists() {
        kit::open_path(&log);
    } else {
        kit::alert(
            &parent_of(view),
            "No log yet",
            &format!("This schedule has not run yet. Its output will be written to {}.", log.display()),
        );
    }
}

/// Installs or removes the systemd units of a schedule without touching the
/// schedule itself, so it can be paused and resumed.
fn set_enabled(view: &Rc<ListSheet>, schedule: &BackupSchedule, enabled: bool) {
    let repo_id = view.repo.id;
    let schedule = schedule.clone();
    let weak = Rc::downgrade(view);
    kit::bg(
        move || {
            if enabled {
                schedule::install(&repo_id, &schedule)
            } else {
                schedule::uninstall(&repo_id, &schedule.id);
                Ok(())
            }
        },
        move |result| {
            let Some(view) = weak.upgrade() else { return };
            if let Err(e) = result {
                kit::alert(&parent_of(&view), "Could not enable the schedule", &e);
            }
            // Force a rebuild so the switch reflects what is really on disk.
            *view.built.borrow_mut() = None;
            sync(&view);
        },
    );
}

fn confirm_delete(view: &Rc<ListSheet>, schedule: &BackupSchedule) {
    let weak = Rc::downgrade(view);
    let schedule = schedule.clone();
    kit::confirm(
        &parent_of(view),
        &format!("Delete schedule \"{}\"?", schedule.display_name()),
        "This unschedules the backup and removes its run history. The repository and existing archives are untouched.",
        "Delete schedule",
        "Cancel",
        true,
        move || {
            if let Some(view) = weak.upgrade() {
                delete(&view, &schedule);
            }
        },
    );
}

fn delete(view: &Rc<ListSheet>, schedule: &BackupSchedule) {
    let (repo_id, schedule_id) = (view.repo.id, schedule.id);
    let app = view.app.clone();
    let fallback = view.repo.clone();
    kit::bg(
        move || schedule::uninstall(&repo_id, &schedule_id),
        move |_| {
            let mut repo = app.repos.get(&repo_id).unwrap_or(fallback);
            repo.schedules.retain(|s| s.id != schedule_id);
            status::delete(&schedule_id);
            app.repos.update(repo);
            refresh_statuses(&app);
        },
    );
}

// ========================================================== shared widgets

/// "Add…" button for sources and excludes. `NSOpenPanel` can pick files and
/// folders in one go; GTK's file dialog cannot, so the button opens a small
/// menu to choose which kind to pick.
pub(super) fn path_picker_button(
    label: &str,
    icon_name: Option<&str>,
    title: &'static str,
    on_chosen: impl Fn(Vec<String>) + 'static,
) -> gtk::MenuButton {
    let button = gtk::MenuButton::new();
    // A custom child (rather than `set_label`) keeps the drop-down arrow away.
    let face = kit::hbox(5);
    if let Some(icon_name) = icon_name {
        face.append(&kit::icon(icon_name));
    }
    face.append(&kit::label(label, &[]));
    button.set_child(Some(&face));
    button.add_css_class("mac");
    button.set_halign(gtk::Align::Start);

    let popover = gtk::Popover::new();
    popover.add_css_class("menu");
    let choices = kit::vbox(0);
    let on_chosen: Rc<dyn Fn(Vec<String>)> = Rc::new(on_chosen);
    for (text, folders) in [("Folders…", true), ("Files…", false)] {
        let choice = gtk::Button::with_label(text);
        choice.add_css_class("flat");
        if let Some(l) = choice.child().and_downcast::<gtk::Label>() {
            l.set_xalign(0.0);
        }
        choice.connect_clicked(glib::clone!(
            #[weak]
            button,
            #[weak]
            popover,
            #[strong]
            on_chosen,
            move |_| {
                popover.popdown();
                let on_chosen = on_chosen.clone();
                let deliver = move |picked: Vec<std::path::PathBuf>| {
                    on_chosen(picked.iter().map(|p| p.to_string_lossy().into_owned()).collect())
                };
                if folders {
                    kit::choose_folders(&button, title, deliver);
                } else {
                    kit::choose_files(&button, title, deliver);
                }
            }
        ));
        choices.append(&choice);
    }
    popover.set_child(Some(&choices));
    button.set_popover(Some(&popover));
    button
}

/// One removable line of a path / exclude list: icon, text truncated in the
/// middle, and a remove button.
pub(super) fn item_line(
    icon_name: &str,
    text: &str,
    text_classes: &[&str],
    remove_icon: &str,
    remove_classes: &[&str],
    on_remove: impl Fn() + 'static,
) -> gtk::Box {
    let line = kit::hbox(8);
    line.append(&kit::icon_styled(icon_name, &["secondary"]));
    let label = kit::ellipsized(text, true, text_classes);
    label.set_tooltip_text(Some(text));
    line.append(&label);
    let remove = gtk::Button::from_icon_name(remove_icon);
    remove.add_css_class("plain");
    for class in remove_classes {
        remove.add_css_class(class);
    }
    remove.set_tooltip_text(Some("Remove"));
    remove.connect_clicked(move |_| on_remove());
    line.append(&remove);
    line
}

/// Appends the items that are not in the list yet.
pub(super) fn append_unique(list: &RefCell<Vec<String>>, items: impl IntoIterator<Item = String>) {
    let mut list = list.borrow_mut();
    for item in items {
        if !list.contains(&item) {
            list.push(item);
        }
    }
}

// ============================================================ editor sheet

struct Editor {
    app: Rc<AppState>,
    repo_id: Uuid,
    existing: Option<BackupSchedule>,
    /// Id used for a new schedule; fixed for the lifetime of the sheet so a
    /// retried save doesn't leave a second set of units behind.
    new_id: Uuid,
    dialog: adw::Dialog,
    name: gtk::Entry,
    frequency: Cell<Frequency>,
    day_row: gtk::Box,
    day: gtk::DropDown,
    time_row: gtk::Box,
    hour: gtk::SpinButton,
    minute: gtk::SpinButton,
    paths: RefCell<Vec<String>>,
    paths_box: gtk::Box,
    excludes: RefCell<Vec<String>>,
    excludes_box: gtk::Box,
    pattern: gtk::Entry,
    keep_daily: gtk::SpinButton,
    keep_weekly: gtk::SpinButton,
    keep_monthly: gtk::SpinButton,
    error: gtk::Label,
    save: gtk::Button,
}

/// SwiftUI `GroupBox(title)`: a caption above a rounded block.
fn group_box(title: &str) -> (gtk::Box, gtk::Box) {
    let outer = kit::vbox(5);
    outer.append(&kit::label(title, &["sched-group-title"]));
    let inner = kit::vbox(6);
    let card = kit::card(&inner);
    card.add_css_class("sched-group");
    outer.append(&card);
    (outer, inner)
}

/// Label on the left, control on the right (a `Picker`/`DatePicker` with a
/// visible title outside a `Form`).
fn labelled(title: &str, control: &impl IsA<gtk::Widget>) -> gtk::Box {
    let row = kit::hbox(10);
    let label = kit::label(title, &[]);
    label.set_width_request(76);
    row.append(&label);
    row.append(control);
    row
}

fn two_digit_spin(max: f64, value: i64) -> gtk::SpinButton {
    let spin = gtk::SpinButton::with_range(0.0, max, 1.0);
    spin.set_value(value as f64);
    spin.set_wrap(true);
    spin.set_numeric(true);
    spin.set_width_chars(2);
    spin.set_orientation(gtk::Orientation::Horizontal);
    spin.add_css_class("sched-spin");
    spin.connect_output(|spin| {
        spin.set_text(&format!("{:02}", spin.value_as_int()));
        glib::Propagation::Stop
    });
    spin
}

fn retention_stepper(title: &str, value: i64, max: f64) -> (gtk::Box, gtk::SpinButton) {
    let column = kit::vbox(2);
    column.append(&kit::label(title, &["caption", "secondary"]));
    let spin = gtk::SpinButton::with_range(0.0, max, 1.0);
    spin.set_value(value as f64);
    spin.set_numeric(true);
    spin.set_width_chars(3);
    spin.add_css_class("sched-spin");
    column.append(&spin);
    (column, spin)
}

fn present_editor(
    parent: &impl IsA<gtk::Widget>,
    app: &Rc<AppState>,
    repo_id: Uuid,
    existing: Option<BackupSchedule>,
) -> Rc<Editor> {
    let initial = existing.clone().unwrap_or_default();
    let title = if existing.is_none() { "New schedule" } else { "Edit schedule" };
    let (dialog, content) = kit::sheet("", 540, 620);
    dialog.set_title(title);
    content.append(&kit::label(title, &["sched-title3"]));

    let form = kit::vbox(12);
    let scroller = kit::scrolled(&form);
    content.append(&scroller);

    // Name
    let name = gtk::Entry::new();
    name.set_text(&initial.name);
    name.set_placeholder_text(Some("e.g. Photos daily"));
    name.set_tooltip_text(Some("Name (optional)"));
    name.set_activates_default(true);
    name.add_css_class("sched-field");
    form.append(&name);

    // When
    let (when_box, when) = group_box("When");
    when.set_spacing(10);
    let segments = kit::hbox(0);
    segments.add_css_class("linked");
    segments.add_css_class("sched-segmented");
    segments.set_hexpand(true);
    segments.set_homogeneous(true);
    let mut segment_buttons: Vec<(Frequency, gtk::ToggleButton)> = vec![];
    for frequency in Frequency::ALL {
        let button = gtk::ToggleButton::with_label(frequency.label());
        button.set_active(frequency == initial.frequency);
        if let Some((_, first)) = segment_buttons.first() {
            button.set_group(Some(first));
        }
        segments.append(&button);
        segment_buttons.push((frequency, button));
    }
    when.append(&labelled("Frequency", &segments));
    let day = gtk::DropDown::from_strings(&["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"]);
    day.set_selected(initial.weekday.clamp(0, 6) as u32);
    day.add_css_class("sched-popup");
    let day_row = labelled("Day", &day);
    when.append(&day_row);
    let hour = two_digit_spin(23.0, initial.hour.clamp(0, 23));
    let minute = two_digit_spin(59.0, initial.minute.clamp(0, 59));
    let time = kit::hbox(4);
    time.append(&hour);
    time.append(&kit::label(":", &[]));
    time.append(&minute);
    let time_row = labelled("Time", &time);
    when.append(&time_row);
    form.append(&when_box);

    // Paths
    let (paths_group, paths_inner) = group_box("Paths to back up");
    let paths_box = kit::vbox(6);
    paths_inner.append(&paths_box);
    form.append(&paths_group);

    // Excludes
    let (excludes_group, excludes_inner) = group_box("Exclude");
    let excludes_box = kit::vbox(6);
    excludes_inner.append(&excludes_box);
    let pattern_row = kit::hbox(8);
    let pattern = gtk::Entry::new();
    pattern.set_placeholder_text(Some("Pattern (e.g. **/node_modules)"));
    pattern.set_hexpand(true);
    pattern.add_css_class("sched-field");
    pattern.add_css_class("mono-entry");
    pattern_row.append(&pattern);
    let add_pattern = kit::button("Add");
    add_pattern.add_css_class("small");
    add_pattern.set_valign(gtk::Align::Center);
    add_pattern.set_sensitive(false);
    pattern_row.append(&add_pattern);
    excludes_inner.append(&pattern_row);
    let exclude_buttons = kit::hbox(8);
    excludes_inner.append(&exclude_buttons);
    form.append(&excludes_group);

    // Retention
    let (retention_group, retention_inner) = group_box("Retention (optional)");
    let steppers = kit::hbox(16);
    let (daily_box, keep_daily) = retention_stepper("Daily", initial.keep_daily, 365.0);
    let (weekly_box, keep_weekly) = retention_stepper("Weekly", initial.keep_weekly, 52.0);
    let (monthly_box, keep_monthly) = retention_stepper("Monthly", initial.keep_monthly, 60.0);
    steppers.append(&daily_box);
    steppers.append(&weekly_box);
    steppers.append(&monthly_box);
    retention_inner.append(&steppers);
    form.append(&retention_group);

    let error = kit::error_label();
    content.append(&error);

    let cancel = kit::button("Cancel");
    let save = kit::primary_button("Save");
    content.append(&kit::button_row(None, &[&cancel, &save]));
    dialog.set_default_widget(Some(&save));

    let editor = Rc::new(Editor {
        app: app.clone(),
        repo_id,
        new_id: initial.id,
        existing,
        dialog: dialog.clone(),
        name,
        frequency: Cell::new(initial.frequency),
        day_row,
        day,
        time_row,
        hour,
        minute,
        paths: RefCell::new(initial.paths.clone()),
        paths_box,
        excludes: RefCell::new(initial.excludes.clone()),
        excludes_box,
        pattern: pattern.clone(),
        keep_daily,
        keep_weekly,
        keep_monthly,
        error,
        save: save.clone(),
    });

    for (frequency, button) in &segment_buttons {
        let frequency = *frequency;
        button.connect_toggled(glib::clone!(
            #[weak]
            editor,
            move |button| {
                if button.is_active() {
                    editor.frequency.set(frequency);
                    update_when(&editor);
                }
            }
        ));
    }

    let add_path = path_picker_button(
        "Add…",
        None,
        "Pick the files and folders to back up",
        glib::clone!(
            #[weak]
            editor,
            move |picked| {
                append_unique(&editor.paths, picked);
                rebuild_paths(&editor);
            }
        ),
    );
    add_path.add_css_class("small");
    paths_inner.append(&add_path);

    // Complements — does not replace — the free-form text field, which is
    // still the only way to add glob patterns like `**/node_modules`.
    let add_from_disk = path_picker_button(
        "Add from disk…",
        Some(icons::FOLDER),
        "Pick the files and folders you want to exclude from this backup.",
        glib::clone!(
            #[weak]
            editor,
            move |picked| {
                append_unique(&editor.excludes, picked);
                rebuild_excludes(&editor);
            }
        ),
    );
    add_from_disk.add_css_class("small");
    add_from_disk.set_tooltip_text(Some(
        "Pick files or folders from the file manager. Handy when you don't remember the exact patterns — the real names get added as absolute paths.",
    ));
    exclude_buttons.append(&add_from_disk);
    let presets = kit::button("Add common presets");
    presets.add_css_class("small");
    exclude_buttons.append(&presets);

    presets.connect_clicked(glib::clone!(
        #[weak]
        editor,
        move |_| {
            append_unique(&editor.excludes, BackupSchedule::COMMON_EXCLUDE_PRESETS.iter().map(|p| p.to_string()));
            rebuild_excludes(&editor);
        }
    ));
    pattern.connect_changed(glib::clone!(
        #[weak]
        add_pattern,
        move |pattern| add_pattern.set_sensitive(!pattern.text().trim().is_empty())
    ));
    pattern.connect_activate(glib::clone!(
        #[weak]
        editor,
        move |_| commit_new_exclude(&editor)
    ));
    add_pattern.connect_clicked(glib::clone!(
        #[weak]
        editor,
        move |_| commit_new_exclude(&editor)
    ));
    cancel.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| {
            dialog.close();
        }
    ));
    save.connect_clicked(glib::clone!(
        #[weak]
        editor,
        move |_| save_schedule(&editor)
    ));

    let keep = RefCell::new(Some(editor.clone()));
    dialog.connect_closed(move |_| {
        keep.borrow_mut().take();
    });

    update_when(&editor);
    rebuild_paths(&editor);
    rebuild_excludes(&editor);
    dialog.present(Some(parent));
    editor
}

fn update_when(editor: &Rc<Editor>) {
    let frequency = editor.frequency.get();
    editor.day_row.set_visible(frequency == Frequency::Weekly);
    editor.time_row.set_visible(frequency != Frequency::Hourly);
}

fn rebuild_items(
    editor: &Rc<Editor>,
    container: &gtk::Box,
    items: &[String],
    icon_name: &str,
    empty_text: &str,
    remove: fn(&Rc<Editor>, usize),
) {
    kit::clear_box(container);
    if items.is_empty() {
        container.append(&kit::caption(empty_text));
        return;
    }
    for (index, item) in items.iter().enumerate() {
        let weak = Rc::downgrade(editor);
        container.append(&item_line(
            icon_name,
            item,
            &["caption", "mono"],
            icons::MINUS_CIRCLE,
            &["c-red"],
            move || {
                if let Some(editor) = weak.upgrade() {
                    remove(&editor, index);
                }
            },
        ));
    }
}

fn rebuild_paths(editor: &Rc<Editor>) {
    let items = editor.paths.borrow().clone();
    rebuild_items(editor, &editor.paths_box, &items, icons::FOLDER, "No paths yet.", |editor, index| {
        editor.paths.borrow_mut().remove(index);
        rebuild_paths(editor);
    });
}

fn rebuild_excludes(editor: &Rc<Editor>) {
    let items = editor.excludes.borrow().clone();
    rebuild_items(editor, &editor.excludes_box, &items, icons::NOSIGN, "No exclusion patterns.", |editor, index| {
        editor.excludes.borrow_mut().remove(index);
        rebuild_excludes(editor);
    });
}

fn commit_new_exclude(editor: &Rc<Editor>) {
    let trimmed = editor.pattern.text().trim().to_string();
    if trimmed.is_empty() {
        return;
    }
    append_unique(&editor.excludes, [trimmed]);
    editor.pattern.set_text("");
    rebuild_excludes(editor);
}

fn save_schedule(editor: &Rc<Editor>) {
    kit::set_error(&editor.error, None);

    if editor.paths.borrow().is_empty() {
        kit::set_error(&editor.error, Some("Add at least one path."));
        return;
    }
    if editor.app.repos.get(&editor.repo_id).is_none() {
        kit::set_error(&editor.error, Some("Repository not found."));
        return;
    }

    let mut schedule = editor.existing.clone().unwrap_or_else(|| BackupSchedule { id: editor.new_id, ..Default::default() });
    schedule.name = editor.name.text().trim().to_string();
    schedule.frequency = editor.frequency.get();
    schedule.hour = editor.hour.value_as_int() as i64;
    schedule.minute = editor.minute.value_as_int() as i64;
    schedule.weekday = editor.day.selected() as i64;
    schedule.paths = editor.paths.borrow().clone();
    schedule.excludes = editor.excludes.borrow().clone();
    schedule.keep_daily = editor.keep_daily.value_as_int() as i64;
    schedule.keep_weekly = editor.keep_weekly.value_as_int() as i64;
    schedule.keep_monthly = editor.keep_monthly.value_as_int() as i64;

    editor.save.set_sensitive(false);
    let repo_id = editor.repo_id;
    let to_install = schedule.clone();
    let weak = Rc::downgrade(editor);
    kit::bg(
        move || schedule::install(&repo_id, &to_install),
        move |result| {
            let Some(editor) = weak.upgrade() else { return };
            editor.save.set_sensitive(true);
            if let Err(e) = result {
                kit::set_error(&editor.error, Some(&e));
                return;
            }
            let Some(mut repo) = editor.app.repos.get(&repo_id) else {
                kit::set_error(&editor.error, Some("Repository not found."));
                return;
            };
            match repo.schedules.iter_mut().find(|s| s.id == schedule.id) {
                Some(slot) => *slot = schedule,
                None => repo.schedules.push(schedule),
            }
            editor.app.repos.update(repo);
            refresh_statuses(&editor.app);
            editor.dialog.close();
        },
    );
}

// ============================================================= debug hooks

/// Reaches deeper states from `LUNA_DEBUG_OPEN=schedule-<action>` (see
/// `debug.rs`). Acts on the first schedule of the repository; the paths for
/// `new-save` come from `LUNA_DEBUG_PATH` (colon-separated).
pub fn debug_open(parent: &impl IsA<gtk::Widget>, app: &Rc<AppState>, repo: &Repository, action: &str) {
    let view = present_list(parent, app, repo);
    let first = view.schedules().into_iter().next();
    match (action, first) {
        ("new", _) => {
            edit(&view, None);
        }
        ("new-invalid", _) => {
            let editor = edit(&view, None);
            save_schedule(&editor);
        }
        ("new-save", _) => {
            let editor = edit(&view, None);
            editor.name.set_text("Debug hourly");
            let paths = std::env::var("LUNA_DEBUG_PATH").unwrap_or_default();
            append_unique(&editor.paths, paths.split(':').filter(|p| !p.is_empty()).map(str::to_string));
            append_unique(&editor.excludes, BackupSchedule::COMMON_EXCLUDE_PRESETS.iter().map(|p| p.to_string()));
            rebuild_paths(&editor);
            rebuild_excludes(&editor);
            save_schedule(&editor);
        }
        ("edit", Some(s)) => {
            edit(&view, Some(s));
        }
        ("edit-save", Some(s)) => {
            let editor = edit(&view, Some(s));
            editor.name.set_text("Debug weekly");
            editor.frequency.set(Frequency::Weekly);
            editor.day.set_selected(3);
            editor.hour.set_value(4.0);
            editor.minute.set_value(5.0);
            save_schedule(&editor);
        }
        ("run", Some(s)) => run_now(&view, &s),
        ("stop", Some(s)) => confirm_stop(&view, &s),
        ("stop-now", Some(s)) => stop(&Rc::downgrade(&view), view.repo.id, s.id),
        ("log", Some(s)) => show_log(&view, &s),
        ("disable", Some(s)) => set_enabled(&view, &s, false),
        ("enable", Some(s)) => set_enabled(&view, &s, true),
        ("delete", Some(s)) => confirm_delete(&view, &s),
        ("delete-now", Some(s)) => delete(&view, &s),
        _ => eprintln!("schedule debug: unknown action or no schedule: {action}"),
    }
}
