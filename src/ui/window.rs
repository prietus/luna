//! Main window (BorgMac's `ContentView`): repository sidebar + detail pane,
//! with a macOS-style unified toolbar.
use super::kit::{self, icons};
use super::state::{AppState, connect_while_alive};
use super::{
    add_repository, automation, borg_wizard, borgbase_panel, borgbase_wizard, borgbox_panel, borgbox_wizard, detail,
    stats,
};
use crate::core::models::Repository;
use adw::prelude::*;
use gtk::{gdk, gio, glib};
use std::cell::RefCell;
use std::rc::Rc;
use uuid::Uuid;

const WINDOW_CLASS: &str = "borg-main";

/// Shows the singleton main window, creating it when needed, and applies any
/// pending repository selection.
pub fn show(app: &Rc<AppState>) {
    let existing = app.application.windows().into_iter().find(|w| w.has_css_class(WINDOW_CLASS));
    let window = existing.unwrap_or_else(|| build(app).upcast());
    window.present();
    app.selection_requested.emit();
}

/// The main window, if it is currently open.
pub fn current(app: &Rc<AppState>) -> Option<gtk::Window> {
    app.application.windows().into_iter().find(|w| w.has_css_class(WINDOW_CLASS))
}

fn lights() -> gtk::WindowControls {
    let controls = gtk::WindowControls::new(gtk::PackType::Start);
    controls.set_decoration_layout(Some("close,minimize,maximize:"));
    controls.add_css_class("borg-lights");
    controls.set_valign(gtk::Align::Center);
    controls
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

fn tool_button(icon: &str, tooltip: &str) -> gtk::Button {
    let b = gtk::Button::from_icon_name(icon);
    b.set_tooltip_text(Some(tooltip));
    b
}

struct Ui {
    app: Rc<AppState>,
    window: adw::ApplicationWindow,
    list: gtk::ListBox,
    title: gtk::Label,
    subtitle: gtk::Label,
    detail_tools: gtk::Box,
    body: adw::Bin,
    selection: RefCell<Option<Uuid>>,
    /// Ids of the rows currently in `list`, in order.
    row_ids: RefCell<Vec<Uuid>>,
}

fn build(app: &Rc<AppState>) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(&app.application)
        .title("Luna")
        .default_width(1040)
        .default_height(700)
        .width_request(820)
        .height_request(520)
        .build();
    window.add_css_class(WINDOW_CLASS);

    let split = adw::OverlaySplitView::new();
    split.set_min_sidebar_width(220.0);
    split.set_max_sidebar_width(280.0);
    split.set_sidebar_width_fraction(0.26);

    // ------------------------------------------------------------- sidebar
    let side_header = adw::HeaderBar::new();
    side_header.set_show_title(false);
    side_header.set_show_start_title_buttons(false);
    side_header.set_show_end_title_buttons(false);
    side_header.pack_start(&lights());

    let add_menu = gio::Menu::new();
    let wizards = gio::Menu::new();
    wizards.append(Some("New on BorgBase…"), Some("win.new-borgbase"));
    wizards.append(Some("New on BorgBox…"), Some("win.new-borgbox"));
    wizards.append(Some("New Borg repository (local or SSH)…"), Some("win.new-borg"));
    add_menu.append_section(None, &wizards);
    let existing = gio::Menu::new();
    existing.append(Some("Add existing repository…"), Some("win.add-existing"));
    add_menu.append_section(None, &existing);
    let add_button = gtk::MenuButton::new();
    add_button.set_icon_name(icons::PLUS);
    add_button.set_always_show_arrow(true);
    add_button.set_menu_model(Some(&add_menu));
    add_button.set_tooltip_text(Some("Add a repository"));
    side_header.pack_start(&pill(&[add_button.upcast_ref()]));

    let hide_sidebar = tool_button(icons::SIDEBAR, "Hide sidebar");
    side_header.pack_start(&pill(&[hide_sidebar.upcast_ref()]));

    let list = gtk::ListBox::new();
    list.add_css_class("borg-sidebar");
    list.set_selection_mode(gtk::SelectionMode::Single);
    let side_body = kit::vbox(0);
    side_body.append(&kit::label("Repositories", &["borg-sidebar-heading"]));
    side_body.append(&list);
    let side_scroll = kit::scrolled(&side_body);

    let side_view = adw::ToolbarView::new();
    side_view.add_top_bar(&side_header);
    side_view.set_content(Some(&side_scroll));
    split.set_sidebar(Some(&side_view));

    // ------------------------------------------------------------- content
    let header = adw::HeaderBar::new();
    header.add_css_class("borg-content-header");
    header.set_show_start_title_buttons(false);
    header.set_show_end_title_buttons(false);
    header.set_title_widget(Some(&gtk::Box::new(gtk::Orientation::Horizontal, 0)));

    // Shown only while the sidebar is hidden, so the window stays operable.
    let show_sidebar = tool_button(icons::SIDEBAR, "Show sidebar");
    let collapsed = kit::hbox(8);
    collapsed.append(&lights());
    collapsed.append(&pill(&[show_sidebar.upcast_ref()]));
    collapsed.set_visible(false);
    header.pack_start(&collapsed);

    let title = kit::label("", &["borg-title"]);
    let subtitle = kit::ellipsized("", true, &["borg-subtitle"]);
    subtitle.set_max_width_chars(48);
    let titles = kit::vbox(0);
    titles.set_valign(gtk::Align::Center);
    titles.set_margin_start(6);
    titles.append(&title);
    titles.append(&subtitle);
    header.pack_start(&titles);

    let borgbase = tool_button(icons::CLOUD, "Open BorgBase panel (remote repos and SSH keys)");
    let borgbox = tool_button(icons::SERVER, "Open BorgBox server panel (stats, repos, maintenance)");
    let statistics = tool_button(icons::CHART, "Aggregated statistics panel across all repositories");
    let automation_btn = tool_button(icons::HISTORY, "Manage BorgBox maintenance schedules across all servers");
    header.pack_end(&pill(&[
        borgbase.upcast_ref(),
        borgbox.upcast_ref(),
        statistics.upcast_ref(),
        automation_btn.upcast_ref(),
    ]));
    let detail_tools = kit::hbox(8);
    header.pack_end(&detail_tools);

    let body = adw::Bin::new();
    let content_view = adw::ToolbarView::new();
    content_view.add_top_bar(&header);
    content_view.set_content(Some(&body));
    split.set_content(Some(&content_view));
    window.set_content(Some(&split));

    let ui = Rc::new(Ui {
        app: app.clone(),
        window: window.clone(),
        list: list.clone(),
        title,
        subtitle,
        detail_tools,
        body,
        selection: RefCell::new(None),
        row_ids: RefCell::new(vec![]),
    });

    // ------------------------------------------------------------ behaviour
    hide_sidebar.connect_clicked(glib::clone!(
        #[weak]
        split,
        move |_| split.set_show_sidebar(false)
    ));
    show_sidebar.connect_clicked(glib::clone!(
        #[weak]
        split,
        move |_| split.set_show_sidebar(true)
    ));
    split.connect_show_sidebar_notify(glib::clone!(
        #[weak]
        collapsed,
        move |split| collapsed.set_visible(!split.shows_sidebar())
    ));

    let add_action = |name: &str, f: Box<dyn Fn(&Rc<Ui>)>| {
        let action = gio::SimpleAction::new(name, None);
        let weak = Rc::downgrade(&ui);
        action.connect_activate(move |_, _| {
            if let Some(ui) = weak.upgrade() {
                f(&ui);
            }
        });
        window.add_action(&action);
    };
    add_action("new-borgbase", Box::new(|ui| borgbase_wizard::open(&ui.app)));
    add_action("new-borgbox", Box::new(|ui| borgbox_wizard::open(&ui.app)));
    add_action("new-borg", Box::new(|ui| borg_wizard::open(&ui.app)));
    add_action("add-existing", Box::new(|ui| add_repository::present(&ui.window, &ui.app, "", "")));

    borgbase.connect_clicked(glib::clone!(
        #[weak]
        ui,
        move |_| {
            let (window, app) = (ui.window.clone(), ui.app.clone());
            borgbase_panel::present(&ui.window, &ui.app, move |name, url| {
                add_repository::present(&window, &app, &name, &url);
            });
        }
    ));
    borgbox.connect_clicked(glib::clone!(
        #[weak]
        ui,
        move |_| borgbox_panel::present(&ui.window, &ui.app)
    ));
    statistics.connect_clicked(glib::clone!(
        #[weak]
        ui,
        move |_| stats::present(&ui.window, &ui.app)
    ));
    automation_btn.connect_clicked(glib::clone!(
        #[weak]
        ui,
        move |_| automation::present(&ui.window, &ui.app)
    ));

    list.connect_row_selected(glib::clone!(
        #[weak]
        ui,
        move |_, row| {
            let id = row.and_then(|r| ui.row_ids.borrow().get(r.index() as usize).copied());
            if *ui.selection.borrow() != id {
                *ui.selection.borrow_mut() = id;
                show_detail(&ui);
            }
        }
    ));

    connect_while_alive(
        app,
        |a| &a.repos.changed,
        &window,
        glib::clone!(
            #[weak]
            ui,
            move || reload_sidebar(&ui)
        ),
    );
    connect_while_alive(
        app,
        |a| &a.selection_requested,
        &window,
        glib::clone!(
            #[weak]
            ui,
            move || {
                let pending = ui.app.pending_selection.borrow_mut().take();
                if let Some(id) = pending {
                    select(&ui, Some(id));
                }
            }
        ),
    );

    // Keep `ui` alive for as long as the window exists.
    let keep = RefCell::new(Some(ui.clone()));
    window.connect_destroy(move |_| {
        keep.borrow_mut().take();
    });

    reload_sidebar(&ui);
    show_detail(&ui);
    window
}

fn sidebar_row(ui: &Rc<Ui>, repo: &Repository) -> gtk::ListBoxRow {
    let content = kit::hbox(8);
    content.append(&kit::icon(icons::DRIVE));
    content.append(&kit::ellipsized(&repo.name, false, &[]));
    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&content));

    // Context menu: Delete.
    let menu = gio::Menu::new();
    menu.append(Some("Delete"), Some("row.delete"));
    let popover = gtk::PopoverMenu::from_model(Some(&menu));
    popover.set_parent(&row);
    popover.set_has_arrow(false);
    popover.set_halign(gtk::Align::Start);
    row.connect_destroy(glib::clone!(
        #[weak]
        popover,
        move |_| popover.unparent()
    ));
    let group = gio::SimpleActionGroup::new();
    let delete = gio::SimpleAction::new("delete", None);
    let repo_id = repo.id;
    delete.connect_activate(glib::clone!(
        #[weak]
        ui,
        move |_, _| {
            let Some(repo) = ui.app.repos.get(&repo_id) else { return };
            let app = ui.app.clone();
            let weak = Rc::downgrade(&ui);
            kit::confirm(
                &ui.window,
                &format!("Delete {}?", repo.name),
                "Removes the repository from Luna together with its saved passphrase and schedules. The repository itself and its archives are not touched.",
                "Delete",
                "Cancel",
                true,
                move || {
                    if let Some(ui) = weak.upgrade() {
                        if *ui.selection.borrow() == Some(repo.id) {
                            *ui.selection.borrow_mut() = None;
                        }
                    }
                    app.repos.remove(&repo);
                },
            );
        }
    ));
    group.add_action(&delete);
    row.insert_action_group("row", Some(&group));
    let click = gtk::GestureClick::new();
    click.set_button(gdk::BUTTON_SECONDARY);
    click.connect_pressed(glib::clone!(
        #[weak]
        popover,
        move |gesture, _, x, y| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            popover.popup();
        }
    ));
    row.add_controller(click);
    row
}

fn reload_sidebar(ui: &Rc<Ui>) {
    let repos = ui.app.repos.repositories();
    let wanted = *ui.selection.borrow();
    // Rebuilding the rows drops the selection; don't let that reset the
    // detail pane.
    *ui.row_ids.borrow_mut() = vec![];
    ui.list.remove_all();
    for repo in &repos {
        ui.list.append(&sidebar_row(ui, repo));
    }
    *ui.row_ids.borrow_mut() = repos.iter().map(|r| r.id).collect();

    let still_there = wanted.filter(|id| repos.iter().any(|r| &r.id == id));
    match still_there.and_then(|id| repos.iter().position(|r| r.id == id)) {
        Some(idx) => {
            ui.list.select_row(ui.list.row_at_index(idx as i32).as_ref());
            // Name/URL may have changed.
            if let Some(repo) = repos.get(idx) {
                ui.title.set_text(&repo.name);
                ui.subtitle.set_text(&repo.url);
            }
        }
        None => {
            ui.list.unselect_all();
            if wanted.is_some() || ui.selection.borrow().is_some() {
                *ui.selection.borrow_mut() = None;
                show_detail(ui);
            }
        }
    }
}

fn select(ui: &Rc<Ui>, id: Option<Uuid>) {
    let idx = id.and_then(|id| ui.row_ids.borrow().iter().position(|r| *r == id));
    match idx {
        Some(idx) => ui.list.select_row(ui.list.row_at_index(idx as i32).as_ref()),
        None => ui.list.unselect_all(),
    }
}

fn show_detail(ui: &Rc<Ui>) {
    kit::clear_box(&ui.detail_tools);
    let repo = ui.selection.borrow().and_then(|id| ui.app.repos.get(&id));
    match repo {
        Some(repo) => {
            ui.title.set_text(&repo.name);
            ui.subtitle.set_text(&repo.url);
            ui.subtitle.set_visible(true);
            let view = detail::build(&ui.app, &repo);
            ui.detail_tools.append(&view.toolbar);
            ui.body.set_child(Some(&view.root));
        }
        None => {
            ui.title.set_text("Luna");
            ui.subtitle.set_text("");
            ui.subtitle.set_visible(false);
            ui.body.set_child(Some(&kit::empty_state(
                icons::DRIVE,
                "Select a repository",
                "Add or pick a repository in the sidebar.",
            )));
        }
    }
}
