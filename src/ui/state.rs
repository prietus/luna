//! Observable application state shared by every view (the counterparts of
//! BorgMac's `RepositoryStore`, `BorgBoxServerStore`, `BackupRunStatusStore`
//! and `ManualBackupStore`). Main-thread only.
use crate::core::borg::{self, Cancel};
use crate::core::models::{BackupRunStatus, BorgBoxServer, Repository};
use crate::core::{keyring, notify, schedule, snapshot, status, store};
use chrono::{DateTime, Local};
use gtk::glib;
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use uuid::Uuid;

// ------------------------------------------------------------------ Signal

/// Minimal change-notification list. Handlers run on the main thread.
#[derive(Default)]
pub struct Signal {
    handlers: RefCell<Vec<(u64, Rc<dyn Fn()>)>>,
    next_id: Cell<u64>,
}

impl Signal {
    pub fn connect(&self, f: impl Fn() + 'static) -> u64 {
        let id = self.next_id.get() + 1;
        self.next_id.set(id);
        self.handlers.borrow_mut().push((id, Rc::new(f)));
        id
    }

    pub fn disconnect(&self, id: u64) {
        self.handlers.borrow_mut().retain(|(i, _)| *i != id);
    }

    pub fn emit(&self) {
        let snapshot: Vec<Rc<dyn Fn()>> = self.handlers.borrow().iter().map(|(_, f)| f.clone()).collect();
        for f in snapshot {
            f();
        }
    }
}

/// Connects `f` to `signal` for as long as `widget` is alive; the handler is
/// dropped when the widget is destroyed. `signal` must be a field reachable
/// from an `Rc<AppState>` (pass a closure that picks it).
pub fn connect_while_alive(
    app: &Rc<AppState>,
    pick: impl Fn(&AppState) -> &Signal + 'static,
    widget: &impl IsA<gtk::Widget>,
    f: impl Fn() + 'static,
) {
    let id = pick(app).connect(f);
    let weak = Rc::downgrade(app);
    widget.connect_destroy(move |_| {
        if let Some(app) = weak.upgrade() {
            pick(&app).disconnect(id);
        }
    });
}

// --------------------------------------------------------- RepositoryStore

#[derive(Default)]
pub struct RepositoryStore {
    repositories: RefCell<Vec<Repository>>,
    pub changed: Signal,
}

impl RepositoryStore {
    fn load() -> Self {
        RepositoryStore { repositories: RefCell::new(store::load_repositories()), changed: Signal::default() }
    }

    pub fn repositories(&self) -> Vec<Repository> {
        self.repositories.borrow().clone()
    }

    pub fn get(&self, id: &Uuid) -> Option<Repository> {
        self.repositories.borrow().iter().find(|r| &r.id == id).cloned()
    }

    fn persist(&self) {
        store::save_repositories(&self.repositories.borrow());
        self.changed.emit();
    }

    /// Stores the passphrase in the keyring and adds the repository.
    pub fn add(&self, repo: Repository, passphrase: &str) -> Result<(), String> {
        keyring::set_passphrase(passphrase, &repo.id)?;
        self.repositories.borrow_mut().push(repo);
        self.persist();
        Ok(())
    }

    /// Adds a repository whose passphrase the caller already stored.
    pub fn add_without_passphrase(&self, repo: Repository) {
        self.repositories.borrow_mut().push(repo);
        self.persist();
    }

    pub fn remove(&self, repo: &Repository) {
        self.repositories.borrow_mut().retain(|r| r.id != repo.id);
        keyring::delete_passphrase(&repo.id);
        borg::forget(&repo.id);
        schedule::uninstall_all(&repo.id);
        for sched in &repo.schedules {
            status::delete(&sched.id);
        }
        self.persist();
    }

    /// Replaces an existing repository (schedule edits…). No keyring side effects.
    pub fn update(&self, repo: Repository) {
        {
            let mut list = self.repositories.borrow_mut();
            let Some(slot) = list.iter_mut().find(|r| r.id == repo.id) else { return };
            *slot = repo;
        }
        self.persist();
    }
}

// --------------------------------------------------------- BorgBoxServerStore

#[derive(Default)]
pub struct BorgBoxServerStore {
    servers: RefCell<Vec<BorgBoxServer>>,
    pub changed: Signal,
}

impl BorgBoxServerStore {
    fn load() -> Self {
        BorgBoxServerStore { servers: RefCell::new(store::load_borgbox_servers()), changed: Signal::default() }
    }

    pub fn servers(&self) -> Vec<BorgBoxServer> {
        self.servers.borrow().clone()
    }

    /// The "current" server (v1 acts on the first one).
    pub fn first(&self) -> Option<BorgBoxServer> {
        self.servers.borrow().first().cloned()
    }

    pub fn add(&self, server: BorgBoxServer, token: &str) -> Result<(), String> {
        keyring::set_borgbox_token(token, &server.id)?;
        self.servers.borrow_mut().push(server);
        store::save_borgbox_servers(&self.servers.borrow());
        self.changed.emit();
        Ok(())
    }

    pub fn remove(&self, server: &BorgBoxServer) {
        self.servers.borrow_mut().retain(|s| s.id != server.id);
        keyring::delete_borgbox_token(&server.id);
        store::save_borgbox_servers(&self.servers.borrow());
        self.changed.emit();
    }
}

// ------------------------------------------------------------- StatusStore

/// Polls the status dir on an adaptive cadence: 60 s when idle, 2 s while
/// any schedule is mid-run so progress bars follow borg closely.
pub struct StatusStore {
    statuses: RefCell<HashMap<Uuid, BackupRunStatus>>,
    pub changed: Signal,
    timer: RefCell<Option<glib::SourceId>>,
    interval: Cell<u32>,
}

const IDLE_INTERVAL: u32 = 60;
const RUNNING_INTERVAL: u32 = 2;

impl StatusStore {
    fn new() -> Self {
        StatusStore {
            statuses: RefCell::new(HashMap::new()),
            changed: Signal::default(),
            timer: RefCell::new(None),
            interval: Cell::new(0),
        }
    }

    pub fn statuses(&self) -> HashMap<Uuid, BackupRunStatus> {
        self.statuses.borrow().clone()
    }

    pub fn status(&self, schedule_id: &Uuid) -> Option<BackupRunStatus> {
        self.statuses.borrow().get(schedule_id).cloned()
    }

    pub fn any_running(&self) -> bool {
        self.statuses.borrow().values().any(|s| s.running)
    }
}

/// Re-reads every status file; emits `changed` (and refreshes the status-bar
/// snapshot) when anything moved. Call after kickstarting or cancelling a
/// schedule to pick the change up without waiting for the next poll.
pub fn refresh_statuses(app: &Rc<AppState>) {
    let next = status::read_all();
    let moved = *app.statuses.statuses.borrow() != next;
    if moved {
        *app.statuses.statuses.borrow_mut() = next.clone();
        snapshot::refresh_schedules(&app.repos.repositories(), &next);
        app.statuses.changed.emit();
    }
    let desired = if next.values().any(|s| s.running) { RUNNING_INTERVAL } else { IDLE_INTERVAL };
    if desired != app.statuses.interval.get() {
        schedule_status_timer(app, desired);
    }
}

/// `refresh_statuses` now and again shortly after — a freshly kickstarted
/// runner needs a moment before it writes `running: true`.
pub fn refresh_statuses_soon(app: &Rc<AppState>) {
    refresh_statuses(app);
    for delay_ms in [700u64, 2000, 5000] {
        let weak = Rc::downgrade(app);
        glib::timeout_add_local_once(std::time::Duration::from_millis(delay_ms), move || {
            if let Some(app) = weak.upgrade() {
                refresh_statuses(&app);
            }
        });
    }
}

fn schedule_status_timer(app: &Rc<AppState>, seconds: u32) {
    if let Some(old) = app.statuses.timer.borrow_mut().take() {
        old.remove();
    }
    app.statuses.interval.set(seconds);
    let weak = Rc::downgrade(app);
    let id = glib::timeout_add_seconds_local(seconds, move || match weak.upgrade() {
        Some(app) => {
            // `refresh_statuses` may replace this very timer; never remove
            // the source from inside its own callback.
            glib::idle_add_local_once(move || refresh_statuses(&app));
            glib::ControlFlow::Continue
        }
        None => glib::ControlFlow::Break,
    });
    *app.statuses.timer.borrow_mut() = Some(id);
}

// ------------------------------------------------------- ManualBackupStore

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManualStatus {
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

/// An in-flight manual backup. Lives in memory only — if the app quits the
/// backup dies with it.
#[derive(Debug, Clone, PartialEq)]
pub struct ManualBackupJob {
    pub id: Uuid,
    pub repo_id: Uuid,
    pub repo_name: String,
    pub archive_name: String,
    pub started_at: DateTime<Local>,
    pub status: ManualStatus,
    pub error_message: Option<String>,
}

impl ManualBackupJob {
    pub fn is_terminal(&self) -> bool {
        self.status != ManualStatus::Running
    }
}

#[derive(Default)]
pub struct ManualBackupStore {
    jobs: RefCell<Vec<ManualBackupJob>>,
    cancels: RefCell<HashMap<Uuid, Cancel>>,
    pub changed: Signal,
}

impl ManualBackupStore {
    pub fn jobs(&self) -> Vec<ManualBackupJob> {
        self.jobs.borrow().clone()
    }

    /// Newest-first, running above terminal.
    pub fn ordered_jobs(&self) -> Vec<ManualBackupJob> {
        let mut jobs = self.jobs();
        jobs.sort_by(|a, b| a.is_terminal().cmp(&b.is_terminal()).then(b.started_at.cmp(&a.started_at)));
        jobs
    }

    pub fn is_running_for(&self, repo_id: &Uuid) -> bool {
        self.jobs.borrow().iter().any(|j| &j.repo_id == repo_id && !j.is_terminal())
    }

    /// Sends SIGTERM to the underlying borg process. The job stays in the
    /// list (as cancelled) until dismissed.
    pub fn cancel(&self, job_id: &Uuid) {
        if let Some(c) = self.cancels.borrow().get(job_id) {
            c.cancel();
        }
    }

    /// Removes a terminal job from the list. No-op for running jobs.
    pub fn dismiss(&self, job_id: &Uuid) {
        let removed = {
            let mut jobs = self.jobs.borrow_mut();
            match jobs.iter().position(|j| &j.id == job_id && j.is_terminal()) {
                Some(idx) => {
                    jobs.remove(idx);
                    true
                }
                None => false,
            }
        };
        if removed {
            self.cancels.borrow_mut().remove(job_id);
            self.changed.emit();
        }
    }
}

/// Kicks off a manual backup that keeps running after the launching sheet is
/// closed. `on_completed` fires on success (on the main thread).
pub fn start_manual_backup(
    app: &Rc<AppState>,
    repo: &Repository,
    archive_name: &str,
    paths: Vec<String>,
    excludes: Vec<String>,
    on_completed: impl Fn() + 'static,
) -> Uuid {
    let job = ManualBackupJob {
        id: Uuid::new_v4(),
        repo_id: repo.id,
        repo_name: repo.name.clone(),
        archive_name: archive_name.to_string(),
        started_at: Local::now(),
        status: ManualStatus::Running,
        error_message: None,
    };
    let job_id = job.id;
    let cancel = Cancel::new();
    app.manual.jobs.borrow_mut().push(job);
    app.manual.cancels.borrow_mut().insert(job_id, cancel.clone());
    app.manual.changed.emit();

    let (repo_bg, name_bg) = (repo.clone(), archive_name.to_string());
    let (repo_name, archive) = (repo.name.clone(), archive_name.to_string());
    let weak = Rc::downgrade(app);
    let cancel_done = cancel.clone();
    super::kit::bg(
        move || borg::create_backup(&repo_bg, &name_bg, &paths, &excludes, Some(&cancel)),
        move |result| {
            let (status, error) = match result {
                Ok(()) => (ManualStatus::Succeeded, None),
                Err(_) if cancel_done.is_cancelled() => (ManualStatus::Cancelled, None),
                Err(e) => (ManualStatus::Failed, Some(e)),
            };
            if let Some(app) = weak.upgrade() {
                if let Some(j) = app.manual.jobs.borrow_mut().iter_mut().find(|j| j.id == job_id) {
                    j.status = status;
                    j.error_message = error.clone();
                }
                app.manual.cancels.borrow_mut().remove(&job_id);
                app.manual.changed.emit();
            }
            match status {
                ManualStatus::Succeeded => {
                    notify::post_success(&repo_name, &archive);
                    on_completed();
                }
                ManualStatus::Failed => {
                    notify::send("Manual backup failed", &repo_name, error.as_deref().unwrap_or(""), true)
                }
                _ => {}
            }
        },
    );
    job_id
}

// ---------------------------------------------------------------- AppState

pub struct AppState {
    pub application: adw::Application,
    pub repos: RepositoryStore,
    pub borgbox: BorgBoxServerStore,
    pub statuses: StatusStore,
    pub manual: ManualBackupStore,
    /// Set by callers (menu, tray) to ask the main window to focus a
    /// repository; the window clears it after applying the selection.
    pub pending_selection: RefCell<Option<Uuid>>,
    pub selection_requested: Signal,
    /// True once the tray icon is registered: the app then keeps running in
    /// the background after its windows are closed.
    pub tray_active: Cell<bool>,
}

impl AppState {
    pub fn new(application: &adw::Application) -> Rc<AppState> {
        let app = Rc::new(AppState {
            application: application.clone(),
            repos: RepositoryStore::load(),
            borgbox: BorgBoxServerStore::load(),
            statuses: StatusStore::new(),
            manual: ManualBackupStore::default(),
            pending_selection: RefCell::new(None),
            selection_requested: Signal::default(),
            tray_active: Cell::new(false),
        });
        *app.statuses.statuses.borrow_mut() = status::read_all();
        snapshot::refresh_schedules(&app.repos.repositories(), &app.statuses.statuses());
        let desired = if app.statuses.any_running() { RUNNING_INTERVAL } else { IDLE_INTERVAL };
        schedule_status_timer(&app, desired);
        // Keep the status-bar snapshot's "next backup" in sync with edits.
        let weak = Rc::downgrade(&app);
        app.repos.changed.connect(move || {
            if let Some(app) = weak.upgrade() {
                snapshot::refresh_schedules(&app.repos.repositories(), &app.statuses.statuses());
            }
        });
        app
    }

    /// Asks the main window to select `repo_id` (and shows the window).
    pub fn request_selection(&self, repo_id: Uuid) {
        *self.pending_selection.borrow_mut() = Some(repo_id);
        self.selection_requested.emit();
    }
}
