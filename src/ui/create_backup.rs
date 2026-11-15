//! "New backup" sheet: a one-off manual backup (BorgMac's
//! `CreateBackupSheet`). The job is handed to the manual-backup store so it
//! keeps running after the sheet closes.
use super::kit::{self, icons};
use super::schedule_sheet::{append_unique, item_line, path_picker_button};
use super::state::{self, AppState};
use crate::core::models::{BackupSchedule, Repository};
use adw::prelude::*;
use gtk::glib;
use std::cell::RefCell;
use std::rc::Rc;

struct Sheet {
    app: Rc<AppState>,
    repo: Repository,
    dialog: adw::Dialog,
    name: adw::EntryRow,
    paths: RefCell<Vec<String>>,
    paths_list: gtk::ListBox,
    /// Rows that always close the section (the "Add path" button…).
    paths_tail: Vec<gtk::ListBoxRow>,
    excludes: RefCell<Vec<String>>,
    excludes_list: gtk::ListBox,
    excludes_tail: Vec<gtk::ListBoxRow>,
    pattern: gtk::Entry,
    start: gtk::Button,
}

fn default_archive_name() -> String {
    let host = glib::host_name();
    let host = if host.is_empty() { "linux".to_string() } else { host.to_string() };
    format!("manual-{host}-{}", chrono::Local::now().format("%Y-%m-%d-%H%M%S"))
}

fn form_row(child: &impl IsA<gtk::Widget>) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();
    row.set_activatable(false);
    row.set_selectable(false);
    row.add_css_class("sched-form-row");
    row.set_child(Some(child));
    row
}

fn section(title: &str) -> (adw::PreferencesGroup, gtk::ListBox) {
    let group = kit::form_group();
    group.set_title(title);
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    group.add(&list);
    (group, list)
}

pub fn present(
    parent: &impl IsA<gtk::Widget>,
    app: &Rc<AppState>,
    repo: &Repository,
    on_completed: impl Fn() + 'static,
) {
    let (dialog, content) = kit::sheet("New backup", 580, 460);

    let form = kit::vbox(18);
    form.add_css_class("sched-form");
    content.append(&kit::scrolled(&form));

    let name_group = kit::form_group();
    let name = adw::EntryRow::builder().title("Archive name").text(default_archive_name()).build();
    name.set_activates_default(true);
    name_group.add(&name);
    form.append(&name_group);

    let (paths_group, paths_list) = section("Paths to back up");
    form.append(&paths_group);
    let (excludes_group, excludes_list) = section("Exclude");
    form.append(&excludes_group);

    let pattern = gtk::Entry::new();
    pattern.set_placeholder_text(Some("Pattern (e.g. **/node_modules)"));
    pattern.set_hexpand(true);
    pattern.add_css_class("sched-field");
    let add_exclude = kit::button("Add");
    add_exclude.set_sensitive(false);
    let presets = kit::button("Add common presets");
    let exclude_buttons = kit::hbox(8);
    exclude_buttons.append(&add_exclude);
    exclude_buttons.append(&kit::spacer());
    exclude_buttons.append(&presets);

    let cancel = kit::button("Cancel");
    let start = kit::primary_button("Start backup");
    content.append(&kit::button_row(None, &[&cancel, &start]));
    dialog.set_default_widget(Some(&start));

    // The "Add path" button needs the controller, which needs the row the
    // button lives in — so the row is created first and filled afterwards.
    let add_path_row = form_row(&kit::hbox(0));
    let sheet = Rc::new(Sheet {
        app: app.clone(),
        repo: repo.clone(),
        dialog: dialog.clone(),
        name: name.clone(),
        paths: RefCell::new(vec![]),
        paths_list,
        paths_tail: vec![add_path_row.clone()],
        excludes: RefCell::new(vec![]),
        excludes_list,
        excludes_tail: vec![form_row(&pattern), form_row(&exclude_buttons)],
        pattern: pattern.clone(),
        start: start.clone(),
    });

    let add_path = path_picker_button(
        "Add path",
        Some(icons::PLUS),
        "Pick the files and folders to back up",
        glib::clone!(
            #[weak]
            sheet,
            move |picked| {
                append_unique(&sheet.paths, picked);
                rebuild_paths(&sheet);
            }
        ),
    );
    add_path_row.set_child(Some(&add_path));

    name.connect_changed(glib::clone!(
        #[weak]
        sheet,
        move |_| update_start(&sheet)
    ));
    pattern.connect_changed(glib::clone!(
        #[weak]
        add_exclude,
        move |pattern| add_exclude.set_sensitive(!pattern.text().trim().is_empty())
    ));
    pattern.connect_activate(glib::clone!(
        #[weak]
        sheet,
        move |_| commit_new_exclude(&sheet)
    ));
    add_exclude.connect_clicked(glib::clone!(
        #[weak]
        sheet,
        move |_| commit_new_exclude(&sheet)
    ));
    presets.connect_clicked(glib::clone!(
        #[weak]
        sheet,
        move |_| {
            append_unique(&sheet.excludes, BackupSchedule::COMMON_EXCLUDE_PRESETS.iter().map(|p| p.to_string()));
            rebuild_excludes(&sheet);
        }
    ));
    cancel.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| {
            dialog.close();
        }
    ));

    let on_completed: Rc<dyn Fn()> = Rc::new(on_completed);
    start.connect_clicked(glib::clone!(
        #[weak]
        sheet,
        move |_| {
            // Hand the job off to the manual-backup store so it can keep
            // running after this sheet closes. Visibility moves to the
            // status menu.
            let on_completed = on_completed.clone();
            state::start_manual_backup(
                &sheet.app,
                &sheet.repo,
                &sheet.name.text(),
                sheet.paths.borrow().clone(),
                sheet.excludes.borrow().clone(),
                move || on_completed(),
            );
            sheet.dialog.close();
        }
    ));

    let keep = RefCell::new(Some(sheet.clone()));
    dialog.connect_closed(move |_| {
        keep.borrow_mut().take();
    });

    rebuild_paths(&sheet);
    rebuild_excludes(&sheet);
    if let Ok(paths) = std::env::var("LUNA_DEBUG_PATH") {
        // Debug hook: pre-fill the sources (colon-separated).
        append_unique(&sheet.paths, paths.split(':').filter(|p| !p.is_empty()).map(str::to_string));
        rebuild_paths(&sheet);
        if std::env::var("LUNA_DEBUG_START").is_ok() {
            let start = start.clone();
            kit::after(1500, move || start.emit_clicked());
        }
    }
    dialog.present(Some(parent));
}

fn update_start(sheet: &Rc<Sheet>) {
    sheet.start.set_sensitive(!sheet.paths.borrow().is_empty() && !sheet.name.text().is_empty());
}

#[allow(clippy::too_many_arguments)]
fn rebuild_list(
    sheet: &Rc<Sheet>,
    list: &gtk::ListBox,
    tail: &[gtk::ListBoxRow],
    items: &[String],
    icon_name: &str,
    text_classes: &[&str],
    empty_text: &str,
    remove: fn(&Rc<Sheet>, usize),
) {
    list.remove_all();
    if items.is_empty() {
        list.append(&form_row(&kit::label(empty_text, &["secondary"])));
    }
    for (index, item) in items.iter().enumerate() {
        let weak = Rc::downgrade(sheet);
        let line = item_line(icon_name, item, text_classes, icons::MINUS_CIRCLE, &["sched-borderless"], move || {
            if let Some(sheet) = weak.upgrade() {
                remove(&sheet, index);
            }
        });
        list.append(&form_row(&line));
    }
    for row in tail {
        list.append(row);
    }
}

fn rebuild_paths(sheet: &Rc<Sheet>) {
    let items = sheet.paths.borrow().clone();
    rebuild_list(
        sheet,
        &sheet.paths_list,
        &sheet.paths_tail,
        &items,
        icons::FOLDER,
        &[],
        "No paths selected",
        |sheet, index| {
            sheet.paths.borrow_mut().remove(index);
            rebuild_paths(sheet);
        },
    );
    update_start(sheet);
}

fn rebuild_excludes(sheet: &Rc<Sheet>) {
    let items = sheet.excludes.borrow().clone();
    rebuild_list(
        sheet,
        &sheet.excludes_list,
        &sheet.excludes_tail,
        &items,
        icons::NOSIGN,
        &["caption", "mono"],
        "No exclusion patterns.",
        |sheet, index| {
            sheet.excludes.borrow_mut().remove(index);
            rebuild_excludes(sheet);
        },
    );
}

fn commit_new_exclude(sheet: &Rc<Sheet>) {
    let trimmed = sheet.pattern.text().trim().to_string();
    if trimmed.is_empty() {
        return;
    }
    append_unique(&sheet.excludes, [trimmed]);
    sheet.pattern.set_text("");
    rebuild_excludes(sheet);
}
