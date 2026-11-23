//! StatusNotifierItem tray icon (BorgMac's `MenuBarExtra("BorgMac",
//! systemImage: "shippingbox.fill")`). Left click toggles the popover
//! (`menu.rs`); the context menu is a compact text version of it.
//!
//! ksni runs the item on its own thread. That thread never touches GTK or
//! `AppState`: it renders from a `Send` snapshot the main thread pushes to
//! it, and reports clicks back as `Command`s over a channel.
use super::state::{AppState, refresh_statuses_soon};
use super::{borg_wizard, borgbase_wizard, borgbox_wizard, kit, menu, window};
use crate::core::snapshot::{self, Health};
use crate::core::{format, paths, schedule};
use adw::prelude::*;
use chrono::Local;
use gtk::glib;
use ksni::blocking::TrayMethods;
use ksni::menu::{MenuItem, StandardItem, SubMenu};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;
use uuid::Uuid;

#[derive(Debug, Clone, Copy)]
enum Command {
    ToggleMenu,
    Open,
    OpenRepo(Uuid),
    RunNow(Uuid, Uuid),
    Stop(Uuid, Uuid),
    NewOnBorgBase,
    NewOnBorgBox,
    NewBorgRepository,
    Quit,
}

/// One context-menu entry: a schedule, or a repository without schedules.
#[derive(Debug, Clone, PartialEq)]
struct Row {
    repo_id: Uuid,
    schedule_id: Option<Uuid>,
    label: String,
    running: bool,
}

/// Everything the tray thread displays.
#[derive(Debug, Clone, PartialEq)]
struct TrayData {
    rows: Vec<Row>,
    /// "OK" / "running 42%" / "error", appended to the item title.
    state: String,
    /// Tooltip body: running and failed schedules, then the next backup.
    description: String,
}

struct BorgTray {
    data: TrayData,
    commands: async_channel::Sender<Command>,
    icon_name: String,
    icon_theme_path: String,
    icon_pixmap: Vec<ksni::Icon>,
}

impl BorgTray {
    fn send(&self, command: Command) {
        // Unbounded channel: never blocks the tray thread.
        let _ = self.commands.try_send(command);
    }
}

/// dbusmenu treats `_` as the mnemonic marker.
fn menu_label(text: &str) -> String {
    text.replace('_', "__")
}

fn item(label: &str, command: Command) -> MenuItem<BorgTray> {
    StandardItem {
        label: menu_label(label),
        activate: Box::new(move |tray: &mut BorgTray| tray.send(command)),
        ..Default::default()
    }
    .into()
}

impl ksni::Tray for BorgTray {
    fn id(&self) -> String {
        paths::APP_ID.to_string()
    }

    fn title(&self) -> String {
        format!("{} — {}", paths::APP_NAME, self.data.state)
    }

    fn icon_name(&self) -> String {
        self.icon_name.clone()
    }

    fn icon_theme_path(&self) -> String {
        self.icon_theme_path.clone()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.icon_pixmap.clone()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            icon_name: self.icon_name.clone(),
            icon_pixmap: vec![],
            title: self.title(),
            description: self.data.description.clone(),
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(Command::ToggleMenu);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items: Vec<MenuItem<Self>> = vec![];
        for row in &self.data.rows {
            let repo_id = row.repo_id;
            match row.schedule_id {
                Some(schedule_id) => {
                    let action = if row.running {
                        item("Stop backup", Command::Stop(repo_id, schedule_id))
                    } else {
                        item("Run now", Command::RunNow(repo_id, schedule_id))
                    };
                    items.push(
                        SubMenu {
                            label: menu_label(&row.label),
                            submenu: vec![
                                action,
                                item(&format!("Show in {}", paths::APP_NAME), Command::OpenRepo(repo_id)),
                            ],
                            ..Default::default()
                        }
                        .into(),
                    );
                }
                None => items.push(item(&row.label, Command::OpenRepo(repo_id))),
            }
        }
        if !items.is_empty() {
            items.push(MenuItem::Separator);
        }
        items.push(item(&format!("Open {}", paths::APP_NAME), Command::Open));
        items.push(item("New on BorgBase…", Command::NewOnBorgBase));
        items.push(item("New on BorgBox…", Command::NewOnBorgBox));
        items.push(item("New Borg repository (local or SSH)…", Command::NewBorgRepository));
        items.push(MenuItem::Separator);
        items.push(item("Quit", Command::Quit));
        items
    }
}

// ------------------------------------------------------------------ snapshot

fn percent(ratio: f64) -> String {
    format!("{:.0}%", ratio * 100.0)
}

/// Main thread: condenses the stores into what the tray thread shows.
fn tray_data(app: &AppState) -> TrayData {
    let repositories = app.repos.repositories();
    let statuses = app.statuses.statuses();
    let (next, health) = snapshot::summarize_schedules(&repositories, &statuses);

    let mut rows = vec![];
    let (mut running_lines, mut failed_lines) = (vec![], vec![]);
    let mut running_ratio: Option<f64> = None;
    for repo in &repositories {
        if repo.schedules.is_empty() {
            rows.push(Row { repo_id: repo.id, schedule_id: None, label: repo.name.clone(), running: false });
        }
        for sched in &repo.schedules {
            let name = format!("{} / {}", repo.name, sched.display_name());
            let status = statuses.get(&sched.id);
            let running = status.map(|s| s.running).unwrap_or(false);
            let ratio = status.and_then(|s| s.progress_ratio());
            let last = match status {
                Some(_) if running => {
                    running_ratio = running_ratio.or(ratio);
                    let text = match ratio {
                        Some(r) => format!("running {}", percent(r)),
                        None => "running".to_string(),
                    };
                    running_lines.push(format!("Running: {name}{}", ratio.map(|r| format!(" — {}", percent(r))).unwrap_or_default()));
                    text
                }
                Some(s) if s.last_error.is_some() => {
                    let first_line = s.last_error.as_deref().and_then(|e| e.lines().next()).unwrap_or("");
                    failed_lines.push(format!("Failed: {name}: {first_line}"));
                    match s.last_run {
                        Some(at) => format!("failed {}", format::relative_short(&at)),
                        None => "failed".to_string(),
                    }
                }
                Some(s) => match s.last_run.or(s.last_success) {
                    Some(at) => format!("OK {}", format::relative_short(&at)),
                    None => "never run".to_string(),
                },
                None => "never run".to_string(),
            };
            let mut label = format!("{name} — {last}");
            if let Some(next_run) = schedule::next_fire_date(sched, Local::now()) {
                label += &format!(" · next {}", format::relative_short(&next_run));
            }
            rows.push(Row { repo_id: repo.id, schedule_id: Some(sched.id), label, running });
        }
    }

    let state = match health {
        Health::Error => "error".to_string(),
        Health::Running => match running_ratio {
            Some(r) => format!("running {}", percent(r)),
            None => "running".to_string(),
        },
        Health::Ok => "OK".to_string(),
    };
    let mut description = running_lines;
    description.append(&mut failed_lines);
    description.push(match next {
        Some(n) => {
            format!("Next: {} / {} {}", n.repo_name, n.schedule_name, format::relative_short(&n.fire_date))
        }
        None => "No scheduled backups".to_string(),
    });
    TrayData { rows, state, description: description.join("\n") }
}

/// The app icon as an SNI pixmap (ARGB32, network byte order) for hosts
/// that cannot resolve the icon by name.
fn app_icon_pixmap() -> Vec<ksni::Icon> {
    let path = super::app::icons_dir().join(format!("{}.png", paths::APP_ID));
    let Ok(pixbuf) = gtk::gdk_pixbuf::Pixbuf::from_file_at_size(&path, 32, 32) else { return vec![] };
    if pixbuf.n_channels() != 4 || pixbuf.bits_per_sample() != 8 {
        return vec![];
    }
    let (width, height, stride) = (pixbuf.width() as usize, pixbuf.height() as usize, pixbuf.rowstride() as usize);
    let pixels = pixbuf.read_pixel_bytes();
    let mut data = Vec::with_capacity(width * height * 4);
    for y in 0..height {
        let Some(line) = pixels.get(y * stride..y * stride + width * 4) else { return vec![] };
        for rgba in line.chunks_exact(4) {
            data.extend_from_slice(&[rgba[3], rgba[0], rgba[1], rgba[2]]);
        }
    }
    vec![ksni::Icon { width: width as i32, height: height as i32, data }]
}

// --------------------------------------------------------------------- start

/// Registers the tray icon. Returns false when no StatusNotifier host is
/// available (the app then quits when its last window closes).
///
/// `wait_for_host`: for `luna --background` at login, which usually races
/// the bar that provides the StatusNotifierWatcher. A missing watcher is then
/// not an error: the item registers as soon as one appears on the bus.
pub fn start(app: &Rc<AppState>, wait_for_host: bool) -> bool {
    // Screenshot runs happen in a throwaway compositor but on the user's
    // session bus: keep them out of the real tray.
    if std::env::var_os("LUNA_SHOT_DIR").is_some() && std::env::var_os("LUNA_DEBUG_TRAY").is_none() {
        return false;
    }

    // A symbolic box like the Mac's `shippingbox.fill`; the app icon when
    // the icon theme has none.
    let themed = gtk::gdk::Display::default()
        .map(|d| gtk::IconTheme::for_display(&d).has_icon(kit::icons::BOX))
        .unwrap_or(false);
    let (commands, received) = async_channel::unbounded::<Command>();
    let initial = tray_data(app);
    let tray = BorgTray {
        data: initial.clone(),
        commands,
        icon_name: if themed { kit::icons::BOX.to_string() } else { paths::APP_ID.to_string() },
        icon_theme_path: super::app::icons_dir().to_string_lossy().into_owned(),
        icon_pixmap: app_icon_pixmap(),
    };

    // `spawn` only succeeds once a StatusNotifierWatcher accepted the item.
    // It answers at once when there is no watcher; the timeout only guards
    // against a session bus that does not answer at all.
    let (done, registered) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = tray.assume_sni_available(wait_for_host).spawn().map_err(|e| e.to_string());
        if let Err(std::sync::mpsc::SendError(Ok(late))) = done.send(result) {
            late.shutdown().wait();
        }
    });
    let handle = match registered.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(handle)) => handle,
        Ok(Err(e)) => {
            eprintln!("[tray] not available: {e}");
            return false;
        }
        Err(_) => {
            eprintln!("[tray] not available: the session bus did not answer");
            return false;
        }
    };
    super::debug::tray_probe();

    // Main thread → tray: push a fresh snapshot whenever it differs.
    let shown = RefCell::new(initial);
    let push = Rc::new(move |app: &AppState| {
        let next = tray_data(app);
        if *shown.borrow() != next {
            *shown.borrow_mut() = next.clone();
            handle.update(move |tray| tray.data = next);
        }
    });
    for signal in [&app.repos.changed, &app.statuses.changed] {
        signal.connect(glib::clone!(
            #[weak]
            app,
            #[strong]
            push,
            move || push(&app)
        ));
    }
    // "next in 3 hr" ages even when nothing changes.
    glib::timeout_add_seconds_local(
        60,
        glib::clone!(
            #[weak]
            app,
            #[upgrade_or]
            glib::ControlFlow::Break,
            move || {
                push(&app);
                glib::ControlFlow::Continue
            }
        ),
    );

    // Tray → main thread.
    let weak = Rc::downgrade(app);
    glib::spawn_future_local(async move {
        while let Ok(command) = received.recv().await {
            let Some(app) = weak.upgrade() else { break };
            dispatch(&app, command);
        }
    });
    true
}

fn dispatch(app: &Rc<AppState>, command: Command) {
    match command {
        Command::ToggleMenu => menu::toggle(app),
        Command::Open => window::show(app),
        Command::OpenRepo(repo_id) => {
            app.request_selection(repo_id);
            window::show(app);
        }
        Command::RunNow(repo_id, schedule_id) => menu::run_now(app, repo_id, schedule_id),
        Command::Stop(repo_id, schedule_id) => {
            let app = app.clone();
            kit::bg(move || schedule::cancel(&repo_id, &schedule_id), move |_| refresh_statuses_soon(&app));
        }
        Command::NewOnBorgBase => borgbase_wizard::open(app),
        Command::NewOnBorgBox => borgbox_wizard::open(app),
        Command::NewBorgRepository => borg_wizard::open(app),
        Command::Quit => app.application.quit(),
    }
}
