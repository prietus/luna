//! Small widget toolkit shared by every view so the app looks and behaves
//! consistently (macOS-style sheets, forms, badges, alerts, file pickers).
use adw::prelude::*;
use gtk::{gio, glib};
use std::path::PathBuf;

// ----------------------------------------------------------- background work

/// Runs blocking `work` on a worker thread and delivers its result to `done`
/// on the main thread. Everything in `crate::core` is blocking; this is the
/// one bridge the UI uses to call it.
pub fn bg<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static, done: impl FnOnce(T) + 'static) {
    glib::spawn_future_local(async move {
        if let Ok(result) = gio::spawn_blocking(work).await {
            done(result);
        }
    });
}

/// Runs `f` on the main thread after `ms` milliseconds.
pub fn after(ms: u64, f: impl FnOnce() + 'static) {
    glib::timeout_add_local_once(std::time::Duration::from_millis(ms), f);
}

// --------------------------------------------------------------------- icons

/// SF Symbol → icon name. Custom `borg-*` icons ship with the app.
pub mod icons {
    pub const DRIVE: &str = "borg-drive-symbolic"; // externaldrive.fill
    pub const CLOUD: &str = "borg-cloud-symbolic"; // cloud / cloud.fill
    pub const SERVER: &str = "borg-server-symbolic"; // server.rack
    pub const CHART: &str = "borg-chart-symbolic"; // chart.bar.xaxis / chart.bar.fill
    pub const ARCHIVE: &str = "borg-archive-symbolic"; // archivebox.fill
    pub const BOX: &str = "package-x-generic-symbolic"; // shippingbox.fill
    pub const PLUS: &str = "list-add-symbolic"; // plus
    pub const PLUS_CIRCLE: &str = "borg-plus-circle-symbolic"; // plus.circle
    pub const MINUS_CIRCLE: &str = "borg-minus-circle-symbolic"; // minus.circle(.fill)
    pub const XMARK_CIRCLE: &str = "borg-xmark-circle-symbolic"; // xmark.circle.fill
    pub const CHECK_CIRCLE: &str = "borg-check-circle-symbolic"; // checkmark.circle.fill
    pub const PLAY_CIRCLE: &str = "borg-play-circle-symbolic"; // play.circle.fill
    pub const ELLIPSIS_CIRCLE: &str = "borg-ellipsis-circle-symbolic"; // ellipsis.circle
    pub const SCISSORS: &str = "edit-cut-symbolic"; // scissors
    pub const HISTORY: &str = "document-open-recent-symbolic"; // clock.arrow.circlepath
    pub const KEY: &str = "dialog-password-symbolic"; // key.fill
    pub const LOCK: &str = "changes-prevent-symbolic"; // lock.fill
    pub const REFRESH: &str = "view-refresh-symbolic"; // arrow.clockwise
    pub const SYNC: &str = "media-playlist-repeat-symbolic"; // arrow.triangle.2.circlepath
    pub const WARNING: &str = "dialog-warning-symbolic"; // exclamationmark.triangle.fill
    pub const TRASH: &str = "user-trash-symbolic"; // trash
    pub const NOSIGN: &str = "action-unavailable-symbolic"; // nosign
    pub const HOME: &str = "go-home-symbolic"; // house
    pub const FOLDER: &str = "folder-symbolic"; // folder
    pub const FILE: &str = "text-x-generic-symbolic"; // doc
    pub const CHEVRON_RIGHT: &str = "go-next-symbolic"; // chevron.right
    pub const CHEVRON_LEFT: &str = "go-previous-symbolic"; // chevron.left
    pub const BELL: &str = "preferences-system-notifications-symbolic"; // bell / bell.badge
    pub const BELL_SLASH: &str = "notifications-disabled-symbolic"; // bell.slash
    pub const DOWNLOAD: &str = "document-save-symbolic"; // square.and.arrow.down.fill
    pub const PENCIL: &str = "document-edit-symbolic"; // pencil
    pub const SEARCH: &str = "system-search-symbolic"; // magnifyingglass
    pub const HOURGLASS: &str = "content-loading-symbolic"; // hourglass
    pub const DRIVE_WRENCH: &str = "drive-harddisk-system-symbolic"; // externaldrive.badge.wrench
    pub const CLOCK_QUESTION: &str = "appointment-missed-symbolic"; // clock.badge.questionmark
    pub const CALENDAR_CLOCK: &str = "x-office-calendar-symbolic"; // calendar.badge.clock
    pub const BOLT: &str = "network-transmit-receive-symbolic"; // bolt.horizontal.fill
    pub const TRAY: &str = "folder-open-symbolic"; // tray (empty state)
    pub const WAND: &str = "starred-symbolic"; // wand.and.stars
    pub const SIDEBAR: &str = "sidebar-show-symbolic"; // sidebar.left
    pub const CLOSE: &str = "window-close-symbolic"; // xmark
    pub const COPY: &str = "edit-copy-symbolic"; // doc.on.doc
    pub const EYE: &str = "view-reveal-symbolic"; // eye
}

pub fn icon(name: &str) -> gtk::Image {
    gtk::Image::from_icon_name(name)
}

/// Icon with CSS classes, e.g. `icon_styled(icons::WARNING, &["c-orange", "big-icon"])`.
pub fn icon_styled(name: &str, classes: &[&str]) -> gtk::Image {
    let img = gtk::Image::from_icon_name(name);
    for c in classes {
        img.add_css_class(c);
    }
    img
}

/// Borderless icon-only button (SwiftUI `.buttonStyle(.plain)` / toolbar item).
pub fn icon_button(icon_name: &str, tooltip: &str) -> gtk::Button {
    let b = gtk::Button::from_icon_name(icon_name);
    b.add_css_class("flat");
    if !tooltip.is_empty() {
        b.set_tooltip_text(Some(tooltip));
    }
    b
}

// -------------------------------------------------------------------- labels

pub fn label(text: &str, classes: &[&str]) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.set_xalign(0.0);
    for c in classes {
        l.add_css_class(c);
    }
    l
}

/// Small secondary wrapping text (`.font(.caption).foregroundStyle(.secondary)`).
pub fn caption(text: &str) -> gtk::Label {
    let l = label(text, &["caption", "secondary"]);
    l.set_wrap(true);
    l.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    l
}

/// Same as `caption` but parses Pango markup (`<b>…</b>`).
pub fn caption_markup(markup: &str) -> gtk::Label {
    let l = caption("");
    l.set_markup(markup);
    l
}

/// Single-line label that ellipsizes in the middle (paths) or at the end.
pub fn ellipsized(text: &str, middle: bool, classes: &[&str]) -> gtk::Label {
    let l = label(text, classes);
    l.set_ellipsize(if middle { gtk::pango::EllipsizeMode::Middle } else { gtk::pango::EllipsizeMode::End });
    l.set_hexpand(true);
    l
}

/// Hidden-until-set red caption for inline errors.
pub fn error_label() -> gtk::Label {
    let l = label("", &["caption", "error-text"]);
    l.set_wrap(true);
    l.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    l.set_selectable(true);
    l.set_visible(false);
    l
}

pub fn set_error(l: &gtk::Label, message: Option<&str>) {
    l.set_text(message.unwrap_or(""));
    l.set_visible(message.map(|m| !m.is_empty()).unwrap_or(false));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tint {
    Blue,
    Orange,
    Green,
    Red,
    Yellow,
    Purple,
    Gray,
}

impl Tint {
    pub fn name(self) -> &'static str {
        match self {
            Tint::Blue => "blue",
            Tint::Orange => "orange",
            Tint::Green => "green",
            Tint::Red => "red",
            Tint::Yellow => "yellow",
            Tint::Purple => "purple",
            Tint::Gray => "gray",
        }
    }
    /// Foreground colour class (`c-blue`…).
    pub fn fg_class(self) -> String {
        format!("c-{}", self.name())
    }
    /// RGB components for Cairo drawing (dark-mode system colours).
    pub fn rgb(self) -> (f64, f64, f64) {
        match self {
            Tint::Blue => (0.04, 0.52, 1.0),
            Tint::Orange => (1.0, 0.62, 0.04),
            Tint::Green => (0.19, 0.82, 0.35),
            Tint::Red => (1.0, 0.27, 0.23),
            Tint::Yellow => (1.0, 0.84, 0.04),
            Tint::Purple => (0.75, 0.35, 0.95),
            Tint::Gray => (0.6, 0.6, 0.62),
        }
    }
}

/// Capsule badge (`Text(..).background(tint.opacity(0.18), in: Capsule())`).
pub fn badge(text: &str, tint: Tint) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.add_css_class("badge");
    l.add_css_class(&format!("b-{}", tint.name()));
    l.set_halign(gtk::Align::Start);
    l.set_valign(gtk::Align::Center);
    l
}

pub fn set_badge(l: &gtk::Label, text: &str, tint: Tint) {
    l.set_text(text);
    for t in ["blue", "orange", "green", "red", "yellow", "purple", "gray"] {
        l.remove_css_class(&format!("b-{t}"));
    }
    l.add_css_class(&format!("b-{}", tint.name()));
}

// ------------------------------------------------------------------- layout

pub fn vbox(spacing: i32) -> gtk::Box {
    gtk::Box::new(gtk::Orientation::Vertical, spacing)
}

pub fn hbox(spacing: i32) -> gtk::Box {
    gtk::Box::new(gtk::Orientation::Horizontal, spacing)
}

/// Expanding empty widget (SwiftUI `Spacer()`).
pub fn spacer() -> gtk::Box {
    let b = hbox(0);
    b.set_hexpand(true);
    b
}

/// 1px separator line (SwiftUI `Divider()`).
pub fn divider() -> gtk::Separator {
    gtk::Separator::new(gtk::Orientation::Horizontal)
}

/// Rounded translucent block around `child`.
pub fn card(child: &impl IsA<gtk::Widget>) -> gtk::Box {
    let b = vbox(0);
    b.add_css_class("mac-card");
    b.append(child);
    b
}

pub fn scrolled(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    let s = gtk::ScrolledWindow::new();
    s.set_hscrollbar_policy(gtk::PolicyType::Never);
    s.set_child(Some(child));
    s.set_vexpand(true);
    s.set_hexpand(true);
    s
}

pub fn spinner() -> gtk::Spinner {
    let s = gtk::Spinner::new();
    s.set_spinning(true);
    s
}

/// Removes every child of a `gtk::Box`.
pub fn clear_box(b: &gtk::Box) {
    while let Some(child) = b.first_child() {
        b.remove(&child);
    }
}

/// Removes every row of a `gtk::ListBox`.
pub fn clear_listbox(l: &gtk::ListBox) {
    l.remove_all();
}

/// Centered placeholder (SwiftUI `ContentUnavailableView`).
pub fn empty_state(icon_name: &str, title: &str, description: &str) -> adw::StatusPage {
    let p = adw::StatusPage::new();
    p.set_icon_name(Some(icon_name));
    p.set_title(title);
    if !description.is_empty() {
        p.set_description(Some(description));
    }
    p.add_css_class("compact");
    p.set_vexpand(true);
    p
}

// ------------------------------------------------------------------ buttons

/// Standard push button.
pub fn button(text: &str) -> gtk::Button {
    let b = gtk::Button::with_label(text);
    b.add_css_class("mac");
    b
}

/// Blue default-action button (`.keyboardShortcut(.defaultAction)`).
pub fn primary_button(text: &str) -> gtk::Button {
    let b = button(text);
    b.add_css_class("suggested-action");
    b
}

/// Red button (`role: .destructive`).
pub fn destructive_button(text: &str) -> gtk::Button {
    let b = button(text);
    b.add_css_class("destructive-action");
    b
}

/// Bottom row of a sheet: optional leading widget (spinner…), then
/// right-aligned buttons in the given order.
pub fn button_row(leading: Option<&gtk::Widget>, buttons: &[&gtk::Button]) -> gtk::Box {
    let row = hbox(8);
    if let Some(w) = leading {
        row.append(w);
    }
    row.append(&spacer());
    for b in buttons {
        row.append(*b);
    }
    row
}

// ------------------------------------------------------------------- sheets

/// A macOS-style sheet: an `adw::Dialog` presented over the parent window.
/// Returns the dialog and its content box (vertical, 16px spacing, 20px
/// padding) already holding the bold title. Pass an empty `title` for sheets
/// that draw their own header, and `-1` for a natural height.
///
/// Present with `dialog.present(Some(parent))`, dismiss with `dialog.close()`.
pub fn sheet(title: &str, width: i32, height: i32) -> (adw::Dialog, gtk::Box) {
    let dialog = adw::Dialog::new();
    dialog.add_css_class("mac-sheet");
    dialog.set_content_width(width);
    if height > 0 {
        dialog.set_content_height(height);
    }
    dialog.set_follows_content_size(height <= 0);
    let content = vbox(16);
    content.set_margin_top(20);
    content.set_margin_bottom(20);
    content.set_margin_start(20);
    content.set_margin_end(20);
    if width > 0 {
        content.set_width_request(width - 40);
    }
    if !title.is_empty() {
        dialog.set_title(title);
        content.append(&label(title, &["sheet-title"]));
    }
    dialog.set_child(Some(&content));
    (dialog, content)
}

/// Like `sheet` but with no padding/title: for panels that manage their own
/// header bar and scrolling. Resizable by content (min `width`×`height`).
pub fn bare_sheet(title: &str, width: i32, height: i32) -> (adw::Dialog, gtk::Box) {
    let dialog = adw::Dialog::new();
    dialog.add_css_class("mac-sheet");
    dialog.set_title(title);
    dialog.set_content_width(width);
    dialog.set_content_height(height);
    let content = vbox(0);
    dialog.set_child(Some(&content));
    (dialog, content)
}

/// Header strip used by panels: icon, title + subtitle, then trailing
/// widgets (refresh button, "Close"…), with a divider-ready 16px padding.
pub fn panel_header(icon_name: &str, title: &str, subtitle: &str, trailing: &[&gtk::Widget]) -> gtk::Box {
    let row = hbox(12);
    row.set_margin_top(16);
    row.set_margin_bottom(14);
    row.set_margin_start(20);
    row.set_margin_end(20);
    if !icon_name.is_empty() {
        row.append(&icon_styled(icon_name, &["tint", "big-icon"]));
    }
    let texts = vbox(2);
    texts.append(&label(title, &["sheet-title"]));
    if !subtitle.is_empty() {
        texts.append(&label(subtitle, &["caption", "secondary"]));
    }
    texts.set_valign(gtk::Align::Center);
    row.append(&texts);
    row.append(&spacer());
    for w in trailing {
        w.set_valign(gtk::Align::Center);
        row.append(*w);
    }
    row
}

/// Grouped form container (SwiftUI `Form { }.formStyle(.grouped)`): add
/// `adw::EntryRow`, `adw::PasswordEntryRow`, `adw::SpinRow`, `adw::ComboRow`,
/// `adw::SwitchRow` or `adw::ActionRow`s with `group.add(&row)`.
pub fn form_group() -> adw::PreferencesGroup {
    adw::PreferencesGroup::new()
}

/// Stepper row (`Stepper("Keep daily: 7", value:, in: 0...365)`).
pub fn spin_row(title: &str, value: i64, min: i64, max: i64) -> adw::SpinRow {
    let row = adw::SpinRow::with_range(min as f64, max as f64, 1.0);
    row.set_title(title);
    row.set_value(value as f64);
    row
}

/// Popup-button row (`Picker`). Returns the row; read `row.selected()`.
pub fn combo_row(title: &str, options: &[&str], selected: usize) -> adw::ComboRow {
    let row = adw::ComboRow::new();
    row.set_title(title);
    row.set_model(Some(&gtk::StringList::new(options)));
    row.set_selected(selected as u32);
    row
}

// ------------------------------------------------------------------- alerts

/// Modal error/info alert with a single OK button.
pub fn alert(parent: &impl IsA<gtk::Widget>, title: &str, message: &str) {
    let d = adw::AlertDialog::new(Some(title), Some(message));
    d.add_response("ok", "OK");
    d.set_default_response(Some("ok"));
    d.set_close_response("ok");
    d.present(Some(parent));
}

/// Confirmation dialog (`.confirmationDialog`). `on_confirm` runs only when
/// the confirm button is chosen.
pub fn confirm(
    parent: &impl IsA<gtk::Widget>,
    title: &str,
    message: &str,
    confirm_label: &str,
    cancel_label: &str,
    destructive: bool,
    on_confirm: impl Fn() + 'static,
) {
    let d = adw::AlertDialog::new(Some(title), Some(message));
    d.add_response("cancel", cancel_label);
    d.add_response("confirm", confirm_label);
    d.set_response_appearance(
        "confirm",
        if destructive { adw::ResponseAppearance::Destructive } else { adw::ResponseAppearance::Suggested },
    );
    d.set_default_response(Some("cancel"));
    d.set_close_response("cancel");
    d.connect_response(None, move |_, response| {
        if response == "confirm" {
            on_confirm();
        }
    });
    d.present(Some(parent));
}

// ------------------------------------------------------------- file pickers

fn window_of(widget: &impl IsA<gtk::Widget>) -> Option<gtk::Window> {
    widget.root().and_then(|r| r.downcast::<gtk::Window>().ok())
}

/// Folder picker (`NSOpenPanel` with `canChooseDirectories`).
pub fn choose_folder(
    parent: &impl IsA<gtk::Widget>,
    title: &str,
    accept_label: &str,
    on_chosen: impl FnOnce(PathBuf) + 'static,
) {
    let dialog = gtk::FileDialog::builder().title(title).accept_label(accept_label).modal(true).build();
    dialog.select_folder(window_of(parent).as_ref(), gio::Cancellable::NONE, move |result| {
        if let Some(path) = result.ok().and_then(|f| f.path()) {
            on_chosen(path);
        }
    });
}

/// Multi-selection folder picker for backup sources.
pub fn choose_folders(parent: &impl IsA<gtk::Widget>, title: &str, on_chosen: impl FnOnce(Vec<PathBuf>) + 'static) {
    let dialog = gtk::FileDialog::builder().title(title).accept_label("Add").modal(true).build();
    dialog.select_multiple_folders(window_of(parent).as_ref(), gio::Cancellable::NONE, move |result| {
        if let Ok(list) = result {
            let paths: Vec<PathBuf> = (0..list.n_items())
                .filter_map(|i| list.item(i))
                .filter_map(|o| o.downcast::<gio::File>().ok())
                .filter_map(|f| f.path())
                .collect();
            if !paths.is_empty() {
                on_chosen(paths);
            }
        }
    });
}

/// Multi-selection file picker.
pub fn choose_files(parent: &impl IsA<gtk::Widget>, title: &str, on_chosen: impl FnOnce(Vec<PathBuf>) + 'static) {
    let dialog = gtk::FileDialog::builder().title(title).accept_label("Add").modal(true).build();
    dialog.open_multiple(window_of(parent).as_ref(), gio::Cancellable::NONE, move |result| {
        if let Ok(list) = result {
            let paths: Vec<PathBuf> = (0..list.n_items())
                .filter_map(|i| list.item(i))
                .filter_map(|o| o.downcast::<gio::File>().ok())
                .filter_map(|f| f.path())
                .collect();
            if !paths.is_empty() {
                on_chosen(paths);
            }
        }
    });
}

/// Opens a file or folder with the default application (`NSWorkspace.open`).
pub fn open_path(path: &std::path::Path) {
    let uri = gio::File::for_path(path).uri();
    if gio::AppInfo::launch_default_for_uri(&uri, gio::AppLaunchContext::NONE).is_err() {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}

pub fn open_uri(uri: &str) {
    if gio::AppInfo::launch_default_for_uri(uri, gio::AppLaunchContext::NONE).is_err() {
        let _ = std::process::Command::new("xdg-open").arg(uri).spawn();
    }
}

pub fn copy_to_clipboard(widget: &impl IsA<gtk::Widget>, text: &str) {
    widget.clipboard().set_text(text);
}

// --------------------------------------------------------------- list models

/// Wraps a Rust value so it can live in a `gio::ListStore`
/// (`store.append(&boxed(value))`, read back with `unbox::<T>(&obj)`).
pub fn boxed<T: 'static>(value: T) -> glib::BoxedAnyObject {
    glib::BoxedAnyObject::new(value)
}

pub fn unbox<T: Clone + 'static>(obj: &glib::Object) -> Option<T> {
    obj.downcast_ref::<glib::BoxedAnyObject>().map(|b| b.borrow::<T>().clone())
}
