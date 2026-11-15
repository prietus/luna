//! Folder-style browser of one archive (BorgMac's `ArchiveBrowserView`):
//! breadcrumb header, whole-tree search, preview, save/restore and the
//! per-file history. Also hosts the pieces the history and treemap views
//! share with it (index loading, navigation header, preview sheet, save-as).
use super::file_history;
use super::kit::{self, icons};
use super::state::AppState;
use crate::core::archive_tree::{ArchiveTree, NodeId, TreeNode, cache};
use crate::core::models::{Archive, Repository};
use crate::core::{borg, format};
use adw::prelude::*;
use gtk::{gdk, gio, glib};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::Arc;

const MAX_SEARCH_RESULTS: usize = 500;
const SEARCH_DEBOUNCE_MS: u64 = 200;
/// Text previews show at most this much of the file.
const MAX_TEXT_PREVIEW: usize = 256 * 1024;

// ------------------------------------------------------------ shared pieces

/// Loads the archive's index (on-disk cache first, `borg list` otherwise) and
/// builds the tree, all on a worker thread.
pub(super) fn load_tree(
    repo: &Repository,
    archive: &Archive,
    done: impl FnOnce(Result<Arc<ArchiveTree>, String>) + 'static,
) {
    let (repo, archive) = (repo.clone(), archive.clone());
    kit::bg(
        move || {
            let entries = match cache::load(&archive.archive_id) {
                Some(cached) => cached,
                None => {
                    let entries = borg::list_archive_entries(&repo, &archive.name)?;
                    cache::save(&archive.archive_id, &entries);
                    entries
                }
            };
            Ok(Arc::new(ArchiveTree::build(&entries)))
        },
        done,
    );
}

/// Header strip shared by the browser and the treemap: close, back, home,
/// archive name and the current path; `trailing` holds the right-hand items.
pub(super) struct NavHeader {
    pub root: gtk::Box,
    pub close: gtk::Button,
    pub back: gtk::Button,
    pub home: gtk::Button,
    pub trailing: gtk::Box,
    path: gtk::Label,
}

fn short_divider() -> gtk::Separator {
    let s = gtk::Separator::new(gtk::Orientation::Vertical);
    s.set_valign(gtk::Align::Center);
    s.set_height_request(16);
    s
}

pub(super) fn nav_header(archive_name: &str, back_tooltip: &str, home_tooltip: &str) -> NavHeader {
    let root = kit::hbox(8);
    root.add_css_class("browser-header");

    let close = gtk::Button::from_icon_name(icons::XMARK_CIRCLE);
    close.add_css_class("plain");
    close.add_css_class("secondary");
    close.set_tooltip_text(Some("Close"));
    close.set_valign(gtk::Align::Center);

    let nav = |icon: &str, tooltip: &str| {
        let b = gtk::Button::from_icon_name(icon);
        b.add_css_class("nav-button");
        b.set_valign(gtk::Align::Center);
        b.set_sensitive(false);
        if !tooltip.is_empty() {
            b.set_tooltip_text(Some(tooltip));
        }
        b
    };
    let back = nav(icons::CHEVRON_LEFT, back_tooltip);
    let home = nav(icons::HOME, home_tooltip);

    let name = kit::label(archive_name, &["heading"]);
    let slash = kit::label("/", &["tertiary"]);
    let path = kit::ellipsized("root", true, &["mono", "secondary"]);
    let trailing = kit::hbox(8);

    root.append(&close);
    root.append(&short_divider());
    root.append(&back);
    root.append(&home);
    root.append(&short_divider());
    root.append(&name);
    root.append(&slash);
    root.append(&path);
    root.append(&trailing);
    NavHeader { root, close, back, home, trailing, path }
}

impl NavHeader {
    pub fn set_path(&self, segments: &[String]) {
        let text = if segments.is_empty() { "root".to_string() } else { segments.join(" / ") };
        self.path.set_text(&text);
        self.back.set_sensitive(!segments.is_empty());
        self.home.set_sensitive(!segments.is_empty());
    }
}

/// Spinner over a title and an optional caption, centred (SwiftUI
/// `ProgressView("…")` + caption).
pub(super) fn loading_page(title: &str, caption: &str) -> gtk::Box {
    let page = kit::vbox(8);
    page.set_valign(gtk::Align::Center);
    page.set_halign(gtk::Align::Center);
    page.set_vexpand(true);
    let spinner = kit::spinner();
    spinner.set_halign(gtk::Align::Center);
    page.append(&spinner);
    let title = kit::label(title, &["secondary"]);
    title.set_halign(gtk::Align::Center);
    page.append(&title);
    if !caption.is_empty() {
        let caption = kit::caption(caption);
        caption.set_justify(gtk::Justification::Center);
        caption.set_halign(gtk::Align::Center);
        caption.set_max_width_chars(70);
        page.append(&caption);
    }
    page
}

/// "17/10/2026, 00:17" for borg's naive timestamps; the raw string when it
/// can't be parsed.
pub(super) fn short_borg_date(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    format::parse_borg_date(raw).map(|d| format::short_date_time(&d)).unwrap_or_else(|| raw.to_string())
}

/// Save panel (`NSSavePanel`) with a suggested file name.
pub(super) fn choose_save_path(
    parent: &impl IsA<gtk::Widget>,
    suggested_name: &str,
    on_chosen: impl FnOnce(PathBuf) + 'static,
) {
    let dialog = gtk::FileDialog::builder().title("Save as…").initial_name(suggested_name).modal(true).build();
    let window = parent.root().and_then(|r| r.downcast::<gtk::Window>().ok());
    dialog.save(window.as_ref(), gio::Cancellable::NONE, move |result| {
        if let Some(path) = result.ok().and_then(|f| f.path()) {
            on_chosen(path);
        }
    });
}

/// Extracts one file and moves it to `destination`, replacing what is there.
/// Blocking — call from a worker thread.
pub(super) fn extract_file_to(
    repo: &Repository,
    archive: &str,
    entry_path: &str,
    destination: &Path,
) -> Result<(), String> {
    let tmp = borg::extract_to_temp(repo, archive, entry_path)?;
    let _ = std::fs::remove_file(destination);
    if std::fs::rename(&tmp, destination).is_ok() {
        return Ok(());
    }
    // The temp dir is usually on another filesystem, where rename can't work.
    std::fs::copy(&tmp, destination).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&tmp);
    Ok(())
}

// ------------------------------------------------------------------ preview

/// What the in-app preview (the Quick Look stand-in) can show for a file.
pub(super) enum PreviewContent {
    Image(gdk::Texture),
    Text { text: String, truncated: bool },
    /// Anything else is handed to the default application.
    Other,
}

fn extension(name: &str) -> String {
    Path::new(name).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

/// Decodes the extracted file for previewing. Blocking — worker thread.
pub(super) fn load_preview(path: &Path) -> PreviewContent {
    let ext = extension(&path.to_string_lossy());
    if matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "tiff" | "tif" | "svg" | "ico") {
        if let Ok(texture) = gdk::Texture::from_filename(path) {
            return PreviewContent::Image(texture);
        }
    }
    read_text(path).unwrap_or(PreviewContent::Other)
}

fn read_text(path: &Path) -> Option<PreviewContent> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut data = Vec::new();
    file.take(MAX_TEXT_PREVIEW as u64 + 1).read_to_end(&mut data).ok()?;
    let truncated = data.len() > MAX_TEXT_PREVIEW;
    data.truncate(MAX_TEXT_PREVIEW);
    if data.contains(&0) {
        return None;
    }
    let text = match String::from_utf8(data) {
        Ok(text) => text,
        // A multi-byte character cut by the size cap is fine; anything else
        // means this isn't text.
        Err(e) if truncated && e.utf8_error().error_len().is_none() => {
            let valid = e.utf8_error().valid_up_to();
            String::from_utf8_lossy(&e.as_bytes()[..valid]).into_owned()
        }
        Err(_) => return None,
    };
    Some(PreviewContent::Text { text, truncated })
}

/// Shows the extracted file: images and text in a sheet with an "Open"
/// button, everything else straight in the default application.
pub(super) fn present_preview(parent: &impl IsA<gtk::Widget>, path: &Path, content: PreviewContent) {
    if matches!(content, PreviewContent::Other) {
        kit::open_path(path);
        return;
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (dialog, root) = kit::bare_sheet(&name, 760, 560);

    let header = kit::hbox(8);
    header.add_css_class("browser-header");
    header.append(&kit::icon_styled(icon_for_name(&name), &["secondary"]));
    let title = kit::label(&name, &["heading"]);
    title.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    header.append(&title);
    if let Ok(meta) = std::fs::metadata(path) {
        header.append(&kit::label(&format::bytes_file(meta.len() as i64), &["caption", "secondary", "numeric"]));
    }
    header.append(&kit::spacer());
    let open = kit::button("Open");
    open.set_tooltip_text(Some("Open with the default application"));
    let close = kit::button("Close");
    header.append(&open);
    header.append(&close);
    root.append(&header);
    root.append(&kit::divider());

    match content {
        PreviewContent::Image(texture) => {
            let picture = gtk::Picture::for_paintable(&texture);
            picture.set_can_shrink(true);
            picture.set_content_fit(gtk::ContentFit::ScaleDown);
            picture.set_vexpand(true);
            picture.set_hexpand(true);
            picture.add_css_class("preview-image");
            root.append(&picture);
        }
        PreviewContent::Text { text, truncated } => {
            let view = gtk::TextView::new();
            view.set_editable(false);
            view.set_cursor_visible(false);
            view.set_monospace(true);
            view.add_css_class("preview-text");
            view.buffer().set_text(&text);
            let scroller = kit::scrolled(&view);
            scroller.set_hscrollbar_policy(gtk::PolicyType::Automatic);
            root.append(&scroller);
            if truncated {
                root.append(&kit::divider());
                let note = kit::caption(&format!(
                    "Showing the first {}. Open the file to see all of it.",
                    format::bytes_binary(MAX_TEXT_PREVIEW as i64)
                ));
                note.add_css_class("preview-note");
                root.append(&note);
            }
        }
        PreviewContent::Other => {}
    }

    let path = path.to_path_buf();
    open.connect_clicked(move |_| kit::open_path(&path));
    close.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| {
            dialog.close();
        }
    ));
    dialog.present(Some(parent));
}

// -------------------------------------------------------------------- icons

fn icon_for_name(name: &str) -> &'static str {
    match extension(name).as_str() {
        "jpg" | "jpeg" | "png" | "heic" | "gif" | "tiff" | "bmp" | "webp" => "image-x-generic-symbolic", // photo
        "mp4" | "mov" | "m4v" | "avi" | "mkv" => "video-x-generic-symbolic",                            // film
        "mp3" | "m4a" | "wav" | "flac" | "aac" => "audio-x-generic-symbolic",                           // music.note
        "pdf" => "x-office-document-symbolic",                                                          // doc.richtext
        "txt" | "md" | "log" => "x-office-document-symbolic",                                           // doc.text
        "swift" | "py" | "rb" | "js" | "ts" | "go" | "rs" | "c" | "cpp" | "h" | "json" | "yaml" | "yml" | "toml" => {
            "utilities-terminal-symbolic" // chevron.left.forwardslash.chevron.right
        }
        "zip" | "tar" | "gz" | "bz2" | "7z" => icons::ARCHIVE, // archivebox
        _ => icons::FILE,                                      // doc
    }
}

fn icon_for(node: &TreeNode) -> &'static str {
    if node.is_directory() {
        return "folder"; // folder.fill (Adwaita's full-colour blue folder)
    }
    if node.entry.as_ref().map(|e| e.is_symlink()).unwrap_or(false) {
        return "go-jump-symbolic"; // arrow.turn.up.right
    }
    icon_for_name(&node.name)
}

// ------------------------------------------------------------------ browser

struct Browser {
    app: Rc<AppState>,
    repo: Repository,
    archive: Archive,
    dialog: adw::Dialog,
    tree: RefCell<Option<Arc<ArchiveTree>>>,
    path: RefCell<Vec<String>>,
    loading: Cell<bool>,
    /// Extractions in flight (the header spinner shows while > 0).
    busy: Cell<u32>,
    /// Whether the list currently holds search hits (two-line rows).
    showing_search: Cell<bool>,
    search_generation: Cell<u64>,
    debounce: RefCell<Option<glib::SourceId>>,
    /// Node the context menu was opened on.
    menu_target: Cell<Option<NodeId>>,
    /// File to select once its parent folder is listed (after "reveal").
    pending_selection: Cell<Option<NodeId>>,
    header: NavHeader,
    count: gtk::Label,
    spinner: gtk::Spinner,
    search: gtk::Text,
    clear: gtk::Button,
    results: gtk::Label,
    stack: gtk::Stack,
    no_results: adw::StatusPage,
    store: gio::ListStore,
    selection: gtk::SingleSelection,
    list: gtk::ListView,
}

/// Lets other code (the debug hooks) drive an open browser.
pub struct Handle(Weak<Browser>);

impl Handle {
    fn with_node(&self, path: &str, f: impl FnOnce(&Rc<Browser>, NodeId)) {
        let Some(b) = self.0.upgrade() else { return };
        let Some(tree) = b.tree.borrow().clone() else { return };
        let segments: Vec<String> = path.split('/').filter(|s| !s.is_empty()).map(String::from).collect();
        let id = tree.node_at(&segments);
        if tree.segments_of(id).len() == segments.len() {
            f(&b, id);
        }
    }

    /// Shows the folder at `path` ("a/b/c").
    pub fn navigate(&self, path: &str) {
        self.with_node(path, |b, id| {
            let segments = b.tree.borrow().as_ref().map(|t| t.segments_of(id)).unwrap_or_default();
            b.path.replace(segments);
            refresh(b);
        });
    }

    pub fn search(&self, query: &str) {
        if let Some(b) = self.0.upgrade() {
            b.search.set_text(query);
        }
    }

    pub fn preview(&self, path: &str) {
        self.with_node(path, preview);
    }

    /// Leaves the search and shows the entry at `path` in its folder.
    pub fn reveal(&self, path: &str) {
        self.with_node(path, reveal);
    }

    pub fn history(&self, path: &str) {
        self.with_node(path, history);
    }

    /// Restores the file or folder at `path` into `destination` without asking.
    pub fn restore_to(&self, path: &str, destination: &Path) {
        self.with_node(path, |b, id| restore_into(b, id, destination.to_path_buf()));
    }

    /// Saves the file at `path` as `destination` without asking.
    pub fn save_to(&self, path: &str, destination: &Path) {
        self.with_node(path, |b, id| save_to(b, id, destination.to_path_buf()));
    }
}

pub fn present(parent: &impl IsA<gtk::Widget>, app: &Rc<AppState>, repo: &Repository, archive: &Archive) {
    open(parent, app, repo, archive);
}

pub fn open(parent: &impl IsA<gtk::Widget>, app: &Rc<AppState>, repo: &Repository, archive: &Archive) -> Handle {
    let (dialog, root) = kit::bare_sheet(&archive.name, 820, 560);

    // ------------------------------------------------------------ breadcrumb
    let header = nav_header(&archive.name, "Back", "Root");
    let count = kit::label("", &["caption", "secondary", "numeric"]);
    count.set_visible(false);
    let spinner = kit::spinner();
    spinner.set_visible(false);
    header.trailing.append(&count);
    header.trailing.append(&spinner);
    root.append(&header.root);
    root.append(&kit::divider());

    // ------------------------------------------------------------ search bar
    let search_bar = kit::hbox(8);
    search_bar.add_css_class("browser-search");
    search_bar.append(&kit::icon_styled(icons::SEARCH, &["secondary"]));
    let search = gtk::Text::new();
    search.set_placeholder_text(Some("Search the whole archive (name or path)…"));
    search.set_hexpand(true);
    let clear = gtk::Button::from_icon_name(icons::XMARK_CIRCLE);
    clear.add_css_class("plain");
    clear.add_css_class("tertiary");
    clear.set_valign(gtk::Align::Center);
    clear.set_visible(false);
    let results = kit::label("", &["caption", "secondary", "numeric"]);
    results.set_visible(false);
    search_bar.append(&search);
    search_bar.append(&clear);
    search_bar.append(&results);
    root.append(&search_bar);
    root.append(&kit::divider());

    // --------------------------------------------------------------- content
    let store = gio::ListStore::new::<glib::BoxedAnyObject>();
    let selection = gtk::SingleSelection::new(Some(store.clone()));
    selection.set_autoselect(false);
    selection.set_can_unselect(true);
    let factory = gtk::SignalListItemFactory::new();
    let list = gtk::ListView::new(Some(selection.clone()), Some(factory.clone()));
    list.add_css_class("mac-list");
    list.add_css_class("browser-list");

    let no_results = kit::empty_state(icons::SEARCH, "No results", "");
    let stack = gtk::Stack::new();
    stack.set_vexpand(true);
    stack.add_named(
        &loading_page(
            "Loading archive index…",
            "The first time takes a while depending on size. Subsequent loads are instant (on-disk cache).",
        ),
        Some("loading"),
    );
    stack.add_named(&kit::scrolled(&list), Some("list"));
    stack.add_named(&kit::empty_state(icons::FOLDER, "Empty folder", "No entries at this path."), Some("empty"));
    stack.add_named(&kit::empty_state("dialog-question-symbolic", "No data", "Could not load the index."), Some("no-data"));
    stack.add_named(&no_results, Some("no-results"));
    root.append(&stack);

    let b = Rc::new(Browser {
        app: app.clone(),
        repo: repo.clone(),
        archive: archive.clone(),
        dialog: dialog.clone(),
        tree: RefCell::new(None),
        path: RefCell::new(vec![]),
        loading: Cell::new(false),
        busy: Cell::new(0),
        showing_search: Cell::new(false),
        search_generation: Cell::new(0),
        debounce: RefCell::new(None),
        menu_target: Cell::new(None),
        pending_selection: Cell::new(None),
        header,
        count,
        spinner,
        search,
        clear,
        results,
        stack,
        no_results,
        store,
        selection,
        list,
    });

    factory.connect_setup(|_, obj| {
        if let Some(item) = obj.downcast_ref::<gtk::ListItem>() {
            item.set_child(Some(&build_row()));
        }
    });
    factory.connect_bind(glib::clone!(
        #[weak]
        b,
        move |_, obj| {
            let Some(item) = obj.downcast_ref::<gtk::ListItem>() else { return };
            let (Some(row), Some(id), Some(tree)) =
                (item.child(), item.item().and_then(|o| kit::unbox::<NodeId>(&o)), b.tree.borrow().clone())
            else {
                return;
            };
            // The context menu maps a click back to its row through this name.
            row.set_widget_name(&item.position().to_string());
            bind_row(&row, tree.get(id), b.showing_search.get());
        }
    ));

    b.header.close.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| {
            dialog.close();
        }
    ));
    b.header.back.connect_clicked(glib::clone!(
        #[weak]
        b,
        move |_| {
            b.path.borrow_mut().pop();
            refresh(&b);
        }
    ));
    b.header.home.connect_clicked(glib::clone!(
        #[weak]
        b,
        move |_| {
            b.path.borrow_mut().clear();
            refresh(&b);
        }
    ));
    b.clear.connect_clicked(glib::clone!(
        #[weak]
        b,
        move |_| b.search.set_text("")
    ));
    b.search.connect_changed(glib::clone!(
        #[weak]
        b,
        move |_| query_changed(&b)
    ));

    // Double click / Return.
    b.list.connect_activate(glib::clone!(
        #[weak]
        b,
        move |_, position| {
            let Some(id) = b.store.item(position).and_then(|o| kit::unbox::<NodeId>(&o)) else { return };
            if b.showing_search.get() { reveal(&b, id) } else { activate(&b, id) }
        }
    ));

    install_context_menu(&b);

    // The controller lives as long as the sheet is open.
    let keep = RefCell::new(Some(b.clone()));
    dialog.connect_closed(move |_| {
        if let Some(b) = keep.borrow_mut().take() {
            if let Some(id) = b.debounce.borrow_mut().take() {
                id.remove();
            }
        }
    });

    dialog.present(Some(parent));
    load(&b);
    Handle(Rc::downgrade(&b))
}

// --------------------------------------------------------------------- rows

fn build_row() -> gtk::Box {
    let row = kit::hbox(10);
    row.add_css_class("browser-row");
    let icon = gtk::Image::new();
    icon.set_width_request(18);
    row.append(&icon);

    let texts = kit::vbox(1);
    texts.set_valign(gtk::Align::Center);
    texts.set_hexpand(true);
    texts.append(&kit::ellipsized("", true, &[]));
    let sub = kit::ellipsized("", false, &["caption", "mono", "secondary"]);
    sub.set_ellipsize(gtk::pango::EllipsizeMode::Start);
    texts.append(&sub);
    row.append(&texts);

    row.append(&kit::label("", &["caption", "secondary", "numeric"]));
    let date = kit::label("", &["caption", "secondary", "numeric"]);
    date.set_width_request(130);
    date.set_xalign(1.0);
    row.append(&date);
    row.append(&kit::icon_styled(icons::CHEVRON_RIGHT, &["tertiary", "browser-chevron"]));
    row
}

fn children_of(widget: &gtk::Widget) -> Vec<gtk::Widget> {
    let mut out = vec![];
    let mut child = widget.first_child();
    while let Some(c) = child {
        child = c.next_sibling();
        out.push(c);
    }
    out
}

fn set_label(widget: &gtk::Widget, text: &str) {
    if let Some(label) = widget.downcast_ref::<gtk::Label>() {
        label.set_text(text);
    }
}

/// Fills a row built by `build_row`. Search hits show the full path under
/// the name and no date/chevron, like the Swift `searchRow`.
fn bind_row(row: &gtk::Widget, node: &TreeNode, search: bool) {
    let parts = children_of(row);
    let [icon, texts, size, date, chevron] = parts.as_slice() else { return };
    let is_dir = node.is_directory();
    if let Some(icon) = icon.downcast_ref::<gtk::Image>() {
        icon.set_icon_name(Some(icon_for(node)));
        if is_dir {
            icon.remove_css_class("secondary");
            icon.add_css_class("tint");
        } else {
            icon.remove_css_class("tint");
            icon.add_css_class("secondary");
        }
    }
    if let [name, sub] = children_of(texts).as_slice() {
        set_label(name, &node.name);
        set_label(sub, &node.path);
        sub.set_visible(search);
    }
    size.set_visible(!is_dir);
    set_label(size, &format::bytes_file(node.entry.as_ref().map(|e| e.size_bytes()).unwrap_or(0) as i64));
    date.set_visible(!search);
    set_label(date, &short_borg_date(node.entry.as_ref().and_then(|e| e.mtime.as_deref()).unwrap_or("")));
    chevron.set_visible(is_dir && !search);
}

// ------------------------------------------------------------------ loading

fn load(b: &Rc<Browser>) {
    if b.tree.borrow().is_some() {
        return;
    }
    b.loading.set(true);
    refresh(b);
    load_tree(
        &b.repo,
        &b.archive,
        glib::clone!(
            #[weak]
            b,
            move |result: Result<Arc<ArchiveTree>, String>| {
                b.loading.set(false);
                match result {
                    Ok(tree) => {
                        b.tree.replace(Some(tree));
                        refresh(&b);
                    }
                    Err(e) => {
                        refresh(&b);
                        kit::alert(&b.dialog, "Error", &e);
                    }
                }
            }
        ),
    );
}

// ---------------------------------------------------------------- rendering

fn query(b: &Browser) -> String {
    b.search.text().trim().to_lowercase()
}

/// Brings header, search bar and content in line with the current state.
fn refresh(b: &Rc<Browser>) {
    let segments = b.path.borrow().clone();
    b.header.set_path(&segments);
    let tree = b.tree.borrow().clone();
    let current = tree.as_ref().map(|t| (t.clone(), t.node_at(&segments)));
    match &current {
        Some((tree, id)) => {
            b.count.set_text(&format!("{} items", tree.get(*id).children.len()));
            b.count.set_visible(true);
        }
        None => b.count.set_visible(false),
    }

    let q = query(b);
    b.clear.set_visible(!q.is_empty());
    b.results.set_visible(!q.is_empty());

    if b.loading.get() {
        b.stack.set_visible_child_name("loading");
    } else if !q.is_empty() {
        run_search(b, tree, q);
    } else if let Some((tree, id)) = current {
        // Invalidate any search still running for an earlier query.
        b.search_generation.set(b.search_generation.get() + 1);
        let children = tree.get(id).children.clone();
        if children.is_empty() {
            b.stack.set_visible_child_name("empty");
        } else {
            b.showing_search.set(false);
            let select = b.pending_selection.take().and_then(|n| children.iter().position(|&c| c == n));
            fill(b, &children, select);
            b.stack.set_visible_child_name("list");
        }
    } else {
        b.stack.set_visible_child_name("no-data");
    }
}

fn fill(b: &Browser, ids: &[NodeId], select: Option<usize>) {
    let items: Vec<glib::BoxedAnyObject> = ids.iter().map(|&id| kit::boxed(id)).collect();
    b.store.splice(0, b.store.n_items(), &items);
    match select {
        Some(position) => {
            b.list.scroll_to(position as u32, gtk::ListScrollFlags::SELECT | gtk::ListScrollFlags::FOCUS, None)
        }
        None => {
            b.selection.set_selected(gtk::INVALID_LIST_POSITION);
            if !ids.is_empty() {
                b.list.scroll_to(0, gtk::ListScrollFlags::NONE, None);
            }
        }
    }
}

fn query_changed(b: &Rc<Browser>) {
    if let Some(id) = b.debounce.borrow_mut().take() {
        id.remove();
    }
    if query(b).is_empty() {
        refresh(b);
        return;
    }
    b.clear.set_visible(true);
    // The whole tree is walked per query, so wait for a pause in the typing.
    let id = glib::timeout_add_local_once(
        std::time::Duration::from_millis(SEARCH_DEBOUNCE_MS),
        glib::clone!(
            #[weak]
            b,
            move || {
                b.debounce.replace(None);
                refresh(&b);
            }
        ),
    );
    b.debounce.replace(Some(id));
}

/// Depth-first walk in display order, capped at `MAX_SEARCH_RESULTS` hits.
fn search_tree(tree: &ArchiveTree, q: &str) -> Vec<NodeId> {
    let mut hits = Vec::with_capacity(64);
    let mut stack: Vec<NodeId> = tree.root().children.iter().rev().copied().collect();
    while let Some(id) = stack.pop() {
        if hits.len() >= MAX_SEARCH_RESULTS {
            break;
        }
        let node = tree.get(id);
        // The name is the tail of the path, so one test covers "name or path".
        if node.path.to_lowercase().contains(q) {
            hits.push(id);
        }
        stack.extend(node.children.iter().rev());
    }
    hits
}

fn run_search(b: &Rc<Browser>, tree: Option<Arc<ArchiveTree>>, q: String) {
    let generation = b.search_generation.get() + 1;
    b.search_generation.set(generation);
    let raw = b.search.text().to_string();
    kit::bg(
        move || tree.map(|t| search_tree(&t, &q)).unwrap_or_default(),
        glib::clone!(
            #[weak]
            b,
            move |hits: Vec<NodeId>| {
                if b.search_generation.get() != generation || b.loading.get() {
                    return;
                }
                let plus = if hits.len() >= MAX_SEARCH_RESULTS { "+" } else { "" };
                b.results.set_text(&format!("{}{plus} results", hits.len()));
                if hits.is_empty() {
                    b.no_results.set_description(Some(&glib::markup_escape_text(&format!(
                        "No file or folder matches \"{raw}\"."
                    ))));
                    b.stack.set_visible_child_name("no-results");
                } else {
                    b.showing_search.set(true);
                    fill(&b, &hits, None);
                    b.stack.set_visible_child_name("list");
                }
            }
        ),
    );
}

// ------------------------------------------------------------------ actions

fn node_of(b: &Browser, id: NodeId) -> Option<TreeNode> {
    b.tree.borrow().as_ref().map(|t| t.get(id).clone())
}

fn set_busy(b: &Browser, on: bool) {
    let n = if on { b.busy.get() + 1 } else { b.busy.get().saturating_sub(1) };
    b.busy.set(n);
    b.spinner.set_visible(n > 0);
}

fn activate(b: &Rc<Browser>, id: NodeId) {
    let Some(node) = node_of(b, id) else { return };
    if node.is_directory() {
        b.path.borrow_mut().push(node.name);
        refresh(b);
    } else {
        preview(b, id);
    }
}

/// Navigates the file browser to the parent folder of the given search hit
/// and clears the search query so the user sees the file in context.
fn reveal(b: &Rc<Browser>, id: NodeId) {
    let Some(tree) = b.tree.borrow().clone() else { return };
    let mut segments = tree.segments_of(id);
    if !tree.get(id).is_directory() {
        // Drop the filename so we land in the parent.
        segments.pop();
        b.pending_selection.set(Some(id));
    }
    b.path.replace(segments);
    if b.search.text().is_empty() {
        refresh(b);
    } else {
        b.search.set_text("");
    }
}

fn preview(b: &Rc<Browser>, id: NodeId) {
    let Some(node) = node_of(b, id) else { return };
    let Some(entry) = node.entry.filter(|e| !e.is_directory()) else { return };
    set_busy(b, true);
    let (repo, archive) = (b.repo.clone(), b.archive.name.clone());
    kit::bg(
        move || {
            let path = borg::extract_to_temp(&repo, &archive, &entry.path)?;
            let content = load_preview(&path);
            Ok::<_, String>((path, content))
        },
        glib::clone!(
            #[weak]
            b,
            move |result: Result<(PathBuf, PreviewContent), String>| {
                set_busy(&b, false);
                match result {
                    Ok((path, content)) => present_preview(&b.dialog, &path, content),
                    Err(e) => kit::alert(&b.dialog, "Error", &e),
                }
            }
        ),
    );
}

fn history(b: &Rc<Browser>, id: NodeId) {
    if let Some(node) = node_of(b, id) {
        file_history::present(&b.dialog, &b.app, &b.repo, &node.path);
    }
}

fn save_as(b: &Rc<Browser>, id: NodeId) {
    let Some(node) = node_of(b, id) else { return };
    if node.entry.is_none() || node.is_directory() {
        return;
    }
    choose_save_path(
        &b.dialog,
        &node.name,
        glib::clone!(
            #[weak]
            b,
            move |destination| save_to(&b, id, destination)
        ),
    );
}

fn save_to(b: &Rc<Browser>, id: NodeId, destination: PathBuf) {
    let Some(node) = node_of(b, id) else { return };
    let Some(entry) = node.entry.filter(|e| !e.is_directory()) else { return };
    set_busy(b, true);
    let (repo, archive) = (b.repo.clone(), b.archive.name.clone());
    kit::bg(
        move || extract_file_to(&repo, &archive, &entry.path, &destination),
        glib::clone!(
            #[weak]
            b,
            move |result: Result<(), String>| {
                set_busy(&b, false);
                if let Err(e) = result {
                    kit::alert(&b.dialog, "Error", &e);
                }
            }
        ),
    );
}

/// Restores a file or a whole folder into a folder the user picks, keeping
/// its path inside the archive (what `borg extract` does).
fn restore(b: &Rc<Browser>, id: NodeId) {
    let Some(node) = node_of(b, id) else { return };
    kit::choose_folder(
        &b.dialog,
        &format!("Restore \"{}\" to…", node.name),
        "Restore",
        glib::clone!(
            #[weak]
            b,
            move |destination| restore_into(&b, id, destination)
        ),
    );
}

fn restore_into(b: &Rc<Browser>, id: NodeId, destination: PathBuf) {
    let Some(node) = node_of(b, id) else { return };
    let entry_path = node.entry.as_ref().map(|e| e.path.clone()).unwrap_or_else(|| node.path.clone());
    set_busy(b, true);
    let (repo, archive) = (b.repo.clone(), b.archive.name.clone());
    let shown = destination.clone();
    kit::bg(
        move || borg::extract_paths(&repo, &archive, &[entry_path], &destination),
        glib::clone!(
            #[weak]
            b,
            move |result: Result<(), String>| {
                set_busy(&b, false);
                match result {
                    Ok(()) => kit::alert(
                        &b.dialog,
                        "Restore complete",
                        &format!("\"{}\" was restored to {}.", node.path, shown.display()),
                    ),
                    Err(e) => kit::alert(&b.dialog, "Error", &e),
                }
            }
        ),
    );
}

// ------------------------------------------------------------- context menu

fn install_context_menu(b: &Rc<Browser>) {
    let actions = gio::SimpleActionGroup::new();
    let add = |name: &str, f: fn(&Rc<Browser>, NodeId)| {
        let action = gio::SimpleAction::new(name, None);
        action.connect_activate(glib::clone!(
            #[weak]
            b,
            move |_, _| {
                if let Some(id) = b.menu_target.get() {
                    f(&b, id);
                }
            }
        ));
        actions.add_action(&action);
    };
    add("open", activate);
    add("reveal", reveal);
    add("preview", preview);
    add("history", history);
    add("save", save_as);
    add("restore", restore);
    b.list.insert_action_group("entry", Some(&actions));

    let popover = gtk::PopoverMenu::from_model(gio::MenuModel::NONE);
    popover.set_parent(&b.list);
    popover.set_has_arrow(false);
    popover.set_halign(gtk::Align::Start);
    b.list.connect_destroy(glib::clone!(
        #[weak]
        popover,
        move |_| popover.unparent()
    ));

    let click = gtk::GestureClick::new();
    click.set_button(gdk::BUTTON_SECONDARY);
    click.connect_pressed(glib::clone!(
        #[weak]
        b,
        #[weak]
        popover,
        move |gesture, _, x, y| {
            let Some(position) = row_at(&b.list, x, y) else { return };
            let Some(id) = b.store.item(position).and_then(|o| kit::unbox::<NodeId>(&o)) else { return };
            let Some(node) = node_of(&b, id) else { return };
            gesture.set_state(gtk::EventSequenceState::Claimed);
            b.selection.set_selected(position);
            b.menu_target.set(Some(id));

            let search = b.showing_search.get();
            let menu = gio::Menu::new();
            let main = gio::Menu::new();
            if node.is_directory() {
                if search {
                    main.append(Some("Go to folder"), Some("entry.reveal"));
                } else {
                    main.append(Some("Open"), Some("entry.open"));
                }
            } else {
                main.append(Some("Preview"), Some("entry.preview"));
                if search {
                    main.append(Some("Go to parent folder"), Some("entry.reveal"));
                }
                main.append(Some("History…"), Some("entry.history"));
                main.append(Some("Save as…"), Some("entry.save"));
            }
            menu.append_section(None, &main);
            let recover = gio::Menu::new();
            recover.append(Some("Restore to…"), Some("entry.restore"));
            menu.append_section(None, &recover);
            popover.set_menu_model(Some(&menu));
            popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            popover.popup();
        }
    ));
    b.list.add_controller(click);
}

/// Row index under the pointer, found through the `browser-row` boxes.
fn row_at(list: &gtk::ListView, x: f64, y: f64) -> Option<u32> {
    let mut widget = list.pick(x, y, gtk::PickFlags::DEFAULT)?;
    loop {
        if widget.has_css_class("browser-row") {
            return widget.widget_name().parse().ok();
        }
        if let Some(child) = widget.first_child().filter(|c| c.has_css_class("browser-row")) {
            return child.widget_name().parse().ok();
        }
        if &widget == list.upcast_ref::<gtk::Widget>() {
            return None;
        }
        widget = widget.parent()?;
    }
}
