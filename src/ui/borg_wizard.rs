//! "New Borg Repository" wizard (BorgMac's `BorgQuickSetupSheet`): initializes
//! a repository in a local folder or on any SSH host and registers it. A
//! stand-alone singleton window, not a sheet.
//!
//! Also hosts the small pieces shared with the BorgBase views: the wizard
//! window shell, the segmented control and the icon + title header.
use super::kit::{self, icons};
use super::state::AppState;
use crate::core::borg;
use crate::core::models::Repository;
use crate::core::sshkeys::{self, LocalSSHKey};
use adw::prelude::*;
use gtk::{gdk, glib};
use std::cell::Cell;
use std::rc::Rc;

// ------------------------------------------------------------ shared pieces

/// Segmented control (SwiftUI `Picker(...).pickerStyle(.segmented)`).
pub struct Segmented {
    pub widget: gtk::Box,
    buttons: Vec<gtk::ToggleButton>,
}

impl Segmented {
    pub fn new(options: &[&str], selected: usize) -> Segmented {
        let widget = kit::hbox(0);
        widget.add_css_class("mac-segmented");
        widget.set_valign(gtk::Align::Center);
        widget.set_halign(gtk::Align::Center);
        widget.set_homogeneous(true);
        let mut buttons: Vec<gtk::ToggleButton> = Vec::with_capacity(options.len());
        for (i, text) in options.iter().enumerate() {
            let b = gtk::ToggleButton::with_label(text);
            if let Some(first) = buttons.first() {
                b.set_group(Some(first));
            }
            b.set_active(i == selected);
            widget.append(&b);
            buttons.push(b);
        }
        Segmented { widget, buttons }
    }

    pub fn selected(&self) -> usize {
        self.buttons.iter().position(|b| b.is_active()).unwrap_or(0)
    }

    pub fn connect_changed(&self, f: impl Fn(usize) + 'static) {
        let f = Rc::new(f);
        for (i, b) in self.buttons.iter().enumerate() {
            let f = f.clone();
            b.connect_toggled(move |b| {
                if b.is_active() {
                    f(i);
                }
            });
        }
    }
}

/// Icon + bold title + caption (the header of the wizards and edit sheets).
pub fn sheet_header(icon_name: &str, title: &str, subtitle: &str) -> gtk::Box {
    let row = kit::hbox(10);
    let icon = kit::icon_styled(icon_name, &["tint", "big-icon"]);
    icon.set_valign(gtk::Align::Center);
    row.append(&icon);
    let texts = kit::vbox(2);
    texts.set_hexpand(true);
    texts.append(&kit::ellipsized(title, false, &["bb-title3"]));
    texts.append(&wrapping_caption(kit::caption(subtitle)));
    row.append(&texts);
    row
}

/// Wrapping caption that doesn't widen its (content-sized) window: it asks
/// for a narrow natural width and then fills whatever it is given.
pub fn wrapping_caption(l: gtk::Label) -> gtk::Label {
    l.set_max_width_chars(40);
    l.set_hexpand(true);
    l
}

/// Lets a combo row show its selected item in full instead of the stock
/// 20-character ellipsis; the popup list keeps the stock rows (check mark).
pub fn show_full_selection(row: &adw::ComboRow) {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let label = gtk::Label::new(None);
        label.set_xalign(1.0);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
            item.set_child(Some(&label));
        }
    });
    factory.connect_bind(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else { return };
        let text = item.item().and_downcast::<gtk::StringObject>().map(|s| s.string());
        if let Some(label) = item.child().and_downcast::<gtk::Label>() {
            label.set_text(text.as_deref().unwrap_or(""));
        }
    });
    row.set_list_factory(row.factory().as_ref());
    row.set_factory(Some(&factory));
}

/// The "spinner / green check + step label" line under a wizard form.
pub struct ProgressRow {
    pub widget: gtk::Box,
    spinner: gtk::Spinner,
    check: gtk::Image,
    label: gtk::Label,
}

impl ProgressRow {
    pub fn new() -> ProgressRow {
        let widget = kit::vbox(16);
        widget.append(&kit::divider());
        let row = kit::hbox(10);
        let spinner = kit::spinner();
        let check = kit::icon_styled(icons::CHECK_CIRCLE, &["c-green"]);
        let label = kit::label("", &["secondary"]);
        row.append(&spinner);
        row.append(&check);
        row.append(&label);
        widget.append(&row);
        widget.set_visible(false);
        ProgressRow { widget, spinner, check, label }
    }

    /// `None` hides the row; otherwise shows `text` with a spinner
    /// (`running`) or the green check mark (done).
    pub fn set(&self, state: Option<(&str, bool)>) {
        self.widget.set_visible(state.is_some());
        if let Some((text, running)) = state {
            self.label.set_text(text);
            self.spinner.set_visible(running);
            self.spinner.set_spinning(running);
            self.check.set_visible(!running);
        }
    }
}

/// Creates the singleton wizard window `id` (SwiftUI `Window(title, id:)`
/// with `.windowResizability(.contentSize)`), or focuses the existing one and
/// returns `None`. The returned box is the content area: vertical, 16px
/// spacing, 20px padding, exactly `width`×`height`.
pub fn wizard_window(app: &Rc<AppState>, id: &str, title: &str, width: i32, height: i32) -> Option<(adw::Window, gtk::Box)> {
    if let Some(existing) = app.application.windows().into_iter().find(|w| w.widget_name() == id) {
        existing.present();
        return None;
    }
    let window = adw::Window::builder().application(&app.application).title(title).resizable(false).build();
    window.set_widget_name(id);
    window.add_css_class("borg-wizard");

    let header = adw::HeaderBar::new();
    header.set_show_start_title_buttons(false);
    header.set_show_end_title_buttons(false);
    let lights = gtk::WindowControls::new(gtk::PackType::Start);
    lights.set_decoration_layout(Some("close,minimize,maximize:"));
    lights.add_css_class("borg-lights");
    lights.set_valign(gtk::Align::Center);
    header.pack_start(&lights);

    let content = kit::vbox(16);
    content.set_margin_top(20);
    content.set_margin_bottom(20);
    content.set_margin_start(20);
    content.set_margin_end(20);
    content.set_size_request(width - 40, height - 40);

    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&content));
    window.set_content(Some(&view));
    Some((window, content))
}

/// Escape presses `cancel` (`.keyboardShortcut(.cancelAction)`); a disabled
/// button swallows it, so a running wizard can't be dismissed by accident.
pub fn bind_escape(window: &adw::Window, cancel: &gtk::Button) {
    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed(glib::clone!(
        #[weak]
        cancel,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_, key, _, _| {
            if key == gdk::Key::Escape {
                if cancel.is_sensitive() {
                    cancel.emit_clicked();
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        }
    ));
    window.add_controller(keys);
}

// ---------------------------------------------------------------- the wizard

const ENCRYPTIONS: [(&str, &str, Option<&str>); 4] = [
    ("repokey-blake2", "Encrypted — key stored in repo (recommended)", None),
    ("repokey", "Encrypted — key in repo, HMAC-SHA256", None),
    (
        "keyfile-blake2",
        "Encrypted — key on this computer only (advanced)",
        Some(
            "The encryption key lives in ~/.config/borg/keys/ on this computer. If you lose it the repo is unrecoverable — back it up separately.",
        ),
    ),
    (
        "none",
        "No encryption",
        Some("Anyone with access to the repo can read your data. Only choose this for throwaway test repos."),
    ),
];

#[derive(Clone, Copy, PartialEq)]
enum Step {
    Idle,
    Initializing,
    SavingLocally,
    Done,
}

impl Step {
    fn label(self) -> &'static str {
        match self {
            Step::Idle => "",
            Step::Initializing => "Initializing the Borg repository (borg init)…",
            Step::SavingLocally => "Saving to Luna…",
            Step::Done => "Done.",
        }
    }

    fn is_running(self) -> bool {
        matches!(self, Step::Initializing | Step::SavingLocally)
    }
}

/// `NSString.lastPathComponent`: trailing slashes are ignored, the root is "/".
fn last_path_component(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return if path.is_empty() { "" } else { "/" };
    }
    trimmed.rsplit('/').next().unwrap_or(trimmed)
}

/// The Swift falls back to the URL's host when the last path component is
/// empty, which only happens for an empty URL — so this is all there is.
fn suggested_name(ssh: bool, local_path: &str, ssh_url: &str) -> String {
    last_path_component(if ssh { ssh_url } else { local_path }).to_string()
}

struct Wizard {
    app: Rc<AppState>,
    window: adw::Window,
    mode: Segmented,
    local_row: adw::EntryRow,
    choose: gtk::Button,
    ssh_row: adw::EntryRow,
    name: adw::EntryRow,
    passphrase: adw::PasswordEntryRow,
    confirm: adw::PasswordEntryRow,
    encryption: adw::ComboRow,
    encryption_help: gtk::Label,
    key_group: adw::PreferencesGroup,
    key_row: adw::ComboRow,
    key_path: gtk::Label,
    key_help: gtk::Label,
    progress: ProgressRow,
    cancel: gtk::Button,
    create: gtk::Button,
    keys: Vec<LocalSSHKey>,
    step: Cell<Step>,
}

impl Wizard {
    fn is_ssh(&self) -> bool {
        self.mode.selected() == 1
    }

    fn location(&self) -> String {
        let text = if self.is_ssh() { self.ssh_row.text() } else { self.local_row.text() };
        text.trim().to_string()
    }

    fn selected_key(&self) -> Option<String> {
        match self.key_row.selected() {
            0 => None,
            n => self.keys.get(n as usize - 1).map(|k| k.private_path.clone()),
        }
    }

    fn suggested_name(&self) -> String {
        suggested_name(self.is_ssh(), &self.local_row.text(), &self.ssh_row.text())
    }

    /// Re-derives everything that depends on the form state.
    fn update(&self) {
        let ssh = self.is_ssh();
        let step = self.step.get();
        let running = step.is_running();

        self.local_row.set_visible(!ssh);
        self.ssh_row.set_visible(ssh);
        self.key_group.set_visible(ssh);

        let suggested = self.suggested_name();
        self.name.set_title(&if suggested.is_empty() {
            "Name (e.g. personal-server)".to_string()
        } else {
            format!("Name (default: {})", glib::markup_escape_text(&suggested))
        });

        let help = ENCRYPTIONS.get(self.encryption.selected() as usize).and_then(|e| e.2);
        self.encryption_help.set_text(help.unwrap_or(""));
        self.encryption_help.set_visible(help.is_some());

        let key = self.selected_key();
        self.key_path.set_text(key.as_deref().unwrap_or(""));
        self.key_path.set_visible(key.is_some());
        self.key_help.set_visible(key.is_none());

        for w in [
            self.mode.widget.upcast_ref::<gtk::Widget>(),
            self.local_row.upcast_ref(),
            self.choose.upcast_ref(),
            self.ssh_row.upcast_ref(),
            self.name.upcast_ref(),
            self.passphrase.upcast_ref(),
            self.confirm.upcast_ref(),
            self.encryption.upcast_ref(),
        ] {
            w.set_sensitive(!running);
        }

        self.progress.set(match step {
            Step::Idle => None,
            other => Some((other.label(), other.is_running())),
        });
        self.cancel.set_sensitive(!running);
        let pass = self.passphrase.text();
        let can_create =
            step == Step::Idle && !self.location().is_empty() && !pass.is_empty() && pass == self.confirm.text();
        self.create.set_sensitive(can_create);
    }

    fn fail(&self, message: &str) {
        self.step.set(Step::Idle);
        self.update();
        kit::alert(&self.window, "Error", message);
    }

    fn run(self: &Rc<Self>) {
        let trimmed = self.name.text().trim().to_string();
        let final_name = if !trimmed.is_empty() {
            trimmed
        } else {
            let suggested = self.suggested_name();
            if suggested.is_empty() { "borg-repo".to_string() } else { suggested }
        };
        // Only carry the SSH key path for SSH repos.
        let key_path = if self.is_ssh() { self.selected_key() } else { None };
        let location = self.location();
        let passphrase = self.passphrase.text().to_string();
        let encryption = ENCRYPTIONS.get(self.encryption.selected() as usize).map(|e| e.0).unwrap_or("repokey-blake2");

        self.step.set(Step::Initializing);
        self.update();

        let weak = Rc::downgrade(self);
        let (url, pass, key) = (location.clone(), passphrase.clone(), key_path.clone());
        kit::bg(
            move || borg::init_repo(&url, &pass, encryption, key.as_deref()),
            move |result| {
                let Some(w) = weak.upgrade() else { return };
                if let Err(e) = result {
                    w.fail(&e);
                    return;
                }
                w.step.set(Step::SavingLocally);
                w.update();
                let repo = Repository::new(&final_name, &location, key_path);
                if let Err(e) = w.app.repos.add(repo, &passphrase) {
                    w.fail(&e);
                    return;
                }
                w.step.set(Step::Done);
                w.update();
                let window = w.window.downgrade();
                kit::after(600, move || {
                    if let Some(window) = window.upgrade() {
                        window.close();
                    }
                });
            },
        );
    }
}

/// Opens (or focuses) the wizard window.
pub fn open(app: &Rc<AppState>) {
    let Some((window, content)) = wizard_window(app, "borg-wizard", "New Borg Repository", 600, 620) else { return };

    content.append(&sheet_header(
        icons::WAND,
        "New Borg repository",
        "Initialize a Borg repository in a local folder or on your own SSH server.",
    ));
    content.append(&kit::divider());

    let form = kit::vbox(14);
    form.add_css_class("bb-compact");

    let type_group = kit::form_group();
    let mode = Segmented::new(&["Local folder", "Remote SSH"], 0);
    let type_row = adw::ActionRow::builder().title("Type").build();
    type_row.add_suffix(&mode.widget);
    type_group.add(&type_row);
    form.append(&type_group);

    let location_group = kit::form_group();
    location_group.set_title("Location");
    let local_row = adw::EntryRow::builder().title("/path/to/repository").build();
    local_row.set_input_hints(gtk::InputHints::NO_SPELLCHECK);
    let choose = kit::button("Choose…");
    choose.set_valign(gtk::Align::Center);
    local_row.add_suffix(&choose);
    let ssh_row = adw::EntryRow::builder().title("ssh://user@host:port/./repo").build();
    ssh_row.set_input_hints(gtk::InputHints::NO_SPELLCHECK | gtk::InputHints::LOWERCASE);
    location_group.add(&local_row);
    location_group.add(&ssh_row);
    form.append(&location_group);

    let repo_group = kit::form_group();
    repo_group.set_title("Repository");
    let name = adw::EntryRow::builder().title("Name").build();
    let passphrase = adw::PasswordEntryRow::builder().title("Passphrase").build();
    let confirm = adw::PasswordEntryRow::builder().title("Repeat passphrase").build();
    repo_group.add(&name);
    repo_group.add(&passphrase);
    repo_group.add(&confirm);
    form.append(&repo_group);

    let encryption_group = kit::form_group();
    encryption_group.set_title("Encryption");
    let labels: Vec<&str> = ENCRYPTIONS.iter().map(|e| e.1).collect();
    let encryption = kit::combo_row("Mode", &labels, 0);
    encryption_group.add(&encryption);
    show_full_selection(&encryption);
    let encryption_help = wrapping_caption(kit::caption(""));
    encryption_help.add_css_class("bb-form-note");
    encryption_group.add(&encryption_help);
    form.append(&encryption_group);

    let keys = sshkeys::list();
    let key_group = kit::form_group();
    key_group.set_title("SSH key");
    let mut key_labels = vec!["Default (ssh-agent / ~/.ssh/config)".to_string()];
    key_labels.extend(keys.iter().map(|k| k.display_name()));
    let key_label_refs: Vec<&str> = key_labels.iter().map(String::as_str).collect();
    let key_row = kit::combo_row("Key to use", &key_label_refs, 0);
    key_group.add(&key_row);
    let key_path = kit::ellipsized("", true, &["caption", "mono", "tertiary", "bb-form-note"]);
    key_path.set_max_width_chars(40);
    key_group.add(&key_path);
    show_full_selection(&key_row);
    let key_help = wrapping_caption(kit::caption(
        "If your SSH key is passphrase-protected, make sure it is loaded in `ssh-agent` before running the wizard.",
    ));
    key_help.add_css_class("bb-form-note");
    key_group.add(&key_help);
    form.append(&key_group);

    let scroll = kit::scrolled(&form);
    scroll.add_css_class("bb-form-scroll");
    content.append(&scroll);

    let progress = ProgressRow::new();
    content.append(&progress.widget);

    let cancel = kit::button("Cancel");
    let create = kit::primary_button("Create");
    content.append(&kit::button_row(None, &[&cancel, &create]));
    window.set_default_widget(Some(&create));
    for row in [&local_row, &ssh_row, &name] {
        row.set_activates_default(true);
    }
    passphrase.set_activates_default(true);
    confirm.set_activates_default(true);
    bind_escape(&window, &cancel);

    let w = Rc::new(Wizard {
        app: app.clone(),
        window: window.clone(),
        mode,
        local_row,
        choose,
        ssh_row,
        name,
        passphrase,
        confirm,
        encryption,
        encryption_help,
        key_group,
        key_row,
        key_path,
        key_help,
        progress,
        cancel,
        create,
        keys,
        step: Cell::new(Step::Idle),
    });

    w.mode.connect_changed(glib::clone!(
        #[weak]
        w,
        move |_| w.update()
    ));
    for row in [&w.local_row, &w.ssh_row, &w.name] {
        row.connect_changed(glib::clone!(
            #[weak]
            w,
            move |_| w.update()
        ));
    }
    for row in [&w.passphrase, &w.confirm] {
        row.connect_changed(glib::clone!(
            #[weak]
            w,
            move |_| w.update()
        ));
    }
    for row in [&w.encryption, &w.key_row] {
        row.connect_selected_notify(glib::clone!(
            #[weak]
            w,
            move |_| w.update()
        ));
    }
    w.choose.connect_clicked(glib::clone!(
        #[weak]
        w,
        move |_| {
            let row = w.local_row.downgrade();
            kit::choose_folder(
                &w.window,
                "Choose an empty folder where the Borg repository will be initialized",
                "Create repo here",
                move |path| {
                    if let Some(row) = row.upgrade() {
                        row.set_text(&path.to_string_lossy());
                    }
                },
            );
        }
    ));
    w.cancel.connect_clicked(glib::clone!(
        #[weak]
        window,
        move |_| window.close()
    ));
    w.create.connect_clicked(glib::clone!(
        #[weak]
        w,
        move |_| w.run()
    ));
    // A running `borg init` must not be orphaned by the close button.
    window.connect_close_request(glib::clone!(
        #[weak]
        w,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_| if w.step.get().is_running() { glib::Propagation::Stop } else { glib::Propagation::Proceed }
    ));

    w.update();
    // The window owns the wizard state; handlers only hold weak references.
    let keep = std::cell::RefCell::new(Some(w));
    window.connect_destroy(move |_| {
        keep.borrow_mut().take();
    });
    window.present();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggested_names() {
        assert_eq!(suggested_name(false, "/mnt/backup/laptop/", ""), "laptop");
        assert_eq!(suggested_name(false, "", ""), "");
        assert_eq!(suggested_name(true, "", "ssh://user@host:22/./repo"), "repo");
        assert_eq!(suggested_name(true, "", "ssh://user@example.com:22/"), "user@example.com:22");
        assert_eq!(suggested_name(false, "/", ""), "/");
        assert_eq!(suggested_name(true, "", ""), "");
    }
}
