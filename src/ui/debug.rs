//! Development hooks, inert unless their environment variables are set:
//!
//! - `LUNA_DEBUG_OPEN=<view>` opens a view right after startup (first
//!   repository selected), so a view can be exercised without clicking.
//! - `LUNA_SHOT_DIR=<dir>` + `LUNA_SHOT_AFTER=<ms>`: that long after
//!   startup every toplevel window is rendered to `<dir>/<n>-<title>.png` by
//!   GTK itself (works on any compositor); `LUNA_SHOT_QUIT` then quits.
//! - `LUNA_DEBUG_TRAY=1`: right after the tray icon registered, dump
//!   what the StatusNotifierWatcher and the item expose over D-Bus and exit
//!   before any window is shown. (Also re-enables the tray under
//!   `LUNA_SHOT_DIR`, where it is otherwise skipped.)
use super::state::AppState;
use super::*;
use adw::prelude::*;
use std::rc::Rc;

pub fn install(app: &Rc<AppState>) {
    if let Ok(dir) = std::env::var("LUNA_SHOT_DIR") {
        let dir = std::path::PathBuf::from(dir);
        let _ = std::fs::create_dir_all(&dir);
        if let Some(ms) = std::env::var("LUNA_SHOT_AFTER").ok().and_then(|v| v.parse::<u64>().ok()) {
            let quit = std::env::var("LUNA_SHOT_QUIT").is_ok();
            let application = app.application.clone();
            kit::after(ms, move || {
                shoot_all(&dir);
                if quit {
                    application.quit();
                }
            });
        }
    }
    // Session-only BorgBase token (never touches the keyring).
    if let Ok(token) = std::env::var("LUNA_DEBUG_BORGBASE_TOKEN") {
        crate::core::borgbase::set_token(&token);
    }
    if let Ok(view) = std::env::var("LUNA_DEBUG_OPEN") {
        let app = app.clone();
        kit::after(600, move || open(&app, &view));
    }
    // `LUNA_DEBUG_STEPS="click:Label;fill:0=text;select:0=2;action:repo.edit;wait:800"`
    // drives the UI after the view opened: `click` presses the last mapped
    // button with that label/tooltip, `fill` types into the n-th mapped text
    // field, `select` picks an item of the n-th mapped combo row, `action`
    // activates a widget-scoped action on the first widget that has it,
    // `scroll:<px>` scrolls every mapped scrolled window.
    if let Ok(steps) = std::env::var("LUNA_DEBUG_STEPS") {
        let steps: Vec<String> = steps.split(';').map(str::to_string).collect();
        kit::after(1500, move || run_steps(steps));
    }
}

fn mapped_widgets() -> Vec<gtk::Widget> {
    fn walk(w: &gtk::Widget, out: &mut Vec<gtk::Widget>) {
        if !w.is_mapped() {
            return;
        }
        out.push(w.clone());
        let mut child = w.first_child();
        while let Some(c) = child {
            walk(&c, out);
            child = c.next_sibling();
        }
    }
    let mut out = vec![];
    for w in gtk::Window::list_toplevels() {
        walk(&w, &mut out);
    }
    out
}

fn button_matches(button: &gtk::Button, text: &str) -> bool {
    if button.label().as_deref() == Some(text) || button.tooltip_text().as_deref() == Some(text) {
        return true;
    }
    let mut child = button.first_child();
    while let Some(c) = child {
        if c.downcast_ref::<gtk::Label>().is_some_and(|l| l.text() == text) {
            return true;
        }
        child = c.first_child().or_else(|| c.next_sibling());
    }
    false
}

fn run_steps(mut steps: Vec<String>) {
    if steps.is_empty() {
        return;
    }
    let step = steps.remove(0);
    let widgets = mapped_widgets();
    let mut delay = 600;
    let indexed = |spec: &str| spec.split_once('=').and_then(|(n, v)| Some((n.parse::<usize>().ok()?, v.to_string())));
    let done = match step.split_once(':') {
        Some(("wait", ms)) => {
            delay = ms.parse().unwrap_or(600);
            true
        }
        Some(("click", label)) => widgets
            .iter()
            .filter_map(|w| w.downcast_ref::<gtk::Button>())
            .filter(|b| b.is_sensitive() && button_matches(b, label))
            .next_back()
            .map(|b| b.emit_clicked())
            .is_some(),
        Some(("fill", spec)) => indexed(spec)
            .and_then(|(n, text)| {
                let field = widgets.iter().filter_map(|w| w.downcast_ref::<gtk::Text>()).nth(n)?;
                field.set_text(&text);
                Some(())
            })
            .is_some(),
        Some(("select", spec)) => indexed(spec)
            .and_then(|(n, item)| {
                let row = widgets.iter().filter_map(|w| w.downcast_ref::<adw::ComboRow>()).nth(n)?;
                row.set_selected(item.parse().ok()?);
                Some(())
            })
            .is_some(),
        Some(("scroll", px)) => {
            let scrollers: Vec<_> = widgets.iter().filter_map(|w| w.downcast_ref::<gtk::ScrolledWindow>()).collect();
            for s in &scrollers {
                s.vadjustment().set_value(px.parse().unwrap_or(0.0));
            }
            !scrollers.is_empty()
        }
        Some(("action", name)) => widgets.iter().any(|w| w.activate_action(name, None).is_ok()),
        _ => false,
    };
    if !done {
        eprintln!("LUNA_DEBUG_STEPS: step had no effect: {step}");
    }
    kit::after(delay, move || run_steps(steps));
}

fn open(app: &Rc<AppState>, view: &str) {
    let Some(win) = window::current(app) else { return };
    // `LUNA_DEBUG_SIZE=WxH` resizes the main window first, so tall
    // sheets fit in the screenshot.
    if let Some((w, h)) = std::env::var("LUNA_DEBUG_SIZE").ok().and_then(|v| {
        let (w, h) = v.split_once('x')?;
        Some((w.parse::<i32>().ok()?, h.parse::<i32>().ok()?))
    }) {
        win.set_default_size(w, h);
    }
    let repo = app.repos.repositories().into_iter().next();
    if let Some(r) = &repo {
        app.request_selection(r.id);
    }
    // `LUNA_DEBUG_ARCHIVE=<name>` picks the archive (default: the last one).
    let archive = || {
        let wanted = std::env::var("LUNA_DEBUG_ARCHIVE").ok();
        repo.as_ref().and_then(|r| crate::core::borg::list_archives(r).ok()).and_then(|a| match &wanted {
            Some(name) => a.into_iter().find(|x| &x.name == name),
            None => a.into_iter().last(),
        })
    };
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let server = app.borgbox.first();
    match (view, &repo) {
        ("add", _) => add_repository::present(&win, app, "", ""),
        ("prune", Some(r)) => prune::present(&win, app, r, || {}),
        ("create_backup", Some(r)) => create_backup::present(&win, app, r, || {}),
        ("update_passphrase", Some(r)) => update_passphrase::present(&win, app, r),
        ("schedule", Some(r)) => schedule_sheet::present(&win, app, r),
        (v, Some(r)) if v.starts_with("schedule-") => schedule_sheet::debug_open(&win, app, r, &v["schedule-".len()..]),
        ("browser", Some(r)) => {
            // Once the index is loaded: LUNA_DEBUG_PATH=<folder> navigates,
            // _SEARCH=<query> searches, _REVEAL=<entry> reveals a search hit in its
            // folder, _PREVIEW=<file> previews, _HISTORY=<file>
            // opens its history, _RESTORE=<file|folder> / _SAVE=<file> write
            // into the folder LUNA_DEBUG_DEST.
            if let Some(a) = archive() {
                let handle = archive_browser::open(&win, app, r, &a);
                kit::after(1500, move || {
                    if let Some(path) = env("LUNA_DEBUG_PATH") {
                        handle.navigate(&path);
                    }
                    if let Some(query) = env("LUNA_DEBUG_SEARCH") {
                        handle.search(&query);
                    }
                    if let Some(path) = env("LUNA_DEBUG_REVEAL") {
                        handle.reveal(&path);
                    }
                    if let Some(path) = env("LUNA_DEBUG_PREVIEW") {
                        handle.preview(&path);
                    }
                    if let Some(path) = env("LUNA_DEBUG_HISTORY") {
                        handle.history(&path);
                    }
                    if let Some(dest) = env("LUNA_DEBUG_DEST").map(std::path::PathBuf::from) {
                        if let Some(path) = env("LUNA_DEBUG_RESTORE") {
                            handle.restore_to(&path, &dest);
                        }
                        if let Some(path) = env("LUNA_DEBUG_SAVE") {
                            let name = path.rsplit('/').next().unwrap_or("saved").to_string();
                            handle.save_to(&path, &dest.join(name));
                        }
                    }
                });
            }
        }
        ("treemap", Some(r)) => {
            // LUNA_DEBUG_PATH=<folder> drills down, _HOVER=<name> highlights
            // a rectangle.
            if let Some(a) = archive() {
                let handle = treemap::open(&win, app, r, &a);
                kit::after(1500, move || {
                    if let Some(path) = env("LUNA_DEBUG_PATH") {
                        handle.drill(&path);
                    }
                    if let Some(name) = env("LUNA_DEBUG_HOVER") {
                        kit::after(300, move || handle.hover(&name));
                    }
                });
            }
        }
        ("history", Some(r)) => {
            let path = std::env::var("LUNA_DEBUG_PATH").unwrap_or_default();
            // LUNA_DEBUG_INDEX=1 presses "Index missing"; _PREVIEW=<archive>
            // previews that archive's version; _SAVE=<archive> saves it into
            // LUNA_DEBUG_DEST.
            let handle = file_history::open(&win, app, r, &path);
            kit::after(1500, move || {
                if env("LUNA_DEBUG_INDEX").is_some() {
                    handle.index_missing();
                }
                if let Some(name) = env("LUNA_DEBUG_PREVIEW") {
                    handle.preview(&name);
                }
                if let (Some(name), Some(dest)) = (env("LUNA_DEBUG_SAVE"), env("LUNA_DEBUG_DEST")) {
                    handle.save_to(&name, &std::path::Path::new(&dest).join(format!("saved.{name}")));
                }
            });
        }
        ("borgbase", _) => {
            let (window, app2) = (win.clone(), app.clone());
            borgbase_panel::present(&win, app, move |name, url| add_repository::present(&window, &app2, &name, &url))
        }
        // Edit sheet of the n-th (`LUNA_DEBUG_INDEX`, default 0) BorgBase repo.
        ("borgbase-edit", _) => {
            use crate::core::borgbase;
            let index = std::env::var("LUNA_DEBUG_INDEX").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
            match (borgbase::repos(), borgbase::ssh_keys()) {
                (Ok(repos), Ok(keys)) => match repos.get(index) {
                    Some(r) => borgbase_edit::present(&win, app, r, &keys, || eprintln!("borgbase-edit: saved")),
                    None => eprintln!("borgbase-edit: no repository #{index}"),
                },
                (Err(e), _) | (_, Err(e)) => eprintln!("borgbase-edit: {e}"),
            }
        }
        ("borgbox", _) => borgbox_panel::present(&win, app),
        ("stats", _) => stats::present(&win, app),
        ("automation", _) => automation::present(&win, app),
        ("wizard-borgbase", _) => borgbase_wizard::open(app),
        ("wizard-borg", _) => borg_wizard::open(app),
        ("wizard-borgbox", _) => borgbox_wizard::open(app),
        ("menu", _) => menu::toggle(app),
        // Open, close, reopen: exercises destroy-on-hide.
        ("menu-twice", _) => {
            menu::toggle(app);
            for delay in [500, 1000] {
                let app = app.clone();
                kit::after(delay, move || menu::toggle(&app));
            }
        }
        // The popover with a manual backup row: backs up a path that does
        // not exist, so the job ends up failed (or is caught mid-run).
        ("menu-manual", Some(r)) => {
            state::start_manual_backup(app, r, "debug-manual", vec!["/nonexistent/luna-debug".into()], vec![], || {});
            menu::toggle(app)
        }
        ("borgbox-passphrase", _) => borgbox_passphrase::present(&win, "demo", "Check", |_| {}),
        ("borgbox-alerts", _) => {
            if let Some(s) = &server {
                borgbox_alerts::present(&win, app, s, "demo")
            }
        }
        ("borgbox-archives", _) => {
            if let Some(s) = &server {
                borgbox_archives::present(&win, app, s, "demo", None)
            }
        }
        ("borgbox-prune", _) => {
            if let Some(s) = &server {
                borgbox_prune::present(&win, app, s, "demo", None, |_| {})
            }
        }
        ("borgbox-import", _) => {
            if let Some(s) = &server {
                borgbox_import::present(&win, app, s, "demo", "/srv/borg/demo", None, borgbox_import::Mode::Register)
            }
        }
        ("borgbox-schedule", _) => {
            if let Some(s) = &server {
                borgbox_schedule_editor::present(&win, app, s, None, Some("demo"), |_| {})
            }
        }
        ("select", _) => {}
        _ => eprintln!("LUNA_DEBUG_OPEN: unknown view or no repository: {view}"),
    }
}

/// Called by `tray::start` once the item is registered.
pub fn tray_probe() {
    if std::env::var_os("LUNA_DEBUG_TRAY").is_none() {
        return;
    }
    // ksni names its first item `org.kde.StatusNotifierItem-<pid>-1`.
    let name = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
    let busctl = |args: &[&str]| {
        println!("$ busctl --user {}", args.join(" "));
        match std::process::Command::new("busctl").arg("--user").args(args).output() {
            Ok(out) => print!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)),
            Err(e) => println!("busctl failed: {e}"),
        }
    };
    println!("tray registered as {name}");
    busctl(&[
        "get-property",
        "org.kde.StatusNotifierWatcher",
        "/StatusNotifierWatcher",
        "org.kde.StatusNotifierWatcher",
        "RegisteredStatusNotifierItems",
    ]);
    busctl(&["introspect", &name, "/StatusNotifierItem", "org.kde.StatusNotifierItem"]);
    busctl(&["call", "--", &name, "/MenuBar", "com.canonical.dbusmenu", "GetLayout", "iias", "0", "-1", "0"]);
    std::process::exit(0);
}

fn shoot_all(dir: &std::path::Path) {
    for (i, w) in gtk::Window::list_toplevels().into_iter().enumerate() {
        let Ok(win) = w.downcast::<gtk::Window>() else { continue };
        if !win.is_mapped() {
            continue;
        }
        let title: String = win
            .title()
            .map(|t| t.to_string())
            .unwrap_or_default()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '_' })
            .collect();
        let path = dir.join(format!("{i}-{title}.png"));
        match shoot(&win, &path) {
            Ok(()) => eprintln!("screenshot: {}", path.display()),
            Err(e) => eprintln!("screenshot failed: {e}"),
        }
    }
}

fn shoot(win: &gtk::Window, path: &std::path::Path) -> Result<(), String> {
    let (w, h) = (win.width(), win.height());
    let paintable = gtk::WidgetPaintable::new(Some(win));
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, w as f64, h as f64);
    let node = snapshot.to_node().ok_or("empty render node")?;
    let renderer = win.renderer().ok_or("no renderer")?;
    let texture = renderer.render_texture(&node, None);
    texture.save_to_png(path).map_err(|e| e.to_string())
}
