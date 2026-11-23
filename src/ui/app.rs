//! Application bootstrap: single-instance `adw::Application`, CSS, bundled
//! icons, tray icon, and the background ("menu bar app") lifecycle.
use super::state::AppState;
use super::{tray, window};
use crate::core::paths;
use adw::prelude::*;
use gtk::{gdk, gio, glib};
use std::cell::RefCell;
use std::rc::Rc;

const CSS: &str = concat!(
    include_str!("style.css"),
    include_str!("style-schedules.css"),
    include_str!("style-browser.css"),
    include_str!("style-borgbase.css"),
    include_str!("style-borgbox.css"),
    include_str!("style-borgbox2.css"),
    include_str!("style-menu.css"),
);
const CSS_DARK: &str = include_str!("style-dark.css");

/// Icons compiled into the binary; unpacked to the cache dir at startup and
/// added to the icon theme search path so `borg-*-symbolic` names resolve
/// without any system-wide installation.
const ICONS: &[(&str, &[u8])] = &[
    ("borg-drive-symbolic.svg", include_bytes!("../../data/icons/borg-drive-symbolic.svg")),
    ("borg-chart-symbolic.svg", include_bytes!("../../data/icons/borg-chart-symbolic.svg")),
    ("borg-archive-symbolic.svg", include_bytes!("../../data/icons/borg-archive-symbolic.svg")),
    ("borg-cloud-symbolic.svg", include_bytes!("../../data/icons/borg-cloud-symbolic.svg")),
    ("borg-server-symbolic.svg", include_bytes!("../../data/icons/borg-server-symbolic.svg")),
    ("borg-xmark-circle-symbolic.svg", include_bytes!("../../data/icons/borg-xmark-circle-symbolic.svg")),
    ("borg-check-circle-symbolic.svg", include_bytes!("../../data/icons/borg-check-circle-symbolic.svg")),
    ("borg-minus-circle-symbolic.svg", include_bytes!("../../data/icons/borg-minus-circle-symbolic.svg")),
    ("borg-play-circle-symbolic.svg", include_bytes!("../../data/icons/borg-play-circle-symbolic.svg")),
    ("borg-plus-circle-symbolic.svg", include_bytes!("../../data/icons/borg-plus-circle-symbolic.svg")),
    ("borg-ellipsis-circle-symbolic.svg", include_bytes!("../../data/icons/borg-ellipsis-circle-symbolic.svg")),
    ("us.priet.Luna.png", include_bytes!("../../data/icons/us.priet.Luna.png")),
];

/// Directory holding the unpacked icons (also used by the tray icon).
pub fn icons_dir() -> std::path::PathBuf {
    paths::cache_dir().join("icons")
}

fn install_icons() {
    let dir = icons_dir();
    let _ = std::fs::create_dir_all(&dir);
    for (name, data) in ICONS {
        let path = dir.join(name);
        if std::fs::read(&path).map(|existing| existing != *data).unwrap_or(true) {
            let _ = std::fs::write(&path, data);
        }
    }
    if let Some(display) = gdk::Display::default() {
        gtk::IconTheme::for_display(&display).add_search_path(&dir);
    }
}

fn load_css() {
    let Some(display) = gdk::Display::default() else { return };
    let base = gtk::CssProvider::new();
    base.load_from_string(CSS);
    gtk::style_context_add_provider_for_display(&display, &base, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1);

    // The dark palette is a second provider that is filled only while the
    // system prefers dark, and emptied again when it switches back.
    let dark = gtk::CssProvider::new();
    gtk::style_context_add_provider_for_display(&display, &dark, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 2);
    let style = adw::StyleManager::default();
    let apply = move |style: &adw::StyleManager| {
        dark.load_from_string(if style.is_dark() { CSS_DARK } else { "" });
    };
    apply(&style);
    style.connect_dark_notify(apply);
}

/// `start_hidden`: come up as a tray icon only (`luna --background`), for
/// autostart. The tray host may not be up yet at login, so the icon waits for
/// it instead of falling back to the window.
pub fn run(start_hidden: bool) -> glib::ExitCode {
    // Screenshot runs (see `debug.rs`) must not be swallowed by an instance
    // that is already running.
    let flags = if std::env::var_os("LUNA_SHOT_DIR").is_some() {
        gio::ApplicationFlags::NON_UNIQUE
    } else {
        gio::ApplicationFlags::empty()
    };
    let application = adw::Application::builder().application_id(paths::APP_ID).flags(flags).build();
    glib::set_application_name(paths::APP_NAME);

    let state: Rc<RefCell<Option<Rc<AppState>>>> = Rc::new(RefCell::new(None));

    application.connect_startup(glib::clone!(
        #[strong]
        state,
        move |application| {
            install_icons();
            gtk::Window::set_default_icon_name(paths::APP_ID);
            load_css();
            let app = AppState::new(application);
            // Like the Mac app, keep running in the background behind the
            // tray icon after the last window closes. Without a tray host
            // there would be no way back in, so then closing the window quits.
            if tray::start(&app, start_hidden) {
                app.tray_active.set(true);
                std::mem::forget(application.hold());
            }

            let quit = gio::SimpleAction::new("quit", None);
            quit.connect_activate(glib::clone!(
                #[weak]
                application,
                move |_, _| application.quit()
            ));
            application.add_action(&quit);
            application.set_accels_for_action("app.quit", &["<primary>q"]);
            application.set_accels_for_action("window.close", &["<primary>w"]);

            super::debug::install(&app);
            *state.borrow_mut() = Some(app);
        }
    ));

    // Only the very first activation is silent; activating again (launching
    // Luna a second time, the tray's "Open Luna") shows the window.
    let stay_hidden = std::cell::Cell::new(start_hidden);
    application.connect_activate(move |_| {
        if let Some(app) = state.borrow().as_ref() {
            if stay_hidden.replace(false) && app.tray_active.get() {
                return;
            }
            window::show(app);
        }
    });

    // A background launch while Luna is already running has nothing to do;
    // without this it would pop the running instance's window up.
    if start_hidden && application.register(gio::Cancellable::NONE).is_ok() && application.is_remote() {
        return glib::ExitCode::SUCCESS;
    }

    // GTK must not see our own flags; nothing is forwarded.
    application.run_with_args::<&str>(&[])
}
