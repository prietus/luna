//! Adopts an existing repo from a BorgBox daemon into the local repository
//! store (BorgMac's `BorgBoxImportSheet`). Two modes:
//!
//! - **ExistingKey**: the user already has an SSH key authorized for this
//!   repo's `authorized_keys`. They pick it and we save locally.
//! - **Register**: the repo exists on disk but has no key authorized
//!   (daemon reports `registered: false`). We generate a fresh ed25519
//!   key, call `POST /repos/import` on the daemon, then save locally.
//!
//! Neither mode runs `borg init` — the repo already exists on the server.
use super::borgbox_alerts::sheet_header;
use super::kit::{self, icons};
use super::state::AppState;
use crate::core::models::{BorgBoxServer, Repository};
use crate::core::{borgbox, format, keyring, paths, sshkeys};
use adw::prelude::*;
use gtk::{gio, glib};
use std::cell::Cell;
use std::rc::Rc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    ExistingKey,
    Register,
}

/// Slug handed to `sshkeys::generate` for a BorgBox repo; the quick setup
/// wizard uses the same one, so both agree on the key file.
pub(super) fn key_slug(remote_name: &str) -> String {
    format!("borgbox_{remote_name}")
}

/// Where the dedicated key of a BorgBox repo lives by convention:
/// `~/.ssh/luna_borgbox_<remoteName>` (the `luna_` prefix is added
/// by `sshkeys::generate`).
fn conventional_key_path(remote_name: &str) -> String {
    let file = format!("luna_{}", format::slug(&key_slug(remote_name), '_'));
    paths::home().join(".ssh").join(file).to_string_lossy().into_owned()
}

/// Single-file picker starting in `~/.ssh`.
fn pick_key(parent: &impl IsA<gtk::Widget>, on_chosen: impl FnOnce(String) + 'static) {
    let dialog = gtk::FileDialog::builder()
        .title("Choose the private SSH key")
        .accept_label("Use")
        .modal(true)
        .initial_folder(&gio::File::for_path(paths::home().join(".ssh")))
        .build();
    let window = parent.root().and_then(|r| r.downcast::<gtk::Window>().ok());
    dialog.open(window.as_ref(), gio::Cancellable::NONE, move |result| {
        if let Some(path) = result.ok().and_then(|f| f.path()) {
            on_chosen(path.to_string_lossy().into_owned());
        }
    });
}

pub fn present(
    parent: &impl IsA<gtk::Widget>,
    app: &Rc<AppState>,
    server: &BorgBoxServer,
    remote_name: &str,
    remote_path: &str,
    remote_ssh_url: Option<&str>,
    mode: Mode,
) {
    // The daemon's canonical SSH URL (v0.3.0 `ssh_url`) is used verbatim
    // when present; otherwise it is built from the server fields.
    let borg_url = match remote_ssh_url {
        Some(url) if !url.is_empty() => url.to_string(),
        _ => format!("ssh://{}@{}:{}{}", server.ssh_user, server.ssh_host, server.ssh_port, remote_path),
    };
    // Auto-detect the Luna convention path: when this computer was the
    // one that originally created the repo via the quick setup wizard, the
    // key is already there.
    let candidate = conventional_key_path(remote_name);
    let candidate_exists = std::path::Path::new(&candidate).exists();
    let display_candidate = paths::abbreviate_home(&candidate);

    let (dialog, content) = kit::sheet("", 580, 540);
    dialog.set_title("Import repo from BorgBox");
    let (header, _) = sheet_header(
        icons::DOWNLOAD,
        "Import repo from BorgBox",
        &format!("{remote_name} — {}", server.name),
        true,
    );
    content.append(&header);

    let form = kit::vbox(14);
    form.add_css_class("bb2-form");
    form.set_margin_end(4);

    let local = kit::form_group();
    local.set_title("Local");
    let local_name = adw::EntryRow::builder().title("Name in Luna").text(remote_name).build();
    local.add(&local_name);
    let url_row = adw::ActionRow::builder().title("SSH URL").build();
    url_row.add_css_class("bb2-dim-title");
    let url_label = kit::label(&borg_url, &["caption", "mono"]);
    url_label.set_selectable(true);
    url_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    url_label.set_xalign(1.0);
    url_label.set_hexpand(true);
    url_row.add_suffix(&url_label);
    local.add(&url_row);
    form.append(&local);

    let key_section = kit::vbox(6);
    let key_group = kit::form_group();
    key_group.set_title("SSH key");
    let key_path = adw::EntryRow::builder()
        .title("Path to the private key")
        .text(if candidate_exists { candidate.as_str() } else { "" })
        .build();
    key_path.add_css_class("mono");
    key_path.set_input_hints(gtk::InputHints::NO_SPELLCHECK);
    let key_help = kit::caption("");
    key_help.set_margin_start(4);
    match mode {
        Mode::ExistingKey => {
            let choose = kit::button("Choose…");
            choose.set_valign(gtk::Align::Center);
            key_path.add_suffix(&choose);
            key_group.add(&key_path);
            choose.connect_clicked(glib::clone!(
                #[weak]
                key_path,
                move |button| {
                    pick_key(button, move |path| key_path.set_text(&path));
                }
            ));
            key_section.append(&key_group);
            key_section.append(&key_help);
        }
        Mode::Register => {
            let row = adw::ActionRow::builder()
                .title("A dedicated ed25519 key will be generated")
                .subtitle(glib::markup_escape_text(&format!(
                    "It will be stored at {display_candidate} and registered in the daemon's authorized_keys. This computer will be able to use the repo without relying on any other keys."
                )))
                .build();
            let wand = kit::icon_styled(icons::WAND, &["tint"]);
            wand.set_valign(gtk::Align::Start);
            wand.set_margin_top(12);
            row.add_prefix(&wand);
            key_group.add(&row);
            key_section.append(&key_group);
        }
    }
    form.append(&key_section);

    let passphrase_group = kit::form_group();
    passphrase_group.set_title("Passphrase");
    let passphrase = adw::PasswordEntryRow::builder().title("Borg repository passphrase").build();
    passphrase_group.add(&passphrase);
    form.append(&passphrase_group);
    content.append(&kit::scrolled(&form));

    let error = kit::error_label();
    content.append(&error);

    let spinner = gtk::Spinner::new();
    let cancel = kit::button("Cancel");
    let import = kit::primary_button(if mode == Mode::Register { "Register and import" } else { "Import" });
    content.append(&kit::button_row(Some(spinner.upcast_ref()), &[&cancel, &import]));
    dialog.set_default_widget(Some(&import));

    // --------------------------------------------------------- derived state
    let busy = Rc::new(Cell::new(false));
    // The key widgets are only part of the sheet in ExistingKey mode, so
    // they are upgraded by hand instead of aborting the whole update.
    let (weak_key_path, weak_key_help) = (key_path.downgrade(), key_help.downgrade());
    let update = Rc::new(glib::clone!(
        #[weak]
        local_name,
        #[weak]
        passphrase,
        #[weak]
        import,
        #[weak]
        cancel,
        #[weak]
        spinner,
        #[strong]
        busy,
        #[strong]
        display_candidate,
        move || {
            let key = weak_key_path.upgrade().map(|row| row.text().to_string()).unwrap_or_default();
            if let Some(key_help) = weak_key_help.upgrade() {
                if key.is_empty() {
                    key_help.set_text(&format!(
                        "You need the SSH key that's already authorized on this repo. If this computer is the one that created it via the wizard, it's usually at {display_candidate}."
                    ));
                } else {
                    key_help.set_text(
                        "Auto-detected from the Luna wizard convention. Change it if your key lives somewhere else.",
                    );
                }
            }
            let can_import = !local_name.text().trim().is_empty()
                && !passphrase.text().is_empty()
                && (mode == Mode::Register || !key.trim().is_empty());
            import.set_sensitive(!busy.get() && can_import);
            cancel.set_sensitive(!busy.get());
            spinner.set_spinning(busy.get());
        }
    ));
    update();
    for row in [&local_name, &key_path] {
        row.connect_changed(glib::clone!(
            #[strong]
            update,
            move |_| update()
        ));
    }
    passphrase.connect_changed(glib::clone!(
        #[strong]
        update,
        move |_| update()
    ));

    cancel.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| {
            dialog.close();
        }
    ));

    // Adds the repository to the local store (passphrase goes to the
    // keyring) and closes the sheet.
    let app = app.clone();
    let server = server.clone();
    let remote_name = remote_name.to_string();
    let save_locally = Rc::new(glib::clone!(
        #[weak]
        dialog,
        #[weak]
        error,
        #[weak]
        local_name,
        #[weak]
        passphrase,
        #[strong]
        server,
        #[strong]
        remote_name,
        move |private_key_path: String| {
            let repo = Repository::new(local_name.text().trim(), &borg_url, Some(private_key_path));
            let passphrase = passphrase.text();
            match app.repos.add(repo, &passphrase) {
                Ok(()) => {
                    // Also remembered per server/repo so the daemon-side
                    // maintenance jobs don't ask for it again. Best effort.
                    let _ = keyring::set_borgbox_repo_passphrase(&passphrase, &server.id, &remote_name);
                    dialog.close();
                }
                Err(e) => kit::set_error(&error, Some(&e)),
            }
        }
    ));

    import.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        #[weak]
        error,
        #[strong]
        key_path,
        move |_| {
            kit::set_error(&error, None);
            match mode {
                Mode::ExistingKey => {
                    let trimmed_key = paths::expand_tilde(key_path.text().trim());
                    if !std::path::Path::new(&trimmed_key).exists() {
                        kit::set_error(&error, Some(&format!("The SSH key {trimmed_key} does not exist.")));
                        return;
                    }
                    save_locally(trimmed_key);
                }
                Mode::Register => {
                    busy.set(true);
                    update();
                    dialog.set_can_close(false);
                    let (server, clean_name) = (server.clone(), remote_name.clone());
                    let (busy, update, save_locally) = (busy.clone(), update.clone(), save_locally.clone());
                    kit::bg(
                        move || {
                            let (private_path, public_key) = sshkeys::generate(
                                &key_slug(&clean_name),
                                &format!("luna-borgbox-{clean_name}"),
                            )?;
                            borgbox::import_repo(&server, &clean_name, &public_key)?;
                            Ok::<String, String>(private_path)
                        },
                        move |result| {
                            busy.set(false);
                            update();
                            dialog.set_can_close(true);
                            match result {
                                Ok(private_path) => save_locally(private_path),
                                Err(e) => kit::set_error(&error, Some(&e)),
                            }
                        },
                    );
                }
            }
        }
    ));

    dialog.present(Some(parent));
}
