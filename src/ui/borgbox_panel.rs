//! BorgBox server panel (BorgMac's `BorgBoxServerPanel`): connect to a BorgBox
//! daemon, then show its stats, active sessions, repositories (with their
//! maintenance actions) and the operations drawer with live job logs.
//!
//! Also hosts the port of `BorgBoxJobsStore`: it is scoped to one panel so
//! every job stream is torn down when the panel closes.
use super::borgbox_passphrase::title_block;
use super::kit::{self, Tint, icons};
use super::state::AppState;
use super::{borgbox_alerts, borgbox_archives, borgbox_import, borgbox_passphrase, borgbox_prune};
use crate::core::borgbox::{
    self, BorgBoxJob, BorgBoxJobStart, BorgBoxJobStreamEvent, BorgBoxRemoteRepo, BorgBoxSession, BorgBoxStorage,
    BorgBoxSystemStats,
};
use crate::core::models::{BorgBoxServer, Repository};
use crate::core::{format, keyring};
use adw::prelude::*;
use gtk::{gio, glib};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use uuid::Uuid;

const CHEVRON_DOWN: &str = "pan-down-symbolic"; // chevron.down
const CHEVRON_UP: &str = "pan-up-symbolic"; // chevron.up

// ---------------------------------------------------------------- jobs store

/// Per-job cap of the live log buffer, so a chatty borg run doesn't grow
/// unbounded.
const MAX_LOG_LINES: usize = 500;

enum StreamUpdate {
    Event(BorgBoxJobStreamEvent),
    /// The stream ended; carries the authoritative final job state when the
    /// daemon could still be reached.
    Finished(Option<BorgBoxJob>),
}

struct StreamMessage {
    job_id: String,
    /// Cancel flag of the stream that produced the message: updates from a
    /// stream that was cancelled (job dismissed, panel closed) are dropped.
    cancel: Arc<AtomicBool>,
    update: StreamUpdate,
}

/// Tracks in-flight BorgBox daemon jobs (check / prune / compact / …) and
/// subscribes to `GET /jobs/<id>/stream` for live log + status updates. One
/// worker thread per live job; events reach the main thread through `tx`.
struct JobsStore {
    jobs: RefCell<HashMap<String, BorgBoxJob>>,
    /// Live log tail accumulated from SSE `log` events. Keyed by job id.
    logs: RefCell<HashMap<String, Vec<String>>>,
    streams: RefCell<HashMap<String, Arc<AtomicBool>>>,
    tx: async_channel::Sender<StreamMessage>,
}

impl JobsStore {
    fn new(tx: async_channel::Sender<StreamMessage>) -> Self {
        JobsStore {
            jobs: RefCell::new(HashMap::new()),
            logs: RefCell::new(HashMap::new()),
            streams: RefCell::new(HashMap::new()),
            tx,
        }
    }

    /// Registers a freshly started job and opens its event stream.
    fn register(&self, initial: &BorgBoxJobStart, kind: &str, repo: &str, server: &BorgBoxServer) {
        let placeholder = BorgBoxJob {
            id: initial.job_id.clone(),
            repo: Some(repo.to_string()),
            kind: kind.to_string(),
            status: initial.status.clone(),
            started_at: None,
            finished_at: None,
            exit_code: None,
            log_tail: None,
        };
        self.logs.borrow_mut().insert(placeholder.id.clone(), vec![]);
        self.jobs.borrow_mut().insert(placeholder.id.clone(), placeholder);
        self.start_streaming(&initial.job_id, server);
    }

    /// Seeds the store with a snapshot from `GET /jobs` — typically called
    /// when the panel opens, so any job that started before the app launched
    /// shows up immediately. Non-terminal jobs get a live stream attached.
    fn hydrate(&self, list: Vec<BorgBoxJob>, server: &BorgBoxServer) {
        for job in list {
            if self.jobs.borrow().contains_key(&job.id) {
                continue;
            }
            // Prime the live log buffer with whatever snapshot the REST
            // endpoint returned so the UI doesn't flash an empty area while
            // the stream reconnects.
            self.logs.borrow_mut().insert(job.id.clone(), job.log_tail.clone().unwrap_or_default());
            let live = !job.is_terminal();
            let id = job.id.clone();
            self.jobs.borrow_mut().insert(id.clone(), job);
            if live {
                self.start_streaming(&id, server);
            }
        }
    }

    /// User dismissed the job from the drawer — stop streaming and drop state.
    fn dismiss(&self, job_id: &str) {
        if let Some(cancel) = self.streams.borrow_mut().remove(job_id) {
            cancel.store(true, Ordering::Relaxed);
        }
        self.jobs.borrow_mut().remove(job_id);
        self.logs.borrow_mut().remove(job_id);
    }

    /// Stops every stream and forgets every job.
    fn clear(&self) {
        for (_, cancel) in self.streams.borrow_mut().drain() {
            cancel.store(true, Ordering::Relaxed);
        }
        self.jobs.borrow_mut().clear();
        self.logs.borrow_mut().clear();
    }

    /// Sorted view for the UI — active jobs above terminal ones, newest-first.
    fn ordered_jobs(&self) -> Vec<BorgBoxJob> {
        let mut jobs: Vec<BorgBoxJob> = self.jobs.borrow().values().cloned().collect();
        jobs.sort_by(|a, b| {
            a.is_terminal()
                .cmp(&b.is_terminal())
                .then_with(|| b.started_at.as_deref().unwrap_or("").cmp(a.started_at.as_deref().unwrap_or("")))
                .then_with(|| a.id.cmp(&b.id))
        });
        jobs
    }

    /// Lines to show for a job: the live buffer (SSE) when it has anything —
    /// it's what we've been appending in real time — else the snapshot
    /// `log_tail` of terminal jobs rehydrated from `GET /jobs`.
    fn lines(&self, job: &BorgBoxJob) -> Vec<String> {
        match self.logs.borrow().get(&job.id) {
            Some(live) if !live.is_empty() => live.clone(),
            _ => job.log_tail.clone().unwrap_or_default(),
        }
    }

    fn start_streaming(&self, job_id: &str, server: &BorgBoxServer) {
        if let Some(previous) = self.streams.borrow_mut().remove(job_id) {
            previous.store(true, Ordering::Relaxed);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, server, id, flag) = (self.tx.clone(), server.clone(), job_id.to_string(), cancel.clone());
        let spawned = std::thread::Builder::new().name("borgbox-job".to_string()).spawn(move || {
            let send = |update: StreamUpdate| {
                let _ = tx.send_blocking(StreamMessage { job_id: id.clone(), cancel: flag.clone(), update });
            };
            if let Err(e) = borgbox::stream_job(&server, &id, &flag, &mut |event| send(StreamUpdate::Event(event))) {
                eprintln!("[BorgBoxJobsStore] stream failed for job {id}: {e}");
            }
            if flag.load(Ordering::Relaxed) {
                return;
            }
            // Stream closed: pull the final job state so the UI has the
            // authoritative exit_code / finished_at fields that aren't sent
            // over SSE directly.
            send(StreamUpdate::Finished(borgbox::job(&server, &id).ok()));
        });
        match spawned {
            Ok(_) => {
                self.streams.borrow_mut().insert(job_id.to_string(), cancel);
            }
            Err(e) => eprintln!("[BorgBoxJobsStore] could not start the stream thread for job {job_id}: {e}"),
        }
    }

    fn apply(&self, message: StreamMessage) {
        if message.cancel.load(Ordering::Relaxed) {
            return;
        }
        let id = message.job_id;
        match message.update {
            StreamUpdate::Event(event) => match event.event_type.as_str() {
                "log" => {
                    if let Some(line) = event.line.filter(|l| !l.is_empty()) {
                        let mut logs = self.logs.borrow_mut();
                        let buffer = logs.entry(id).or_default();
                        buffer.push(line);
                        if buffer.len() > MAX_LOG_LINES {
                            let excess = buffer.len() - MAX_LOG_LINES;
                            buffer.drain(..excess);
                        }
                    }
                }
                "status" => {
                    if let (Some(job), Some(status)) =
                        (self.jobs.borrow_mut().get_mut(&id), event.status.filter(|s| !s.is_empty()))
                    {
                        job.status = status;
                        job.exit_code = event.exit_code.or(job.exit_code);
                    }
                }
                // "end": the stream loop exits on its own.
                _ => {}
            },
            StreamUpdate::Finished(final_job) => {
                if let Some(job) = final_job {
                    self.jobs.borrow_mut().insert(id.clone(), job);
                }
                let mut streams = self.streams.borrow_mut();
                if streams.get(&id).is_some_and(|current| Arc::ptr_eq(current, &message.cancel)) {
                    streams.remove(&id);
                }
            }
        }
    }
}

// --------------------------------------------------------------------- panel

struct JobRow {
    root: gtk::Box,
    icon: gtk::Stack,
    title: gtk::Label,
    status: gtk::Label,
    tail: gtk::Box,
    collapsed: gtk::Label,
    expanded: gtk::ScrolledWindow,
    expanded_text: gtk::Label,
    toggle: gtk::Button,
    toggle_icon: gtk::Image,
    toggle_text: gtk::Label,
}

/// Widgets of the connected-server layout that are updated in place.
struct Content {
    refresh: gtk::Button,
    scroller: gtk::ScrolledWindow,
    loading: gtk::Box,
    stats: gtk::Box,
    sessions: gtk::Box,
    repos: gtk::Box,
    jobs: gtk::Box,
    jobs_empty: gtk::Label,
    job_rows: RefCell<HashMap<String, JobRow>>,
    error: gtk::Label,
}

struct Panel {
    app: Rc<AppState>,
    dialog: adw::Dialog,
    root: gtk::Box,
    actions: gio::SimpleActionGroup,
    server: RefCell<Option<BorgBoxServer>>,
    stats: RefCell<Option<BorgBoxSystemStats>>,
    repos: RefCell<Vec<BorgBoxRemoteRepo>>,
    sessions: RefCell<Vec<BorgBoxSession>>,
    /// Number of `load_all` rounds in flight.
    loading: Cell<u32>,
    error: RefCell<Option<String>>,
    /// Passphrases the user entered during this panel session, keyed by
    /// remote repo name. Only kept in memory — wiped when the panel closes.
    session_passphrases: RefCell<HashMap<String, String>>,
    /// Jobs whose log tail the user has expanded to see the full rolling
    /// buffer. Collapsed by default.
    expanded_job_logs: RefCell<HashSet<String>>,
    jobs: JobsStore,
    content: RefCell<Option<Content>>,
    /// Error label of the "add server" form while that layout is shown.
    form_error: RefCell<Option<gtk::Label>>,
    store_handlers: RefCell<Vec<(bool, u64)>>,
}

pub fn present(parent: &impl IsA<gtk::Widget>, app: &Rc<AppState>) {
    let (dialog, root) = kit::bare_sheet("BorgBox", 680, 720);
    root.add_css_class("bb-panel");
    let (tx, rx) = async_channel::unbounded::<StreamMessage>();
    let actions = gio::SimpleActionGroup::new();
    root.insert_action_group("bb", Some(&actions));

    let panel = Rc::new(Panel {
        app: app.clone(),
        dialog: dialog.clone(),
        root,
        actions,
        server: RefCell::new(None),
        stats: RefCell::new(None),
        repos: RefCell::new(vec![]),
        sessions: RefCell::new(vec![]),
        loading: Cell::new(0),
        error: RefCell::new(None),
        session_passphrases: RefCell::new(HashMap::new()),
        expanded_job_logs: RefCell::new(HashSet::new()),
        jobs: JobsStore::new(tx),
        content: RefCell::new(None),
        form_error: RefCell::new(None),
        store_handlers: RefCell::new(vec![]),
    });
    install_actions(&panel);

    // Job stream events → main thread.
    let weak = Rc::downgrade(&panel);
    glib::spawn_future_local(async move {
        while let Ok(message) = rx.recv().await {
            let Some(panel) = weak.upgrade() else { break };
            let job_id = message.job_id.clone();
            let log_only = matches!(&message.update, StreamUpdate::Event(e) if e.event_type == "log");
            panel.jobs.apply(message);
            if log_only {
                render_job(&panel, &job_id);
            } else {
                // Status changes and the final state reorder the drawer.
                render_jobs(&panel);
            }
        }
    });

    let servers_changed = app.borgbox.changed.connect(glib::clone!(
        #[weak]
        panel,
        move || sync_server(&panel)
    ));
    // The repo menus hide "Import…" for repos that are already in the sidebar.
    let repos_changed = app.repos.changed.connect(glib::clone!(
        #[weak]
        panel,
        move || render_repos(&panel)
    ));
    *panel.store_handlers.borrow_mut() = vec![(true, servers_changed), (false, repos_changed)];

    // The panel state lives until the sheet goes away; closing it stops
    // every job stream.
    let keep = Rc::new(RefCell::new(Some(panel.clone())));
    let teardown = move || {
        let Some(panel) = keep.borrow_mut().take() else { return };
        panel.jobs.clear();
        panel.jobs.tx.close();
        for (servers, id) in panel.store_handlers.borrow_mut().drain(..) {
            if servers {
                panel.app.borgbox.changed.disconnect(id);
            } else {
                panel.app.repos.changed.disconnect(id);
            }
        }
    };
    dialog.connect_closed({
        let teardown = teardown.clone();
        move |_| teardown()
    });
    panel.root.connect_destroy(move |_| teardown());

    sync_server(&panel);
    dialog.present(Some(parent));
    debug_script(&panel);
}

/// Shows the layout matching the current (first) server; reloads everything
/// when the server changed.
fn sync_server(p: &Rc<Panel>) {
    let server = p.app.borgbox.first();
    let unchanged = p.server.borrow().as_ref().map(|s| s.id) == server.as_ref().map(|s| s.id);
    if unchanged && (p.content.borrow().is_some() || p.form_error.borrow().is_some()) {
        return;
    }
    p.jobs.clear();
    p.expanded_job_logs.borrow_mut().clear();
    p.session_passphrases.borrow_mut().clear();
    *p.stats.borrow_mut() = None;
    p.repos.borrow_mut().clear();
    p.sessions.borrow_mut().clear();
    *p.server.borrow_mut() = server.clone();
    *p.content.borrow_mut() = None;
    *p.form_error.borrow_mut() = None;
    kit::clear_box(&p.root);
    match server {
        Some(server) => {
            build_content(p, &server);
            load_all(p);
        }
        None => build_server_form(p),
    }
}

fn set_error(p: &Panel, message: Option<String>) {
    if let Some(content) = p.content.borrow().as_ref() {
        kit::set_error(&content.error, message.as_deref());
    }
    if let Some(label) = p.form_error.borrow().as_ref() {
        kit::set_error(label, message.as_deref());
    }
    *p.error.borrow_mut() = message;
}

fn close_button(p: &Rc<Panel>) -> gtk::Button {
    let close = kit::button("Close");
    close.connect_clicked(glib::clone!(
        #[weak(rename_to = dialog)]
        p.dialog,
        move |_| {
            dialog.close();
        }
    ));
    close
}

fn header_row(icon_and_texts: &gtk::Box, trailing: &[&gtk::Widget]) -> gtk::Box {
    let row = kit::hbox(10);
    row.set_margin_top(14);
    row.set_margin_bottom(14);
    row.set_margin_start(20);
    row.set_margin_end(20);
    icon_and_texts.set_hexpand(true);
    row.append(icon_and_texts);
    for w in trailing {
        w.set_valign(gtk::Align::Center);
        row.append(*w);
    }
    row
}

/// SwiftUI `GroupBox(title)`: a small title above a rounded block.
fn group_box(title: &str) -> (gtk::Box, gtk::Box) {
    let outer = kit::vbox(6);
    outer.append(&kit::label(title, &["section-title"]));
    let inner = kit::vbox(6);
    outer.append(&kit::card(&inner));
    (outer, inner)
}

// ------------------------------------------------------- connected layout

fn build_content(p: &Rc<Panel>, server: &BorgBoxServer) {
    let texts = title_block(icons::SERVER, &server.name, &server.daemon_url);
    if let Some(url) = texts.last_child().and_then(|t| t.last_child()).and_then(|w| w.downcast::<gtk::Label>().ok()) {
        url.set_selectable(true);
        url.set_focusable(false);
    }
    let refresh = kit::icon_button(icons::REFRESH, "Refresh");
    refresh.add_css_class("plain");
    refresh.connect_clicked(glib::clone!(
        #[weak]
        p,
        move |_| load_all(&p)
    ));
    let menu = gio::Menu::new();
    menu.append(Some("Change server…"), Some("bb.change-server"));
    let more = gtk::MenuButton::new();
    more.set_icon_name(icons::ELLIPSIS_CIRCLE);
    more.add_css_class("flat");
    more.set_menu_model(Some(&menu));
    let close = close_button(p);
    p.root.append(&header_row(&texts, &[refresh.upcast_ref(), more.upcast_ref(), close.upcast_ref()]));
    p.root.append(&kit::divider());

    let body = kit::vbox(16);
    body.set_margin_top(20);
    body.set_margin_bottom(20);
    body.set_margin_start(20);
    body.set_margin_end(20);

    let loading = kit::vbox(8);
    loading.set_margin_top(40);
    loading.set_margin_bottom(40);
    loading.set_halign(gtk::Align::Center);
    let spinner = kit::spinner();
    spinner.set_halign(gtk::Align::Center);
    loading.append(&spinner);
    loading.append(&kit::label("Loading…", &["secondary"]));
    body.append(&loading);

    let stats = kit::vbox(0);
    stats.set_visible(false);
    body.append(&stats);
    let sessions = kit::vbox(0);
    body.append(&sessions);
    let repos = kit::vbox(0);
    body.append(&repos);

    let (jobs_card, jobs) = group_box("Operations");
    jobs.set_spacing(10);
    let jobs_empty = kit::caption("No operations. Launch Check/Prune/Compact from a repo's ⋯ menu.");
    jobs.append(&jobs_empty);
    body.append(&jobs_card);

    let error = kit::error_label();
    body.append(&error);

    let scroller = kit::scrolled(&body);
    // Let the expanded job logs take their natural height (up to their cap)
    // instead of the minimum a viewport hands out by default.
    if let Some(viewport) = scroller.child().and_then(|c| c.downcast::<gtk::Viewport>().ok()) {
        viewport.set_vscroll_policy(gtk::ScrollablePolicy::Natural);
    }
    p.root.append(&scroller);
    *p.content.borrow_mut() = Some(Content {
        refresh,
        scroller,
        loading,
        stats,
        sessions,
        repos,
        jobs,
        jobs_empty,
        job_rows: RefCell::new(HashMap::new()),
        error,
    });
    render_all(p);
}

fn render_all(p: &Rc<Panel>) {
    render_loading(p);
    render_stats(p);
    render_sessions(p);
    render_repos(p);
    render_jobs(p);
    let error = p.error.borrow().clone();
    if let Some(content) = p.content.borrow().as_ref() {
        kit::set_error(&content.error, error.as_deref());
    }
}

fn render_loading(p: &Panel) {
    if let Some(content) = p.content.borrow().as_ref() {
        let loading = p.loading.get() > 0;
        content.refresh.set_sensitive(!loading);
        content.loading.set_visible(loading && p.stats.borrow().is_none());
    }
}

fn stat_row(name: &str, value: &str) -> gtk::Box {
    let row = kit::hbox(8);
    let name = kit::label(name, &["caption", "secondary"]);
    name.set_width_request(110);
    row.append(&name);
    let value = kit::label(value, &["caption", "mono"]);
    value.set_selectable(true);
    value.set_focusable(false);
    row.append(&value);
    row
}

fn format_uptime(seconds: i64) -> String {
    let days = seconds / 86400;
    let hours = (seconds % 86400) / 3600;
    let mins = (seconds % 3600) / 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {mins}m")
    } else {
        format!("{mins}m")
    }
}

fn storage_fraction(storage: &BorgBoxStorage) -> f64 {
    storage.used_bytes as f64 / storage.total_bytes.max(1) as f64
}

fn render_stats(p: &Panel) {
    let content = p.content.borrow();
    let Some(content) = content.as_ref() else { return };
    kit::clear_box(&content.stats);
    let stats = p.stats.borrow();
    content.stats.set_visible(stats.is_some());
    let Some(stats) = stats.as_ref() else { return };

    let (card, rows) = group_box("Server");
    rows.set_spacing(8);
    rows.append(&stat_row("Hostname", &stats.hostname));
    rows.append(&stat_row("Uptime", &format_uptime(stats.uptime_seconds)));
    let load: Vec<String> = stats.load.iter().map(|l| format!("{l:.2}")).collect();
    rows.append(&stat_row("Load", &load.join(" · ")));
    if let Some(version) = &stats.borgbox_version {
        rows.append(&stat_row("Daemon version", version));
    }
    rows.append(&kit::divider());
    rows.append(&stat_row("Path", &stats.storage.path));
    rows.append(&stat_row("Total", &format::bytes_binary(stats.storage.total_bytes)));
    rows.append(&stat_row("Used", &format::bytes_binary(stats.storage.used_bytes)));
    rows.append(&stat_row("Free", &format::bytes_binary(stats.storage.free_bytes)));
    let bar = gtk::ProgressBar::new();
    let fraction = storage_fraction(&stats.storage);
    bar.set_fraction(fraction.clamp(0.0, 1.0));
    if fraction > 0.9 {
        bar.add_css_class("red");
    } else if fraction > 0.75 {
        bar.add_css_class("orange");
    }
    rows.append(&bar);
    content.stats.append(&card);
}

fn render_sessions(p: &Panel) {
    let content = p.content.borrow();
    let Some(content) = content.as_ref() else { return };
    kit::clear_box(&content.sessions);
    let (card, rows) = group_box("Active sessions");
    let sessions = p.sessions.borrow();
    if sessions.is_empty() {
        rows.append(&kit::caption("No active borg sessions at the moment."));
    }
    for session in sessions.iter() {
        let row = kit::hbox(8);
        row.append(&kit::icon_styled(icons::BOLT, &["tint"]));
        row.append(&kit::label(&session.repo, &["mono"]));
        row.append(&kit::spacer());
        if let Some(ip) = &session.client_ip {
            row.append(&kit::label(ip, &["caption", "mono", "secondary"]));
        }
        if let Some(cpu) = session.cpu_percent {
            row.append(&kit::label(&format!("{cpu:.0}%"), &["caption", "numeric", "secondary"]));
        }
        rows.append(&row);
    }
    content.sessions.append(&card);
}

fn menu_item(label: &str, action: &str, repo: &str) -> gio::MenuItem {
    let item = gio::MenuItem::new(Some(label), None);
    item.set_action_and_target_value(Some(&format!("bb.{action}")), Some(&repo.to_variant()));
    item
}

fn repo_menu(p: &Panel, repo: &BorgBoxRemoteRepo) -> gio::Menu {
    let name = repo.name.as_str();
    let menu = gio::Menu::new();
    let open = gio::Menu::new();
    open.append_item(&menu_item("Archives…", "archives", name));
    open.append_item(&menu_item("Alerts…", "alerts", name));
    menu.append_section(None, &open);
    let maintenance = gio::Menu::new();
    maintenance.append_item(&menu_item("Check", "check", name));
    maintenance.append_item(&menu_item("Prune…", "prune", name));
    maintenance.append_item(&menu_item("Compact", "compact", name));
    menu.append_section(None, &maintenance);
    if !is_already_imported(p, repo) {
        let import = gio::Menu::new();
        import.append_item(&menu_item("Import into Luna…", "import", name));
        menu.append_section(None, &import);
    }
    let danger = gio::Menu::new();
    if repo.append_only == Some(true) {
        danger.append_item(&menu_item("Disable append-only…", "append-only-off", name));
    } else {
        danger.append_item(&menu_item("Enable append-only", "append-only-on", name));
    }
    danger.append_item(&menu_item("Break lock", "break-lock", name));
    danger.append_item(&menu_item("Delete repo…", "delete", name));
    menu.append_section(None, &danger);
    menu
}

fn repo_row(p: &Panel, repo: &BorgBoxRemoteRepo) -> gtk::Box {
    let registered = repo.is_registered();
    let row = kit::hbox(10);
    row.set_margin_top(2);
    row.set_margin_bottom(2);
    let icon = kit::icon_styled(icons::DRIVE, &[if registered { "tint" } else { "secondary" }]);
    icon.set_width_request(18);
    row.append(&icon);

    let texts = kit::vbox(1);
    texts.set_valign(gtk::Align::Center);
    let title = kit::hbox(6);
    title.append(&kit::label(&repo.name, &["bb-medium"]));
    if !registered {
        title.append(&kit::badge("Unregistered", Tint::Orange));
    }
    if repo.append_only == Some(true) {
        let badge = kit::hbox(3);
        badge.add_css_class("badge");
        badge.add_css_class("b-blue");
        badge.set_valign(gtk::Align::Center);
        badge.append(&kit::icon(icons::LOCK));
        badge.append(&gtk::Label::new(Some("Append-only")));
        badge.set_tooltip_text(Some(
            "The daemon enforces append-only on this repo: clients can add archives but cannot prune or delete without server-side intervention.",
        ));
        title.append(&badge);
    }
    texts.append(&title);
    if let Some(size) = repo.size_bytes {
        texts.append(&kit::label(&format::bytes_binary(size), &["caption", "numeric", "secondary"]));
    }
    row.append(&texts);
    row.append(&kit::spacer());

    if registered {
        let more = gtk::MenuButton::new();
        more.set_icon_name(icons::ELLIPSIS_CIRCLE);
        more.add_css_class("flat");
        more.set_valign(gtk::Align::Center);
        more.set_menu_model(Some(&repo_menu(p, repo)));
        row.append(&more);
    } else {
        let register = kit::button("");
        register.add_css_class("bb-small");
        let label = kit::hbox(4);
        label.append(&kit::icon(icons::KEY));
        label.append(&gtk::Label::new(Some("Register…")));
        register.set_child(Some(&label));
        register.set_valign(gtk::Align::Center);
        register.set_tooltip_text(Some(
            "Generates a dedicated SSH key, registers it with the daemon, and adopts the repo in Luna.",
        ));
        register.set_action_name(Some("bb.register"));
        register.set_action_target_value(Some(&repo.name.to_variant()));
        row.append(&register);
    }
    row
}

fn render_repos(p: &Rc<Panel>) {
    let content = p.content.borrow();
    let Some(content) = content.as_ref() else { return };
    kit::clear_box(&content.repos);
    let (card, rows) = group_box("Repositories");
    let repos = p.repos.borrow();
    if repos.is_empty() {
        rows.append(&kit::caption("No repositories registered."));
    }
    for repo in repos.iter() {
        rows.append(&repo_row(p, repo));
    }
    content.repos.append(&card);
}

// ------------------------------------------------------------ jobs drawer

fn build_job_row(p: &Rc<Panel>, job_id: &str) -> JobRow {
    let root = kit::hbox(10);

    let icon = gtk::Stack::new();
    icon.set_width_request(22);
    icon.set_valign(gtk::Align::Start);
    let spinner = kit::spinner();
    spinner.set_halign(gtk::Align::Center);
    icon.add_named(&spinner, Some("running"));
    icon.add_named(&kit::icon_styled(icons::CHECK_CIRCLE, &["c-green"]), Some("ok"));
    icon.add_named(&kit::icon_styled(icons::XMARK_CIRCLE, &["c-red"]), Some("failed"));
    root.append(&icon);

    let column = kit::vbox(2);
    column.set_hexpand(true);
    let head = kit::hbox(8);
    let title = kit::ellipsized("", false, &["bb-medium"]);
    head.append(&title);
    let status = kit::label("", &["caption", "mono", "secondary"]);
    head.append(&status);
    let dismiss = kit::icon_button(icons::XMARK_CIRCLE, "");
    dismiss.add_css_class("plain");
    dismiss.add_css_class("tertiary");
    head.append(&dismiss);
    column.append(&head);

    let tail = kit::vbox(4);
    let collapsed = kit::label("", &["caption", "mono", "secondary"]);
    collapsed.set_ellipsize(gtk::pango::EllipsizeMode::End);
    collapsed.set_selectable(true);
    collapsed.set_focusable(false);
    tail.append(&collapsed);

    let expanded_text = kit::label("", &["caption", "mono", "secondary"]);
    expanded_text.set_selectable(true);
    expanded_text.set_focusable(false);
    expanded_text.set_yalign(0.0);
    let expanded = gtk::ScrolledWindow::new();
    expanded.add_css_class("bb-log");
    expanded.set_child(Some(&expanded_text));
    expanded.set_propagate_natural_height(true);
    expanded.set_max_content_height(220);
    expanded.set_overflow(gtk::Overflow::Hidden);
    // Follow the log: stick to the bottom whenever it grows.
    expanded.vadjustment().connect_changed(|adj| adj.set_value(adj.upper() - adj.page_size()));
    tail.append(&expanded);

    let toggle = gtk::Button::new();
    toggle.add_css_class("plain");
    toggle.add_css_class("tint");
    toggle.set_halign(gtk::Align::Start);
    let toggle_content = kit::hbox(4);
    let toggle_icon = kit::icon_styled(CHEVRON_DOWN, &["bb-chevron"]);
    let toggle_text = kit::label("", &["caption"]);
    toggle_content.append(&toggle_icon);
    toggle_content.append(&toggle_text);
    toggle.set_child(Some(&toggle_content));
    tail.append(&toggle);
    column.append(&tail);
    root.append(&column);

    let id = job_id.to_string();
    dismiss.connect_clicked(glib::clone!(
        #[weak]
        p,
        #[strong]
        id,
        move |_| {
            p.jobs.dismiss(&id);
            p.expanded_job_logs.borrow_mut().remove(&id);
            render_jobs(&p);
        }
    ));
    toggle.connect_clicked(glib::clone!(
        #[weak]
        p,
        move |_| {
            {
                let mut expanded = p.expanded_job_logs.borrow_mut();
                if !expanded.remove(&id) {
                    expanded.insert(id.clone());
                }
            }
            render_job(&p, &id);
        }
    ));

    JobRow { root, icon, title, status, tail, collapsed, expanded, expanded_text, toggle, toggle_icon, toggle_text }
}

fn set_text_if_changed(label: &gtk::Label, text: &str) {
    // Re-setting identical text would needlessly drop the user's selection.
    if label.text() != text {
        label.set_text(text);
    }
}

fn update_job_row(p: &Panel, row: &JobRow, job: &BorgBoxJob) {
    let state = match (job.is_terminal(), job.is_failure()) {
        (false, _) => "running",
        (true, true) => "failed",
        (true, false) => "ok",
    };
    row.icon.set_visible_child_name(state);
    row.title.set_text(&format!("{} · {}", job.kind, job.repo.as_deref().unwrap_or("?")));
    row.status.set_text(&job.status);

    let lines = p.jobs.lines(job);
    row.tail.set_visible(!lines.is_empty());
    if lines.is_empty() {
        return;
    }
    let is_expanded = p.expanded_job_logs.borrow().contains(&job.id);
    let has_more = lines.len() > 6;
    row.collapsed.set_visible(!is_expanded);
    row.expanded.set_visible(is_expanded);
    if is_expanded {
        set_text_if_changed(&row.expanded_text, &lines.join("\n"));
        // The adjustment handler runs before the new page size is known;
        // settle on the very bottom once the layout pass is done.
        let adj = row.expanded.vadjustment();
        glib::idle_add_local_once(move || adj.set_value(adj.upper() - adj.page_size()));
    } else {
        let recent = &lines[lines.len().saturating_sub(6)..];
        set_text_if_changed(&row.collapsed, &recent.join("\n"));
    }
    row.toggle.set_visible(has_more || is_expanded);
    row.toggle_icon.set_icon_name(Some(if is_expanded { CHEVRON_UP } else { CHEVRON_DOWN }));
    row.toggle_text.set_text(&if is_expanded {
        "Collapse".to_string()
    } else {
        format!("Show full log ({} lines)", lines.len())
    });
}

/// Refreshes one job row in place (log lines, expand/collapse); anything
/// that can add, remove or reorder jobs goes through `render_jobs`.
fn render_job(p: &Rc<Panel>, job_id: &str) {
    let job = p.jobs.jobs.borrow().get(job_id).cloned();
    let content = p.content.borrow();
    let Some(content) = content.as_ref() else { return };
    if let (Some(job), Some(row)) = (job, content.job_rows.borrow().get(job_id)) {
        update_job_row(p, row, &job);
    }
}

fn render_jobs(p: &Rc<Panel>) {
    let ordered = p.jobs.ordered_jobs();
    let known: Vec<String> = {
        let content = p.content.borrow();
        let Some(content) = content.as_ref() else { return };
        content.job_rows.borrow().keys().cloned().collect()
    };
    // Build missing rows before borrowing the content for the layout pass.
    let fresh: Vec<(String, JobRow)> = ordered
        .iter()
        .filter(|job| !known.contains(&job.id))
        .map(|job| (job.id.clone(), build_job_row(p, &job.id)))
        .collect();

    let content = p.content.borrow();
    let Some(content) = content.as_ref() else { return };
    let mut rows = content.job_rows.borrow_mut();
    for (id, row) in fresh {
        content.jobs.append(&row.root);
        rows.insert(id, row);
    }
    let gone: Vec<String> = rows.keys().filter(|id| !ordered.iter().any(|j| &j.id == *id)).cloned().collect();
    for id in gone {
        if let Some(row) = rows.remove(&id) {
            content.jobs.remove(&row.root);
        }
    }
    content.jobs_empty.set_visible(ordered.is_empty());
    let mut previous: gtk::Widget = content.jobs_empty.clone().upcast();
    for job in &ordered {
        let Some(row) = rows.get(&job.id) else { continue };
        content.jobs.reorder_child_after(&row.root, Some(&previous));
        update_job_row(p, row, job);
        previous = row.root.clone().upcast();
    }
}

// ------------------------------------------- server entry (no server yet)

/// Host component of a URL (`URLComponents(string:)?.host`).
fn url_host(url: &str) -> String {
    let Some((_, rest)) = url.split_once("://") else { return String::new() };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    if let Some(bracketed) = host_port.strip_prefix('[') {
        return bracketed.split(']').next().unwrap_or("").to_string();
    }
    host_port.split(':').next().unwrap_or("").to_string()
}

fn build_server_form(p: &Rc<Panel>) {
    let header_texts = kit::hbox(10);
    let icon = kit::icon_styled(icons::SERVER, &["tint", "bb-title-icon"]);
    icon.set_valign(gtk::Align::Center);
    header_texts.append(&icon);
    let texts = kit::vbox(2);
    texts.set_hexpand(true);
    texts.append(&kit::label("Configure your BorgBox server", &["title-3"]));
    texts.append(&kit::caption("Paste the daemon URL and API token. We check /health and /info before saving."));
    header_texts.append(&texts);
    let close = close_button(p);
    p.root.append(&header_row(&header_texts, &[close.upcast_ref()]));
    p.root.append(&kit::divider());

    let form = kit::vbox(18);
    form.set_margin_top(20);
    form.set_margin_bottom(20);
    form.set_margin_start(20);
    form.set_margin_end(20);

    let server_group = kit::form_group();
    server_group.set_title("Server");
    let name = adw::EntryRow::builder().title("Name (e.g. Home NAS)").text("My BorgBox server").build();
    let url = adw::EntryRow::builder().title("Daemon URL (http://host:9999)").text("http://hive.local:9999").build();
    url.set_input_hints(gtk::InputHints::NO_SPELLCHECK);
    let token = adw::PasswordEntryRow::builder().title("API token").build();
    server_group.add(&name);
    server_group.add(&url);
    server_group.add(&token);
    form.append(&server_group);

    let ssh_group = kit::form_group();
    let advanced = adw::ExpanderRow::builder().title("SSH (advanced)").build();
    let ssh_host = adw::EntryRow::new();
    ssh_host.set_input_hints(gtk::InputHints::NO_SPELLCHECK);
    let ssh_user = adw::EntryRow::builder().title("SSH user").text("borg").build();
    let ssh_port = adw::EntryRow::builder().title("SSH port").text("22").build();
    ssh_port.set_input_purpose(gtk::InputPurpose::Digits);
    advanced.add_row(&ssh_host);
    advanced.add_row(&ssh_user);
    advanced.add_row(&ssh_port);
    ssh_group.add(&advanced);
    form.append(&ssh_group);

    let error = kit::error_label();
    error.set_margin_start(12);
    kit::set_error(&error, p.error.borrow().as_deref());
    form.append(&error);
    p.root.append(&kit::scrolled(&form));

    let validating = kit::hbox(6);
    validating.append(&kit::spinner());
    validating.append(&kit::label("Validating…", &["caption", "secondary"]));
    validating.set_visible(false);
    let save = kit::primary_button("Save and connect");
    let footer = kit::button_row(Some(validating.upcast_ref()), &[&save]);
    footer.set_margin_top(14);
    footer.set_margin_bottom(14);
    footer.set_margin_start(20);
    footer.set_margin_end(20);
    p.root.append(&footer);
    p.dialog.set_default_widget(Some(&save));
    *p.form_error.borrow_mut() = Some(error);

    let busy = Rc::new(Cell::new(false));
    // Derived state: the auto-detected SSH host hint and "Save" enabled.
    let update = Rc::new(glib::clone!(
        #[weak]
        name,
        #[weak]
        url,
        #[weak]
        token,
        #[weak]
        ssh_host,
        #[weak]
        save,
        #[strong]
        busy,
        move || {
            let auto = url_host(url.text().trim());
            ssh_host.set_title(&if auto.is_empty() {
                "SSH host (empty = auto from URL)".to_string()
            } else {
                format!("SSH host (empty = auto from URL: {auto})")
            });
            let can_save =
                !name.text().trim().is_empty() && !url.text().trim().is_empty() && !token.text().trim().is_empty();
            save.set_sensitive(!busy.get() && can_save);
        }
    ));
    update();
    for row in [&name, &url] {
        row.connect_changed(glib::clone!(
            #[strong]
            update,
            move |_| update()
        ));
    }
    token.connect_changed(glib::clone!(
        #[strong]
        update,
        move |_| update()
    ));

    save.connect_clicked(glib::clone!(
        #[weak]
        p,
        #[weak]
        validating,
        #[weak]
        token,
        move |_| {
            set_error(&p, None);
            let clean_url = url.text().trim().to_string();
            let clean_token = token.text().trim().to_string();
            let clean_name = name.text().trim().to_string();
            let typed_host = ssh_host.text().trim().to_string();
            let host = if typed_host.is_empty() { url_host(&clean_url) } else { typed_host };
            let port = ssh_port.text().trim().parse::<i64>().unwrap_or(22);
            let typed_user = ssh_user.text().trim().to_string();
            let user = if typed_user.is_empty() { "borg".to_string() } else { typed_user };
            if host.is_empty() {
                set_error(
                    &p,
                    Some("Could not extract a host from the daemon URL. Set one under SSH (advanced).".to_string()),
                );
                return;
            }

            busy.set(true);
            validating.set_visible(true);
            update();
            let (check_url, check_token) = (clean_url.clone(), clean_token.clone());
            let weak = Rc::downgrade(&p);
            let (busy, update) = (busy.clone(), update.clone());
            kit::bg(
                move || borgbox::health(&check_url).and_then(|()| borgbox::info(&check_url, &check_token).map(|_| ())),
                move |result| {
                    busy.set(false);
                    validating.set_visible(false);
                    update();
                    let Some(p) = weak.upgrade() else { return };
                    if let Err(e) = result {
                        set_error(&p, Some(e));
                        return;
                    }
                    let server = BorgBoxServer {
                        id: Uuid::new_v4(),
                        name: clean_name,
                        daemon_url: clean_url,
                        ssh_host: host,
                        ssh_user: user,
                        ssh_port: port,
                    };
                    // On success the store notification swaps the layout,
                    // which drops the form (and the token typed into it).
                    if let Err(e) = p.app.borgbox.add(server, &clean_token) {
                        set_error(&p, Some(e));
                    }
                },
            );
        }
    ));
}

// ------------------------------------------------------------------ loading

/// Failures of the optional cards (stats, sessions, jobs) only hide the
/// card; log them so a decode bug in one of them still leaves a clue.
fn silent<T>(result: Result<T, String>) -> Option<T> {
    result.map_err(|e| eprintln!("[BorgBoxServerPanel] silent fetch failed: {e}")).ok()
}

fn load_all(p: &Rc<Panel>) {
    let Some(server) = p.server.borrow().clone() else { return };
    p.loading.set(p.loading.get() + 1);
    set_error(p, None);
    render_loading(p);

    let weak = Rc::downgrade(p);
    let fetch_server = server.clone();
    kit::bg(
        move || {
            let s = &fetch_server;
            fn lost<T>(_: Box<dyn std::any::Any + Send>) -> Result<T, String> {
                Err("The request thread stopped unexpectedly.".to_string())
            }
            std::thread::scope(|scope| {
                let stats = scope.spawn(|| borgbox::system_stats(s));
                let sessions = scope.spawn(|| borgbox::sessions(s));
                let jobs = scope.spawn(|| borgbox::list_jobs(s));
                let repos = borgbox::list_repos(s);
                (
                    stats.join().unwrap_or_else(lost),
                    repos,
                    sessions.join().unwrap_or_else(lost),
                    jobs.join().unwrap_or_else(lost),
                )
            })
        },
        move |(stats, repos, sessions, jobs)| {
            let Some(p) = weak.upgrade() else { return };
            p.loading.set(p.loading.get().saturating_sub(1));
            if p.server.borrow().as_ref().map(|s| s.id) != Some(server.id) {
                render_loading(&p);
                return;
            }
            // `repos` is the critical fetch — its failure is the only one
            // that surfaces in red. The rest are optional "nice to have"
            // endpoints: their cards just stay hidden/empty.
            *p.stats.borrow_mut() = silent(stats);
            *p.sessions.borrow_mut() = silent(sessions).unwrap_or_default();
            match repos {
                Ok(repos) => *p.repos.borrow_mut() = repos,
                Err(e) => set_error(&p, Some(e)),
            }
            if let Some(jobs) = silent(jobs) {
                p.jobs.hydrate(jobs, &server);
            }
            render_all(&p);
        },
    );
}

// --------------------------------------------------------- import detection

/// Single source of truth for "does this local repository point at that
/// remote BorgBox repo?". Prefers the daemon's `ssh_url` when available
/// (v0.3.0+), falls back to host + path matching for older daemons.
fn repo_matches_remote(local: &Repository, remote: &BorgBoxRemoteRepo, server: &BorgBoxServer) -> bool {
    if remote.ssh_url.as_ref().is_some_and(|url| !url.is_empty() && &local.url == url) {
        return true;
    }
    local.url.contains(&server.ssh_host) && local.url.ends_with(&remote.path)
}

fn is_already_imported(p: &Panel, remote: &BorgBoxRemoteRepo) -> bool {
    let Some(server) = p.app.borgbox.first() else { return false };
    p.app.repos.repositories().iter().any(|local| repo_matches_remote(local, remote, &server))
}

// ---------------------------------------------------- passphrase resolution

/// Resolves the borg passphrase for a given remote repo, in this order:
/// 1. Per-panel session cache (in-memory, wiped when the panel closes).
/// 2. BorgBox-namespaced keyring entry (persisted across launches — saved
///    the first time the user types the passphrase into the prompt).
/// 3. Local repository whose SSH URL points at the same daemon host and ends
///    in `/<repo_name>` — read non-interactively from the keyring.
/// Returns `None` only if all three miss; the caller falls back to a prompt.
fn resolve_local_passphrase(p: &Panel, repo_name: &str, server: &BorgBoxServer) -> Option<String> {
    if let Some(cached) = p.session_passphrases.borrow().get(repo_name) {
        return Some(cached.clone());
    }
    if let Some(stored) = keyring::borgbox_repo_passphrase(&server.id, repo_name) {
        return Some(stored);
    }
    let host = url_host(&server.daemon_url);
    let suffix = format!("/{repo_name}");
    let local = p.app.repos.repositories().into_iter().find(|r| r.url.contains(&host) && r.url.ends_with(&suffix))?;
    keyring::passphrase(&local.id)
}

fn with_passphrase(p: &Rc<Panel>, repo: &str, action: &str, server: &BorgBoxServer, run: impl Fn(&Rc<Panel>, String) + 'static) {
    if let Some(passphrase) = resolve_local_passphrase(p, repo, server) {
        p.session_passphrases.borrow_mut().insert(repo.to_string(), passphrase.clone());
        run(p, passphrase);
        return;
    }
    let weak = Rc::downgrade(p);
    let (repo_name, server_id) = (repo.to_string(), server.id);
    borgbox_passphrase::present(&p.root, repo, action, move |entered| {
        let Some(p) = weak.upgrade() else { return };
        p.session_passphrases.borrow_mut().insert(repo_name.clone(), entered.clone());
        // Persist so the next submenu / next app launch doesn't ask again.
        // It is cleared when the repo is deleted from the server.
        let _ = keyring::set_borgbox_repo_passphrase(&entered, &server_id, &repo_name);
        run(&p, entered);
    });
}

// ------------------------------------------------------------------ actions

fn install_actions(p: &Rc<Panel>) {
    let add = |name: &str, f: fn(&Rc<Panel>, &str, &BorgBoxServer)| {
        let action = gio::SimpleAction::new(name, Some(glib::VariantTy::STRING));
        let weak = Rc::downgrade(p);
        action.connect_activate(move |_, target| {
            let (Some(p), Some(repo)) = (weak.upgrade(), target.and_then(|t| t.get::<String>())) else { return };
            let Some(server) = p.server.borrow().clone() else { return };
            f(&p, &repo, &server);
        });
        p.actions.add_action(&action);
    };
    add("archives", start_archives);
    add("alerts", |p, repo, server| borgbox_alerts::present(&p.root, &p.app, server, repo));
    add("check", start_check);
    add("prune", start_prune);
    add("compact", start_compact);
    add("import", |p, repo, server| open_import(p, repo, server, borgbox_import::Mode::ExistingKey));
    add("register", |p, repo, server| open_import(p, repo, server, borgbox_import::Mode::Register));
    add("append-only-on", |p, repo, server| run_set_append_only(p, repo, true, server));
    add("append-only-off", confirm_append_only_disable);
    add("break-lock", run_break_lock);
    add("delete", confirm_delete);

    let change = gio::SimpleAction::new("change-server", None);
    let weak = Rc::downgrade(p);
    change.connect_activate(move |_, _| {
        if let Some(p) = weak.upgrade() {
            confirm_change_server(&p);
        }
    });
    p.actions.add_action(&change);
}

/// Maps the bare "HTTP <code>" failures (no API error body) the daemon
/// returns for routes it doesn't know.
fn is_http(error: &str, code: u16) -> bool {
    error.starts_with(&format!("BorgBox returned HTTP {code}:"))
}

fn start_job(
    p: &Rc<Panel>,
    repo: &str,
    kind: &'static str,
    server: &BorgBoxServer,
    start: impl FnOnce() -> Result<BorgBoxJobStart, String> + Send + 'static,
) {
    let weak = Rc::downgrade(p);
    let (repo, server) = (repo.to_string(), server.clone());
    kit::bg(start, move |result| {
        let Some(p) = weak.upgrade() else { return };
        match result {
            Ok(job) => {
                p.jobs.register(&job, kind, &repo, &server);
                render_jobs(&p);
            }
            Err(e) if is_http(&e, 404) => {
                set_error(&p, Some(format!("The daemon doesn't implement /repos/{repo}/{kind} yet.")))
            }
            Err(e) => set_error(&p, Some(e)),
        }
    });
}

fn start_check(p: &Rc<Panel>, repo: &str, server: &BorgBoxServer) {
    let (name, srv) = (repo.to_string(), server.clone());
    with_passphrase(p, repo, "Check", server, move |p, passphrase| {
        let (name, srv) = (name.clone(), srv.clone());
        start_job(p, &name.clone(), "check", &srv.clone(), move || {
            borgbox::check(&srv, &name, Some(&passphrase), false, false)
        });
    });
}

fn start_compact(p: &Rc<Panel>, repo: &str, server: &BorgBoxServer) {
    let (name, srv) = (repo.to_string(), server.clone());
    with_passphrase(p, repo, "Compact", server, move |p, passphrase| {
        let (name, srv) = (name.clone(), srv.clone());
        start_job(p, &name.clone(), "compact", &srv.clone(), move || borgbox::compact(&srv, &name, Some(&passphrase)));
    });
}

fn start_prune(p: &Rc<Panel>, repo: &str, server: &BorgBoxServer) {
    let (name, srv) = (repo.to_string(), server.clone());
    with_passphrase(p, repo, "Prune", server, move |p, passphrase| {
        let weak = Rc::downgrade(p);
        let (job_repo, job_server) = (name.clone(), srv.clone());
        borgbox_prune::present(&p.root, &p.app, &srv, &name, Some(passphrase), move |start| {
            if let Some(p) = weak.upgrade() {
                p.jobs.register(&start, "prune", &job_repo, &job_server);
                render_jobs(&p);
            }
        });
    });
}

fn start_archives(p: &Rc<Panel>, repo: &str, server: &BorgBoxServer) {
    let (name, srv) = (repo.to_string(), server.clone());
    with_passphrase(p, repo, "Archives", server, move |p, passphrase| {
        borgbox_archives::present(&p.root, &p.app, &srv, &name, Some(passphrase));
    });
}

/// The sheet currently on top of the window hosting the panel.
fn visible_dialog(p: &Panel) -> Option<adw::Dialog> {
    let window = p.root.root()?;
    if let Some(w) = window.downcast_ref::<adw::ApplicationWindow>() {
        return w.visible_dialog();
    }
    window.downcast_ref::<adw::Window>().and_then(|w| w.visible_dialog())
}

fn open_import(p: &Rc<Panel>, repo: &str, server: &BorgBoxServer, mode: borgbox_import::Mode) {
    let Some(remote) = p.repos.borrow().iter().find(|r| r.name == repo).cloned() else { return };
    borgbox_import::present(&p.root, &p.app, server, &remote.name, &remote.path, remote.ssh_url.as_deref(), mode);
    if mode != borgbox_import::Mode::Register {
        return;
    }
    // After a successful register the daemon's /repos list shows the repo as
    // `registered: true`; refresh once the sheet goes away to reflect that.
    if let Some(sheet) = visible_dialog(p).filter(|d| d != &p.dialog) {
        sheet.connect_closed(glib::clone!(
            #[weak]
            p,
            move |_| load_all(&p)
        ));
    }
}

fn run_set_append_only(p: &Rc<Panel>, repo: &str, append_only: bool, server: &BorgBoxServer) {
    let weak = Rc::downgrade(p);
    let (name, srv) = (repo.to_string(), server.clone());
    let repo = repo.to_string();
    kit::bg(
        move || borgbox::update_repo(&srv, &name, append_only),
        move |result| {
            let Some(p) = weak.upgrade() else { return };
            match result {
                Ok(updated) => {
                    if let Some(slot) = p.repos.borrow_mut().iter_mut().find(|r| r.name == repo) {
                        *slot = updated;
                    }
                    render_repos(&p);
                }
                Err(e) if is_http(&e, 409) => set_error(
                    &p,
                    Some(format!(
                        "The daemon doesn't have a registered key for {repo}, so it won't toggle append-only on it."
                    )),
                ),
                Err(e) => set_error(&p, Some(e)),
            }
        },
    );
}

fn confirm_append_only_disable(p: &Rc<Panel>, repo: &str, server: &BorgBoxServer) {
    let weak = Rc::downgrade(p);
    let (name, srv) = (repo.to_string(), server.clone());
    kit::confirm(
        &p.root,
        &format!("Disable append-only on {repo}?"),
        "Clients will regain the ability to prune and delete archives in this repo. Only turn this off if you trust the key that's writing to it.",
        "Disable append-only",
        "Cancel",
        true,
        move || {
            if let Some(p) = weak.upgrade() {
                run_set_append_only(&p, &name, false, &srv);
            }
        },
    );
}

fn run_break_lock(p: &Rc<Panel>, repo: &str, server: &BorgBoxServer) {
    let weak = Rc::downgrade(p);
    let (name, srv) = (repo.to_string(), server.clone());
    let repo = repo.to_string();
    kit::bg(
        move || borgbox::break_lock(&srv, &name),
        move |result| {
            let Some(p) = weak.upgrade() else { return };
            match result {
                // Refresh since break-lock typically follows a stuck
                // operation; the session list is the easiest thing to
                // visibly reflect the state change.
                Ok(()) => load_all(&p),
                Err(e) if is_http(&e, 404) => {
                    set_error(&p, Some(format!("The daemon doesn't implement /repos/{repo}/break-lock yet.")))
                }
                Err(e) => set_error(&p, Some(e)),
            }
        },
    );
}

fn confirm_delete(p: &Rc<Panel>, repo: &str, server: &BorgBoxServer) {
    let weak = Rc::downgrade(p);
    let (name, srv) = (repo.to_string(), server.clone());
    kit::confirm(
        &p.root,
        &format!("Delete repo {repo}?"),
        "This deletes the repo from the BorgBox daemon. The on-disk data is lost and cannot be recovered.",
        "Delete from server",
        "Cancel",
        true,
        move || {
            if let Some(p) = weak.upgrade() {
                run_delete(&p, &name, &srv);
            }
        },
    );
}

fn run_delete(p: &Rc<Panel>, repo: &str, server: &BorgBoxServer) {
    let weak = Rc::downgrade(p);
    let (name, srv) = (repo.to_string(), server.clone());
    let (repo, server) = (repo.to_string(), server.clone());
    kit::bg(
        move || borgbox::delete_repo(&srv, &name),
        move |result| {
            let Some(p) = weak.upgrade() else { return };
            match result {
                Ok(()) => {
                    keyring::delete_borgbox_repo_passphrase(&server.id, &repo);
                    p.session_passphrases.borrow_mut().remove(&repo);
                    remove_local_counterpart(&p, &repo, &server);
                    load_all(&p);
                }
                Err(e) => set_error(&p, Some(e)),
            }
        },
    );
}

/// Removes any local repository that points at the just-deleted remote, so
/// the sidebar doesn't keep a dangling entry whose URL no longer exists.
fn remove_local_counterpart(p: &Panel, repo_name: &str, server: &BorgBoxServer) {
    let suffix = format!("/{repo_name}");
    for local in p.app.repos.repositories() {
        if local.url.contains(&server.ssh_host) && local.url.ends_with(&suffix) {
            p.app.repos.remove(&local);
        }
    }
}

fn confirm_change_server(p: &Rc<Panel>) {
    let Some(server) = p.server.borrow().clone() else { return };
    let weak = Rc::downgrade(p);
    kit::confirm(
        &p.root,
        "Forget this BorgBox server?",
        "Existing repos stay in your sidebar but keep pointing at this server's SSH host. If the next server is a different machine, their scheduled backups will fail until you reconfigure them.",
        "Forget server",
        "Cancel",
        true,
        move || {
            if let Some(p) = weak.upgrade() {
                p.app.borgbox.remove(&server);
            }
        },
    );
}

// -------------------------------------------------------------- debug hook

/// Development hook, inert unless `LUNA_DEBUG_BORGBOX` is set: a
/// `;`-separated list of `<step>@<delay-ms>` run after the panel opens, so
/// states behind menus (not reachable without clicking) can be exercised.
/// Steps: `<action>:<repo>` (any `bb.*` action: check, prune, compact,
/// archives, alerts, import, register, append-only-on/off, break-lock,
/// delete), `change-server`, `pass:<repo>=<passphrase>` (seed the session
/// cache), `expand` (expand every job log), `scroll[:<fraction>]`, `refresh`, `close`,
/// and, acting on whatever sheet is on top: `tab` (focus next), `type:<text>`
/// (into the focused field), `enter` (activate it), `default` (activate the
/// default button), `respond:<id>` (answer an alert/confirmation).
fn debug_script(p: &Rc<Panel>) {
    let Ok(script) = std::env::var("LUNA_DEBUG_BORGBOX") else { return };
    for step in script.split(';').filter(|s| !s.is_empty()) {
        let (command, delay) = step.rsplit_once('@').unwrap_or((step, "0"));
        let (name, target) = command.split_once(':').unwrap_or((command, ""));
        let (name, target) = (name.to_string(), target.to_string());
        let weak = Rc::downgrade(p);
        kit::after(delay.parse().unwrap_or(0), move || {
            let Some(p) = weak.upgrade() else { return };
            match name.as_str() {
                "pass" => {
                    if let Some((repo, passphrase)) = target.split_once('=') {
                        p.session_passphrases.borrow_mut().insert(repo.to_string(), passphrase.to_string());
                    }
                }
                "expand" => {
                    let ids: Vec<String> = p.jobs.jobs.borrow().keys().cloned().collect();
                    p.expanded_job_logs.borrow_mut().extend(ids);
                    render_jobs(&p);
                }
                "scroll" => {
                    if let Some(content) = p.content.borrow().as_ref() {
                        let adj = content.scroller.vadjustment();
                        let fraction = target.parse::<f64>().unwrap_or(1.0);
                        adj.set_value((adj.upper() - adj.page_size()) * fraction);
                    }
                }
                "refresh" => load_all(&p),
                "close" => {
                    p.dialog.close();
                }
                "tab" | "type" | "enter" => {
                    let Some(window) = p.root.root() else { return };
                    match (name.as_str(), window.focus()) {
                        ("tab", _) => {
                            window.child_focus(gtk::DirectionType::TabForward);
                        }
                        ("type", Some(focus)) => {
                            if let Some(editable) = focus.downcast_ref::<gtk::Editable>() {
                                editable.set_text(&target);
                            }
                        }
                        ("enter", Some(focus)) => {
                            if focus.is::<gtk::Text>() {
                                focus.emit_by_name::<()>("activate", &[]);
                            } else {
                                focus.activate();
                            }
                        }
                        _ => {}
                    }
                }
                "default" => {
                    if let Some(button) = visible_dialog(&p).and_then(|d| d.default_widget()) {
                        button.activate();
                    }
                }
                "respond" => {
                    if let Some(alert) = visible_dialog(&p).and_then(|d| d.downcast::<adw::AlertDialog>().ok()) {
                        alert.emit_by_name::<()>("response", &[&target]);
                        alert.close();
                    }
                }
                "change-server" => p.actions.activate_action("change-server", None),
                action => p.actions.activate_action(action, Some(&target.to_variant())),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: &str, status: &str, started_at: Option<&str>) -> BorgBoxJob {
        BorgBoxJob {
            id: id.to_string(),
            repo: Some("mac".to_string()),
            kind: "check".to_string(),
            status: status.to_string(),
            started_at: started_at.map(str::to_string),
            finished_at: None,
            exit_code: None,
            log_tail: None,
        }
    }

    fn event(kind: &str, line: Option<&str>, status: Option<&str>, exit_code: Option<i64>) -> StreamUpdate {
        StreamUpdate::Event(BorgBoxJobStreamEvent {
            event_type: kind.to_string(),
            line: line.map(str::to_string),
            status: status.map(str::to_string),
            exit_code,
        })
    }

    fn store_with(jobs: Vec<BorgBoxJob>) -> JobsStore {
        let (tx, _rx) = async_channel::unbounded();
        let store = JobsStore::new(tx);
        for job in jobs {
            store.logs.borrow_mut().insert(job.id.clone(), job.log_tail.clone().unwrap_or_default());
            store.jobs.borrow_mut().insert(job.id.clone(), job);
        }
        store
    }

    #[test]
    fn hosts_and_uptime() {
        assert_eq!(url_host("http://hive.local:9999"), "hive.local");
        assert_eq!(url_host("https://user:pw@nas.example/borgbox?x=1"), "nas.example");
        assert_eq!(url_host("http://[::1]:9000/x"), "::1");
        assert_eq!(url_host("hive.local:9999"), "");
        assert_eq!(format_uptime(59), "0m");
        assert_eq!(format_uptime(3 * 3600 + 120), "3h 2m");
        assert_eq!(format_uptime(6 * 86400 + 12 * 3600 + 59), "6d 12h");
    }

    #[test]
    fn jobs_are_ordered_active_first_then_newest() {
        let store = store_with(vec![
            job("old", "done", Some("2026-11-01T00:00:00Z")),
            job("new", "error", Some("2026-11-02T00:00:00Z")),
            job("live", "running", Some("2026-10-01T00:00:00Z")),
            job("fresh", "queued", None),
        ]);
        let order: Vec<String> = store.ordered_jobs().into_iter().map(|j| j.id).collect();
        assert_eq!(order, ["live", "fresh", "new", "old"]);
    }

    #[test]
    fn stream_updates_are_applied() {
        let store = store_with(vec![job("j", "queued", None)]);
        let cancel = Arc::new(AtomicBool::new(false));
        let send = |update| store.apply(StreamMessage { job_id: "j".to_string(), cancel: cancel.clone(), update });

        for i in 0..(MAX_LOG_LINES + 20) {
            send(event("log", Some(&format!("line {i}")), None, None));
        }
        send(event("log", Some(""), None, None));
        let current = store.jobs.borrow()["j"].clone();
        let lines = store.lines(&current);
        assert_eq!(lines.len(), MAX_LOG_LINES);
        assert_eq!(lines.first().map(String::as_str), Some("line 20"));

        send(event("status", None, Some(""), None));
        assert_eq!(store.jobs.borrow()["j"].status, "queued");
        send(event("status", None, Some("done"), Some(0)));
        send(event("end", None, Some("done"), Some(0)));
        assert_eq!(store.jobs.borrow()["j"].status, "done");
        assert_eq!(store.jobs.borrow()["j"].exit_code, Some(0));

        let mut finished = job("j", "done", Some("2026-11-02T00:00:00Z"));
        finished.log_tail = Some(vec!["snapshot".to_string()]);
        send(StreamUpdate::Finished(Some(finished)));
        let current = store.jobs.borrow()["j"].clone();
        assert_eq!(current.started_at.as_deref(), Some("2026-11-02T00:00:00Z"));
        // The live buffer still wins over the snapshot tail.
        assert_eq!(store.lines(&current).len(), MAX_LOG_LINES);

        // Updates from a cancelled stream never resurrect a dismissed job.
        store.dismiss("j");
        cancel.store(true, Ordering::Relaxed);
        send(StreamUpdate::Finished(Some(job("j", "done", None))));
        send(event("log", Some("late"), None, None));
        assert!(store.jobs.borrow().is_empty() && store.logs.borrow().is_empty());
    }
}
