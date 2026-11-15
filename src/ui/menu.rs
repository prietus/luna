//! The tray popover (BorgMac's `MenuBarExtra` window, MenuBarContentView.swift):
//! every schedule with its last result / next run / live progress, manual
//! backups, and the quick actions. A layer-shell surface under the tray on
//! wlroots compositors, a small undecorated window elsewhere.
use super::kit::{self, icons};
use super::state::{
    AppState, ManualBackupJob, ManualStatus, Signal, connect_while_alive, refresh_statuses, refresh_statuses_soon,
};
use super::{borg_wizard, borgbase_wizard, borgbox_wizard, window};
use crate::core::models::{BackupRunStatus, BackupSchedule, Repository};
use crate::core::{format, paths, schedule};
use adw::prelude::*;
use chrono::{Local, Utc};
use gtk::{gdk, glib};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

const WINDOW_CLASS: &str = "borg-menu";
const WIDTH: i32 = 340;

thread_local! {
    /// When the popover last closed because it lost focus. Clicking the tray
    /// icon while it is open first takes the focus away (closing it) and
    /// then asks for a toggle, which must not reopen it.
    static FOCUS_CLOSED_AT: Cell<Option<Instant>> = const { Cell::new(None) };
}

struct Menu {
    app: Rc<AppState>,
    window: glib::WeakRef<gtk::Window>,
    body: gtk::Box,
    /// Indeterminate progress bars of the current render, advanced by one
    /// shared timer.
    pulsing: RefCell<Vec<gtk::ProgressBar>>,
}

fn current(app: &Rc<AppState>) -> Option<gtk::Window> {
    app.application.windows().into_iter().find(|w| w.has_css_class(WINDOW_CLASS))
}

/// Shows the menu window, or hides it when it is already visible.
pub fn toggle(app: &Rc<AppState>) {
    if let Some(existing) = current(app) {
        existing.close();
        return;
    }
    let just_closed =
        FOCUS_CLOSED_AT.with(|c| c.take()).map(|at| at.elapsed() < Duration::from_millis(400)).unwrap_or(false);
    if just_closed {
        return;
    }
    build(app).present();
    // Statuses may be up to a minute old while idle.
    refresh_statuses(app);
}

fn build(app: &Rc<AppState>) -> gtk::Window {
    // Built on show and destroyed on hide: a hidden window would keep the
    // application alive after the main window closes.
    let window = gtk::Window::builder()
        .application(&app.application)
        .title(format!("{} Menu", paths::APP_NAME))
        .decorated(false)
        .resizable(false)
        .default_width(WIDTH)
        .build();
    window.add_css_class(WINDOW_CLASS);

    if gtk4_layer_shell::is_supported() {
        window.init_layer_shell();
        window.set_namespace(Some("luna-menu"));
        window.set_layer(Layer::Overlay);
        window.set_anchor(Edge::Top, true);
        window.set_anchor(Edge::Right, true);
        window.set_margin(Edge::Top, 6);
        window.set_margin(Edge::Right, 8);
        window.set_keyboard_mode(KeyboardMode::OnDemand);
        window.add_css_class("layer");
    }

    let body = kit::vbox(0);
    body.set_size_request(WIDTH, -1);
    window.set_child(Some(&body));

    let menu = Rc::new(Menu { app: app.clone(), window: window.downgrade(), body, pulsing: RefCell::new(vec![]) });

    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed(glib::clone!(
        #[weak]
        window,
        #[weak]
        app,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_, key, _, modifiers| {
            if key == gdk::Key::Escape {
                window.close();
                glib::Propagation::Stop
            } else if key == gdk::Key::q && modifiers.contains(gdk::ModifierType::CONTROL_MASK) {
                app.application.quit();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        }
    ));
    window.add_controller(keys);

    // Popover behaviour: dismissed as soon as the focus goes elsewhere.
    let was_active = Cell::new(false);
    window.connect_is_active_notify(move |w| {
        if w.is_active() {
            was_active.set(true);
        } else if was_active.get() {
            FOCUS_CLOSED_AT.with(|c| c.set(Some(Instant::now())));
            w.close();
        }
    });

    let signals: [fn(&AppState) -> &Signal; 3] =
        [|a| &a.statuses.changed, |a| &a.repos.changed, |a| &a.manual.changed];
    for pick in signals {
        connect_while_alive(
            app,
            pick,
            &window,
            glib::clone!(
                #[weak]
                menu,
                move || render(&menu)
            ),
        );
    }

    // Relative times ("last 2 min ago") age while the popover stays open.
    glib::timeout_add_seconds_local(
        30,
        glib::clone!(
            #[weak]
            menu,
            #[upgrade_or]
            glib::ControlFlow::Break,
            move || {
                render(&menu);
                glib::ControlFlow::Continue
            }
        ),
    );
    glib::timeout_add_local(
        Duration::from_millis(120),
        glib::clone!(
            #[weak]
            menu,
            #[upgrade_or]
            glib::ControlFlow::Break,
            move || {
                for bar in menu.pulsing.borrow().iter() {
                    bar.pulse();
                }
                glib::ControlFlow::Continue
            }
        ),
    );

    render(&menu);
    // The window owns the menu state; every handler refers to it weakly, so
    // the timers above stop once the window is destroyed.
    window.connect_destroy(move |_| {
        let _ = &menu;
    });
    window
}

fn close(m: &Menu) {
    if let Some(w) = m.window.upgrade() {
        w.close();
    }
}

fn show_main_window(m: &Menu) {
    let app = m.app.clone();
    close(m);
    window::show(&app);
}

// -------------------------------------------------------------------- render

fn render(m: &Rc<Menu>) {
    kit::clear_box(&m.body);
    m.pulsing.borrow_mut().clear();

    m.body.append(&header(m));
    m.body.append(&kit::divider());
    let jobs = m.app.manual.ordered_jobs();
    if !jobs.is_empty() {
        m.body.append(&manual_backups_section(m, &jobs));
        m.body.append(&kit::divider());
    }
    m.body.append(&repo_list(m));
    m.body.append(&kit::divider());
    m.body.append(&footer(m));
}

fn header(m: &Rc<Menu>) -> gtk::Box {
    let row = kit::hbox(8);
    row.set_margin_start(12);
    row.set_margin_end(12);
    row.set_margin_top(10);
    row.set_margin_bottom(10);
    row.append(&kit::icon_styled(icons::BOX, &["tint"]));
    row.append(&kit::label(paths::APP_NAME, &["menu-headline"]));
    row.append(&kit::spacer());
    let count = m.app.repos.repositories().len();
    row.append(&kit::label(
        &format!("{count} repo{}", if count == 1 { "" } else { "s" }),
        &["caption", "secondary", "stats-digits"],
    ));
    row
}

/// Icon column of a row (`Image(...).frame(width: 18)`).
fn row_icon(name: &str, color_class: &str) -> gtk::Image {
    let icon = kit::icon_styled(name, &[color_class, "menu-row-icon"]);
    icon.set_size_request(18, -1);
    icon.set_valign(gtk::Align::Center);
    icon
}

/// Borderless trailing button of a row (cancel / dismiss / run now).
fn inline_button(icon_name: &str, tooltip: &str) -> gtk::Button {
    let b = kit::icon_button(icon_name, tooltip);
    b.add_css_class("plain");
    b.add_css_class("menu-inline");
    b.set_valign(gtk::Align::Center);
    b
}

// ------------------------------------------------------------ manual backups

fn manual_backups_section(m: &Rc<Menu>, jobs: &[ManualBackupJob]) -> gtk::Box {
    let section = kit::vbox(0);
    let title = kit::label("Manual backups", &["menu-section"]);
    title.set_margin_start(12);
    title.set_margin_end(12);
    title.set_margin_top(8);
    title.set_margin_bottom(4);
    section.append(&title);
    for job in jobs {
        section.append(&manual_backup_row(m, job));
    }
    section
}

fn manual_backup_row(m: &Rc<Menu>, job: &ManualBackupJob) -> gtk::Box {
    let (icon, color) = match job.status {
        ManualStatus::Running => (icons::SYNC, "c-orange"),
        ManualStatus::Succeeded => (icons::CHECK_CIRCLE, "c-green"),
        ManualStatus::Failed => (icons::WARNING, "c-red"),
        ManualStatus::Cancelled => (STOP_CIRCLE, "secondary"),
    };
    let row = kit::hbox(10);
    row.set_margin_start(12);
    row.set_margin_end(12);
    row.set_margin_top(6);
    row.set_margin_bottom(6);
    row.append(&row_icon(icon, color));

    let texts = kit::vbox(1);
    texts.set_hexpand(true);
    texts.append(&kit::ellipsized(&job.repo_name, false, &[]));
    texts.append(&kit::ellipsized(&job.archive_name, true, &["menu-mono", "secondary"]));
    match (&job.error_message, job.status) {
        (Some(err), ManualStatus::Failed) => texts.append(&two_line_error(err)),
        _ => texts.append(&kit::ellipsized(&manual_subtitle(job), false, &["caption2", "tertiary"])),
    }
    row.append(&texts);

    let job_id = job.id;
    if job.status == ManualStatus::Running {
        let cancel = inline_button(icons::XMARK_CIRCLE, "Cancel backup (SIGTERM to borg)");
        cancel.connect_clicked(glib::clone!(
            #[weak]
            m,
            move |_| m.app.manual.cancel(&job_id)
        ));
        row.append(&cancel);
    } else {
        let dismiss = inline_button(icons::XMARK_CIRCLE, "Hide from the list");
        dismiss.connect_clicked(glib::clone!(
            #[weak]
            m,
            move |_| m.app.manual.dismiss(&job_id)
        ));
        row.append(&dismiss);
    }
    row
}

const STOP_CIRCLE: &str = "media-playback-stop-symbolic"; // stop.circle.fill
const WINDOW: &str = "focus-windows-symbolic"; // macwindow
const POWER: &str = "system-shutdown-symbolic"; // power

fn manual_subtitle(job: &ManualBackupJob) -> String {
    let elapsed = format::relative_short(&job.started_at);
    match job.status {
        ManualStatus::Running => format!("in progress · {elapsed}"),
        ManualStatus::Succeeded => format!("completed {elapsed}"),
        ManualStatus::Failed => format!("failed {elapsed}"),
        ManualStatus::Cancelled => format!("cancelled {elapsed}"),
    }
}

fn two_line_error(err: &str) -> gtk::Label {
    // Line breaks would defeat the two-line limit.
    let l = kit::label(&err.split_whitespace().collect::<Vec<_>>().join(" "), &["caption2", "c-red"]);
    l.set_wrap(true);
    l.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    l.set_lines(2);
    l.set_ellipsize(gtk::pango::EllipsizeMode::End);
    l.set_tooltip_text(Some(err));
    l
}

// ----------------------------------------------------------------- repo list

/// Rows displayed in the popover — one per repo, plus one per schedule
/// inside each repo. Repos without schedules still get a single row so
/// the user can jump to them.
fn row_models(repositories: &[Repository]) -> Vec<(&Repository, Option<&BackupSchedule>)> {
    let mut rows = vec![];
    for repo in repositories {
        if repo.schedules.is_empty() {
            rows.push((repo, None));
        } else {
            rows.extend(repo.schedules.iter().map(|s| (repo, Some(s))));
        }
    }
    rows
}

fn repo_list(m: &Rc<Menu>) -> gtk::Widget {
    let repositories = m.app.repos.repositories();
    if repositories.is_empty() {
        let empty = kit::vbox(6);
        empty.set_margin_top(18);
        empty.set_margin_bottom(18);
        let icon = kit::icon_styled(icons::TRAY, &["secondary", "big-icon"]);
        empty.append(&icon);
        let text = kit::label("No repositories", &["caption", "secondary"]);
        text.set_halign(gtk::Align::Center);
        empty.append(&text);
        let add = kit::button(&format!("Add from {}…", paths::APP_NAME));
        add.set_halign(gtk::Align::Center);
        add.connect_clicked(glib::clone!(
            #[weak]
            m,
            move |_| show_main_window(&m)
        ));
        empty.append(&add);
        return empty.upcast();
    }

    let list = kit::vbox(0);
    list.set_margin_top(4);
    list.set_margin_bottom(4);
    for (repo, schedule) in row_models(&repositories) {
        list.append(&row(m, repo, schedule));
    }
    // Unlike the Mac popover this one cannot grow past the screen: scroll
    // once there are more schedules than fit.
    let scroller = gtk::ScrolledWindow::new();
    scroller.set_hscrollbar_policy(gtk::PolicyType::Never);
    scroller.set_propagate_natural_height(true);
    scroller.set_max_content_height(460);
    scroller.set_child(Some(&list));
    scroller.upcast()
}

fn row(m: &Rc<Menu>, repo: &Repository, schedule: Option<&BackupSchedule>) -> gtk::Overlay {
    let status = schedule.and_then(|s| m.app.statuses.status(&s.id));
    let next_run = schedule.and_then(|s| schedule::next_fire_date(s, Local::now()));
    let is_running = status.as_ref().map(|s| s.running).unwrap_or(false);
    let last_error = status.as_ref().and_then(|s| s.last_error.clone());

    let (icon, color) = if is_running {
        (icons::SYNC, "c-orange")
    } else if last_error.is_some() {
        (icons::WARNING, "c-red")
    } else {
        (icons::DRIVE, "tint")
    };

    let content = kit::hbox(10);
    let icon = row_icon(icon, color);
    if let Some(err) = &last_error {
        icon.set_tooltip_text(Some(err));
    }
    content.append(&icon);

    let texts = kit::vbox(1);
    texts.set_hexpand(true);
    let primary = match schedule {
        Some(s) => format!("{} · {}", repo.name, s.display_name()),
        None => repo.name.clone(),
    };
    texts.append(&kit::ellipsized(&primary, false, &[]));
    texts.append(&kit::ellipsized(&repo.url, true, &["menu-mono", "secondary"]));
    match &status {
        Some(st) if st.running => texts.append(&progress_block(m, st)),
        _ => {
            if let Some(err) = &last_error {
                texts.append(&two_line_error(err));
            } else if let Some(line) = schedule_line(status.as_ref(), next_run) {
                texts.append(&kit::ellipsized(&line, false, &["caption2", "tertiary"]));
            }
        }
    }
    content.append(&texts);

    // The trailing action button floats over the row button (a button
    // cannot nest inside another), so keep its slot free in the content.
    let slot = kit::hbox(0);
    slot.set_size_request(if schedule.is_some() { 22 } else { 0 }, -1);
    content.append(&slot);
    content.append(&kit::icon_styled(icons::CHEVRON_RIGHT, &["menu-chevron"]));

    let button = gtk::Button::new();
    button.add_css_class("menu-row");
    button.set_child(Some(&content));
    button.set_cursor_from_name(Some("pointer"));
    let repo_id = repo.id;
    button.connect_clicked(glib::clone!(
        #[weak]
        m,
        move |_| {
            m.app.request_selection(repo_id);
            show_main_window(&m);
        }
    ));

    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&button));
    if let Some(s) = schedule {
        let schedule_id = s.id;
        let action = if is_running {
            let stop = inline_button(icons::XMARK_CIRCLE, "Stop backup (SIGTERM to borg)");
            stop.connect_clicked(glib::clone!(
                #[weak]
                m,
                move |_| {
                    let app = m.app.clone();
                    kit::bg(move || schedule::cancel(&repo_id, &schedule_id), move |_| refresh_statuses_soon(&app));
                }
            ));
            stop
        } else {
            let run = inline_button(icons::PLAY_CIRCLE, "Run now");
            run.connect_clicked(glib::clone!(
                #[weak]
                m,
                move |b| {
                    b.set_sensitive(false);
                    run_now(&m.app, repo_id, schedule_id);
                }
            ));
            run
        };
        action.set_halign(gtk::Align::End);
        action.set_margin_end(32);
        overlay.add_overlay(&action);
    }
    overlay
}

/// Fires a schedule's systemd unit right away ("Run now"), from the popover
/// or the tray menu.
pub fn run_now(app: &Rc<AppState>, repo_id: uuid::Uuid, schedule_id: uuid::Uuid) {
    let app = app.clone();
    kit::bg(
        move || schedule::kickstart(&repo_id, &schedule_id),
        move |result| {
            if let Err(e) = result {
                eprintln!("kickstart failed: {e}");
            }
            refresh_statuses_soon(&app);
        },
    );
}

/// Compact "in-progress" block for a running scheduled backup.
/// Renders a determinate progress bar when we know the previous
/// run's total (so we can compute a percent), an indeterminate bar
/// when we don't, plus a single text line with bytes/files/path.
fn progress_block(m: &Rc<Menu>, status: &BackupRunStatus) -> gtk::Box {
    let block = kit::vbox(2);
    block.set_margin_top(2);
    let bar = gtk::ProgressBar::new();
    bar.add_css_class("orange");
    bar.add_css_class("menu-progress");
    match status.progress_ratio() {
        Some(ratio) => bar.set_fraction(ratio),
        None => {
            bar.set_pulse_step(0.08);
            bar.pulse();
            m.pulsing.borrow_mut().push(bar.clone());
        }
    }
    block.append(&bar);
    // The numbers stay whole; only the path gives way when space runs out.
    let (numbers, path) = progress_parts(status);
    let line = kit::hbox(0);
    let has_numbers = !numbers.is_empty();
    if has_numbers {
        line.append(&kit::label(&numbers.join(" · "), &["caption2", "tertiary"]));
    }
    match path {
        Some(path) => {
            let text = if has_numbers { format!(" · {path}") } else { path };
            let label = kit::ellipsized(&text, true, &["caption2", "tertiary"]);
            // Keep the file name when only a few characters fit.
            label.set_ellipsize(gtk::pango::EllipsizeMode::Start);
            line.append(&label);
        }
        None if !has_numbers => line.append(&kit::label("starting…", &["caption2", "tertiary"])),
        None => {}
    }
    block.append(&line);
    block
}

/// The pieces of the "4.2 GB · 12345 files · 42% · ~3m 10s left · /foo" text
/// beneath the progress bar: the numbers, and the path borg is working on.
/// Each piece is optional so early ticks (borg hasn't reported numbers yet)
/// degrade gracefully.
fn progress_parts(status: &BackupRunStatus) -> (Vec<String>, Option<String>) {
    let mut parts: Vec<String> = vec![];
    if let Some(bytes) = status.progress_original_bytes.filter(|b| *b > 0) {
        parts.push(format::bytes_binary(bytes));
    }
    if let Some(n) = status.progress_file_count.filter(|n| *n > 0) {
        parts.push(format!("{n} {}", if n == 1 { "file" } else { "files" }));
    }
    if let Some(ratio) = status.progress_ratio() {
        parts.push(format!("{:.0}%", ratio * 100.0));
        if let Some(eta) = eta_seconds(status, ratio) {
            parts.push(format!("~{} left", format::duration(eta)));
        }
    } else if let (Some(attempt), Some(max)) = (status.attempt, status.max_attempts) {
        if attempt > 1 {
            parts.push(format!("retry {attempt}/{max}"));
        }
    }
    (parts, status.progress_current_path.clone().filter(|p| !p.is_empty()))
}

/// Rough time left, extrapolating the pace since the run started. Withheld
/// until enough has been copied for the estimate to mean something.
fn eta_seconds(status: &BackupRunStatus, ratio: f64) -> Option<i64> {
    let started = status.progress_started_at?;
    let elapsed = Utc::now().signed_duration_since(started).num_seconds();
    if !(0.02..1.0).contains(&ratio) || elapsed < 5 {
        return None;
    }
    Some((elapsed as f64 * (1.0 - ratio) / ratio).round() as i64)
}

fn schedule_line(status: Option<&BackupRunStatus>, next_run: Option<chrono::DateTime<Local>>) -> Option<String> {
    let mut parts: Vec<String> = vec![];
    if let Some(last) = status.and_then(|s| s.last_run) {
        parts.push(format!("last {}", format::relative_short(&last)));
    }
    if let Some(next) = next_run {
        parts.push(format!("next {}", format::relative_short(&next)));
    }
    if parts.is_empty() { None } else { Some(parts.join(" · ")) }
}

// -------------------------------------------------------------------- footer

fn footer(m: &Rc<Menu>) -> gtk::Box {
    let b = kit::vbox(0);
    b.set_margin_top(4);
    b.set_margin_bottom(4);
    b.append(&menu_button(m, "New on BorgBase…", icons::WAND, |m| {
        let app = m.app.clone();
        close(m);
        borgbase_wizard::open(&app);
    }));
    b.append(&menu_button(m, "New on BorgBox…", icons::SERVER, |m| {
        let app = m.app.clone();
        close(m);
        borgbox_wizard::open(&app);
    }));
    b.append(&menu_button(m, "New Borg repository (local or SSH)…", icons::WAND, |m| {
        let app = m.app.clone();
        close(m);
        borg_wizard::open(&app);
    }));
    b.append(&menu_button(m, &format!("Open {}", paths::APP_NAME), WINDOW, |m| show_main_window(m)));
    b.append(&menu_button(m, "Quit", POWER, |m| m.app.application.quit()));
    b
}

fn menu_button(m: &Rc<Menu>, label: &str, icon_name: &str, action: impl Fn(&Menu) + 'static) -> gtk::Button {
    let content = kit::hbox(10);
    let icon = kit::icon_styled(icon_name, &["menu-row-icon"]);
    icon.set_size_request(18, -1);
    content.append(&icon);
    content.append(&kit::ellipsized(label, false, &[]));
    let button = gtk::Button::new();
    button.add_css_class("menu-row");
    button.set_child(Some(&content));
    button.set_cursor_from_name(Some("pointer"));
    button.connect_clicked(glib::clone!(
        #[weak]
        m,
        move |_| action(&m)
    ));
    button
}
