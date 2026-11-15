//! Every version of one path across all archives of a repository (BorgMac's
//! `FileHistoryView`). Only archives whose index is cached on disk are known;
//! "Index missing" downloads the rest, one borg call each.
use super::archive_browser::{PreviewContent, choose_save_path, extract_file_to, load_preview, loading_page, present_preview, short_borg_date};
use super::kit::{self, icons};
use super::state::AppState;
use crate::core::archive_tree::cache;
use crate::core::models::{Archive, ArchiveEntry, Repository};
use crate::core::{borg, format};
use adw::prelude::*;
use gtk::{gdk, gio, glib};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

const QUESTION_CIRCLE: &str = "dialog-question-symbolic"; // questionmark.circle

#[derive(Debug, Clone)]
struct FileVersion {
    archive_name: String,
    archive_start: String,
    entry: Option<ArchiveEntry>,
    indexed: bool,
}

impl FileVersion {
    fn exists(&self) -> bool {
        self.entry.is_some()
    }
    fn status_icon(&self) -> &'static str {
        if !self.indexed {
            QUESTION_CIRCLE
        } else if self.exists() {
            icons::CHECK_CIRCLE
        } else {
            icons::MINUS_CIRCLE
        }
    }
    fn status_class(&self) -> &'static str {
        if !self.indexed {
            "c-orange"
        } else if self.exists() {
            "c-green"
        } else {
            "secondary"
        }
    }
    fn status_text(&self) -> &'static str {
        if !self.indexed {
            "not indexed"
        } else if self.exists() {
            "present"
        } else {
            "missing"
        }
    }
}

/// Looks the path up in every cached archive index. Reads and decodes the
/// cache files, so it runs on a worker thread.
fn build_versions(archives: &[Archive], file_path: &str) -> Vec<FileVersion> {
    archives
        .iter()
        .map(|archive| {
            let cached = cache::load(&archive.archive_id);
            FileVersion {
                archive_name: archive.name.clone(),
                archive_start: archive.start.clone(),
                indexed: cached.is_some(),
                entry: cached.and_then(|entries| entries.into_iter().find(|e| e.path == file_path)),
            }
        })
        .collect()
}

struct History {
    repo: Repository,
    file_path: String,
    dialog: adw::Dialog,
    archives: RefCell<Vec<Archive>>,
    versions: RefCell<Vec<FileVersion>>,
    loading_archives: Cell<bool>,
    indexing: Cell<bool>,
    index_progress: Cell<usize>,
    index_total: Cell<usize>,
    /// Extractions in flight (the footer spinner shows while > 0).
    busy: Cell<u32>,
    menu_target: Cell<Option<u32>>,
    stack: gtk::Stack,
    store: gio::ListStore,
    selection: gtk::SingleSelection,
    table: gtk::ColumnView,
    progress: gtk::ProgressBar,
    progress_label: gtk::Label,
    present_label: gtk::Box,
    present_text: gtk::Label,
    missing_label: gtk::Box,
    missing_text: gtk::Label,
    spinner: gtk::Spinner,
    index_button: gtk::Button,
}

/// Lets other code (the debug hooks) drive an open history sheet.
pub struct Handle(Weak<History>);

impl Handle {
    pub fn index_missing(&self) {
        if let Some(h) = self.0.upgrade() {
            index_missing(&h);
        }
    }

    fn with_version(&self, archive_name: &str, f: impl FnOnce(&Rc<History>, FileVersion)) {
        let Some(h) = self.0.upgrade() else { return };
        let version = h.versions.borrow().iter().find(|v| v.archive_name == archive_name).cloned();
        if let Some(v) = version {
            f(&h, v);
        }
    }

    /// Previews the version stored in the archive called `archive_name`.
    pub fn preview(&self, archive_name: &str) {
        self.with_version(archive_name, |h, v| preview(h, &v));
    }

    /// Saves that version as `destination` without asking.
    pub fn save_to(&self, archive_name: &str, destination: &Path) {
        self.with_version(archive_name, |h, v| save_to(h, &v, destination.to_path_buf()));
    }
}

pub fn present(parent: &impl IsA<gtk::Widget>, app: &Rc<AppState>, repo: &Repository, file_path: &str) {
    open(parent, app, repo, file_path);
}

pub fn open(parent: &impl IsA<gtk::Widget>, _app: &Rc<AppState>, repo: &Repository, file_path: &str) -> Handle {
    let (dialog, root) = kit::bare_sheet("File history", 760, 520);

    // ---------------------------------------------------------------- header
    let header = kit::hbox(8);
    header.add_css_class("history-header");
    let titles = kit::vbox(2);
    titles.set_hexpand(true);
    titles.append(&kit::label("File history", &["history-title"]));
    let path_label = kit::ellipsized(file_path, true, &["caption", "mono", "secondary"]);
    path_label.set_selectable(true);
    path_label.set_focusable(false);
    titles.append(&path_label);
    header.append(&titles);
    let close = kit::button("Close");
    close.set_valign(gtk::Align::Center);
    header.append(&close);
    root.append(&header);
    root.append(&kit::divider());

    // --------------------------------------------------------------- content
    let store = gio::ListStore::new::<glib::BoxedAnyObject>();
    let selection = gtk::SingleSelection::new(Some(store.clone()));
    selection.set_autoselect(false);
    selection.set_can_unselect(true);
    let table = gtk::ColumnView::new(Some(selection.clone()));
    table.add_css_class("mac-table");

    let archive_col = column(
        "Archive",
        || {
            let cell = kit::hbox(6);
            cell.append(&gtk::Image::new());
            cell.append(&kit::ellipsized("", true, &[]));
            cell
        },
        |cell, v| {
            if let Some(icon) = cell.first_child().and_then(|w| w.downcast::<gtk::Image>().ok()) {
                icon.set_icon_name(Some(v.status_icon()));
                set_status_class(&icon, v);
            }
            if let Some(name) = cell.last_child().and_then(|w| w.downcast::<gtk::Label>().ok()) {
                name.set_text(&v.archive_name);
            }
        },
    );
    archive_col.set_expand(true);
    table.append_column(&archive_col);
    let date_col = column(
        "Date",
        || kit::label("", &["caption", "numeric"]),
        |label, v| label.set_text(&short_borg_date(&v.archive_start)),
    );
    date_col.set_fixed_width(150);
    table.append_column(&date_col);
    let size_col = column(
        "Size",
        || kit::label("", &[]),
        |label, v| {
            for class in ["caption", "numeric", "tertiary"] {
                label.remove_css_class(class);
            }
            if let Some(entry) = &v.entry {
                label.set_text(&format::bytes_file(entry.size_bytes() as i64));
                label.add_css_class("caption");
                label.add_css_class("numeric");
            } else if !v.indexed {
                label.set_text("—");
                label.add_css_class("tertiary");
            } else {
                label.set_text("missing");
                label.add_css_class("caption");
                label.add_css_class("tertiary");
            }
        },
    );
    size_col.set_fixed_width(100);
    table.append_column(&size_col);
    let status_col = column(
        "Status",
        || kit::label("", &["caption"]),
        |label, v| {
            label.set_text(v.status_text());
            set_status_class(label, v);
        },
    );
    status_col.set_fixed_width(110);
    table.append_column(&status_col);

    let stack = gtk::Stack::new();
    stack.set_vexpand(true);
    stack.add_named(&loading_page("Listing the repository's archives…", ""), Some("loading"));
    stack.add_named(&kit::empty_state(icons::TRAY, "No archives", "The repository has no archives."), Some("empty"));
    stack.add_named(&kit::scrolled(&table), Some("table"));
    root.append(&stack);
    root.append(&kit::divider());

    // ---------------------------------------------------------------- footer
    let footer = kit::hbox(10);
    footer.add_css_class("history-footer");
    let progress = gtk::ProgressBar::new();
    progress.add_css_class("history-progress");
    progress.set_valign(gtk::Align::Center);
    let progress_label = kit::label("", &["caption", "secondary", "numeric"]);
    let footer_label = |icon: &str, class: &str| {
        let row = kit::hbox(5);
        row.add_css_class(class);
        row.add_css_class("caption");
        row.append(&kit::icon(icon));
        let text = kit::label("", &[]);
        row.append(&text);
        (row, text)
    };
    let (present_label, present_text) = footer_label(icons::CHECK_CIRCLE, "c-green");
    let (missing_label, missing_text) = footer_label(QUESTION_CIRCLE, "c-orange");
    let spinner = kit::spinner();
    spinner.set_visible(false);
    let index_button = gtk::Button::new();
    index_button.add_css_class("mac");
    index_button.set_child(Some(&adw::ButtonContent::builder().icon_name(icons::DOWNLOAD).label("Index missing").build()));
    index_button.set_tooltip_text(Some(
        "Downloads the manifests of archives that aren't indexed yet to complete the history. Each one costs one borg call.",
    ));
    footer.append(&progress);
    footer.append(&progress_label);
    footer.append(&present_label);
    footer.append(&missing_label);
    footer.append(&kit::spacer());
    footer.append(&spinner);
    footer.append(&index_button);
    root.append(&footer);

    let h = Rc::new(History {
        repo: repo.clone(),
        file_path: file_path.to_string(),
        dialog: dialog.clone(),
        archives: RefCell::new(vec![]),
        versions: RefCell::new(vec![]),
        loading_archives: Cell::new(false),
        indexing: Cell::new(false),
        index_progress: Cell::new(0),
        index_total: Cell::new(0),
        busy: Cell::new(0),
        menu_target: Cell::new(None),
        stack,
        store,
        selection,
        table,
        progress,
        progress_label,
        present_label,
        present_text,
        missing_label,
        missing_text,
        spinner,
        index_button,
    });

    close.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| {
            dialog.close();
        }
    ));
    h.index_button.connect_clicked(glib::clone!(
        #[weak]
        h,
        move |_| index_missing(&h)
    ));
    // Double click / Return: preview.
    h.table.connect_activate(glib::clone!(
        #[weak]
        h,
        move |_, position| {
            if let Some(v) = version_at(&h, position).filter(|v| v.exists()) {
                preview(&h, &v);
            }
        }
    ));
    install_context_menu(&h);

    // The controller lives as long as the sheet is open.
    let keep = RefCell::new(Some(h.clone()));
    dialog.connect_closed(move |_| {
        keep.borrow_mut().take();
    });

    dialog.present(Some(parent));
    load_archives(&h);
    Handle(Rc::downgrade(&h))
}

fn set_status_class(widget: &impl IsA<gtk::Widget>, v: &FileVersion) {
    for class in ["c-orange", "c-green", "secondary"] {
        widget.remove_css_class(class);
    }
    widget.add_css_class(v.status_class());
}

/// Column whose cells are built once by `setup` and filled by `bind`.
fn column<W: IsA<gtk::Widget>>(
    title: &str,
    setup: impl Fn() -> W + 'static,
    bind: impl Fn(&W, &FileVersion) + 'static,
) -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_, obj| {
        let Some(cell) = obj.downcast_ref::<gtk::ColumnViewCell>() else { return };
        let holder = kit::hbox(0);
        holder.add_css_class("history-cell");
        holder.append(&setup());
        cell.set_child(Some(&holder));
    });
    factory.connect_bind(move |_, obj| {
        let Some(cell) = obj.downcast_ref::<gtk::ColumnViewCell>() else { return };
        let Some(holder) = cell.child() else { return };
        // The context menu maps a click back to its row through this name.
        holder.set_widget_name(&cell.position().to_string());
        let (Some(widget), Some(version)) = (
            holder.first_child().and_then(|w| w.downcast::<W>().ok()),
            cell.item().and_then(|o| kit::unbox::<FileVersion>(&o)),
        ) else {
            return;
        };
        bind(&widget, &version);
    });
    let col = gtk::ColumnViewColumn::new(Some(title), Some(factory));
    col.set_resizable(true);
    col
}

fn version_at(h: &History, position: u32) -> Option<FileVersion> {
    h.store.item(position).and_then(|o| kit::unbox::<FileVersion>(&o))
}

// ---------------------------------------------------------------- rendering

fn set_versions(h: &History, versions: Vec<FileVersion>) {
    let selected = h.selection.selected();
    let items: Vec<glib::BoxedAnyObject> = versions.iter().cloned().map(kit::boxed).collect();
    h.store.splice(0, h.store.n_items(), &items);
    if selected != gtk::INVALID_LIST_POSITION && selected < h.store.n_items() {
        h.selection.set_selected(selected);
    }
    h.versions.replace(versions);
    update(h);
}

fn update(h: &History) {
    let (total, present, not_indexed) = {
        let versions = h.versions.borrow();
        (
            versions.len(),
            versions.iter().filter(|v| v.exists()).count(),
            versions.iter().filter(|v| !v.indexed).count(),
        )
    };
    h.stack.set_visible_child_name(if h.loading_archives.get() {
        "loading"
    } else if total == 0 {
        "empty"
    } else {
        "table"
    });

    let indexing = h.indexing.get();
    h.progress.set_visible(indexing);
    h.progress_label.set_visible(indexing);
    h.present_label.set_visible(!indexing);
    h.missing_label.set_visible(!indexing && not_indexed > 0);
    if indexing {
        let (done, of) = (h.index_progress.get(), h.index_total.get());
        h.progress.set_fraction(done as f64 / of.max(1) as f64);
        h.progress_label.set_text(&format!("Indexing {done}/{of}…"));
    }
    h.present_text.set_text(&format!("{present} present"));
    h.missing_text.set_text(&format!("{not_indexed} not indexed"));
    h.index_button.set_visible(not_indexed > 0);
    h.index_button.set_sensitive(!indexing);
}

fn set_busy(h: &History, on: bool) {
    let n = if on { h.busy.get() + 1 } else { h.busy.get().saturating_sub(1) };
    h.busy.set(n);
    h.spinner.set_visible(n > 0);
}

// ------------------------------------------------------------------ loading

fn load_archives(h: &Rc<History>) {
    h.loading_archives.set(true);
    update(h);
    let (repo, file_path) = (h.repo.clone(), h.file_path.clone());
    kit::bg(
        move || {
            let mut archives = borg::list_archives(&repo)?;
            // Newest first.
            archives.sort_by(|a, b| b.start.cmp(&a.start));
            let versions = build_versions(&archives, &file_path);
            Ok::<_, String>((archives, versions))
        },
        glib::clone!(
            #[weak]
            h,
            move |result: Result<(Vec<Archive>, Vec<FileVersion>), String>| {
                h.loading_archives.set(false);
                match result {
                    Ok((archives, versions)) => {
                        h.archives.replace(archives);
                        set_versions(&h, versions);
                    }
                    Err(e) => {
                        update(&h);
                        kit::alert(&h.dialog, "Error", &e);
                    }
                }
            }
        ),
    );
}

fn index_missing(h: &Rc<History>) {
    if h.indexing.get() {
        return;
    }
    let targets: Vec<Archive> = {
        let versions = h.versions.borrow();
        h.archives
            .borrow()
            .iter()
            .filter(|a| versions.iter().any(|v| v.archive_name == a.name && !v.indexed))
            .cloned()
            .collect()
    };
    if targets.is_empty() {
        return;
    }
    h.indexing.set(true);
    h.index_progress.set(0);
    h.index_total.set(targets.len());
    update(h);
    index_next(h, targets);
}

/// Indexes the first of `targets` on a worker thread, refreshes the table,
/// then continues with the rest. Stops at the first error, or when the sheet
/// is closed.
fn index_next(h: &Rc<History>, mut targets: Vec<Archive>) {
    if targets.is_empty() {
        h.indexing.set(false);
        update(h);
        return;
    }
    let archive = targets.remove(0);
    let (repo, file_path, archives) = (h.repo.clone(), h.file_path.clone(), h.archives.borrow().clone());
    kit::bg(
        move || {
            let entries = borg::list_archive_entries(&repo, &archive.name)?;
            cache::save(&archive.archive_id, &entries);
            Ok::<_, String>(build_versions(&archives, &file_path))
        },
        glib::clone!(
            #[weak]
            h,
            move |result: Result<Vec<FileVersion>, String>| match result {
                Ok(versions) => {
                    h.index_progress.set(h.index_progress.get() + 1);
                    set_versions(&h, versions);
                    index_next(&h, targets);
                }
                Err(e) => {
                    h.indexing.set(false);
                    update(&h);
                    kit::alert(&h.dialog, "Error", &e);
                }
            }
        ),
    );
}

// ------------------------------------------------------------------ actions

fn preview(h: &Rc<History>, version: &FileVersion) {
    let Some(entry) = version.entry.clone().filter(|e| !e.is_directory()) else { return };
    set_busy(h, true);
    let (repo, archive) = (h.repo.clone(), version.archive_name.clone());
    kit::bg(
        move || {
            let path = borg::extract_to_temp(&repo, &archive, &entry.path)?;
            let content = load_preview(&path);
            Ok::<_, String>((path, content))
        },
        glib::clone!(
            #[weak]
            h,
            move |result: Result<(PathBuf, PreviewContent), String>| {
                set_busy(&h, false);
                match result {
                    Ok((path, content)) => present_preview(&h.dialog, &path, content),
                    Err(e) => kit::alert(&h.dialog, "Error", &e),
                }
            }
        ),
    );
}

fn save_as(h: &Rc<History>, version: &FileVersion) {
    let Some(entry) = version.entry.as_ref().filter(|e| !e.is_directory()) else { return };
    let suggested = format!("{}.{}", entry.name(), version.archive_name);
    let version = version.clone();
    choose_save_path(
        &h.dialog,
        &suggested,
        glib::clone!(
            #[weak]
            h,
            move |destination| save_to(&h, &version, destination)
        ),
    );
}

fn save_to(h: &Rc<History>, version: &FileVersion, destination: PathBuf) {
    let Some(entry) = version.entry.clone().filter(|e| !e.is_directory()) else { return };
    set_busy(h, true);
    let (repo, archive) = (h.repo.clone(), version.archive_name.clone());
    kit::bg(
        move || extract_file_to(&repo, &archive, &entry.path, &destination),
        glib::clone!(
            #[weak]
            h,
            move |result: Result<(), String>| {
                set_busy(&h, false);
                if let Err(e) = result {
                    kit::alert(&h.dialog, "Error", &e);
                }
            }
        ),
    );
}

// ------------------------------------------------------------- context menu

fn install_context_menu(h: &Rc<History>) {
    let actions = gio::SimpleActionGroup::new();
    let add = |name: &str, f: fn(&Rc<History>, &FileVersion)| {
        let action = gio::SimpleAction::new(name, None);
        action.connect_activate(glib::clone!(
            #[weak]
            h,
            move |_, _| {
                if let Some(v) = h.menu_target.get().and_then(|p| version_at(&h, p)) {
                    f(&h, &v);
                }
            }
        ));
        actions.add_action(&action);
    };
    add("preview", preview);
    add("save", save_as);
    h.table.insert_action_group("version", Some(&actions));

    let menu = gio::Menu::new();
    menu.append(Some("Preview"), Some("version.preview"));
    menu.append(Some("Save as…"), Some("version.save"));
    let popover = gtk::PopoverMenu::from_model(Some(&menu));
    popover.set_parent(&h.table);
    popover.set_has_arrow(false);
    popover.set_halign(gtk::Align::Start);
    h.table.connect_destroy(glib::clone!(
        #[weak]
        popover,
        move |_| popover.unparent()
    ));

    let click = gtk::GestureClick::new();
    click.set_button(gdk::BUTTON_SECONDARY);
    click.connect_pressed(glib::clone!(
        #[weak]
        h,
        #[weak]
        popover,
        move |gesture, _, x, y| {
            let Some(position) = row_at(&h.table, x, y) else { return };
            gesture.set_state(gtk::EventSequenceState::Claimed);
            h.selection.set_selected(position);
            // Only versions that exist have a menu.
            if !version_at(&h, position).map(|v| v.exists()).unwrap_or(false) {
                return;
            }
            h.menu_target.set(Some(position));
            popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            popover.popup();
        }
    ));
    h.table.add_controller(click);
}

/// Row index under the pointer, found through the `history-cell` holders.
fn row_at(table: &gtk::ColumnView, x: f64, y: f64) -> Option<u32> {
    let mut widget = table.pick(x, y, gtk::PickFlags::DEFAULT)?;
    loop {
        if widget.has_css_class("history-cell") {
            return widget.widget_name().parse().ok();
        }
        if let Some(child) = widget.first_child().filter(|c| c.has_css_class("history-cell")) {
            return child.widget_name().parse().ok();
        }
        if &widget == table.upcast_ref::<gtk::Widget>() {
            return None;
        }
        widget = widget.parent()?;
    }
}
