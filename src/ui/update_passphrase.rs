//! Sheet that lets the user (re-)store the Borg passphrase for an existing
//! repository in the keyring (BorgMac's `UpdatePassphraseSheet`). Added
//! because the scheduled runner needs a non-interactive read path, and when
//! the item is missing the only way out used to be deleting and re-adding the
//! repo (losing its schedules).
//!
//! Before writing to the keyring we verify the passphrase by running
//! `borg info --json` with it — a wrong passphrase would otherwise just mean
//! the next scheduled run fails with "Invalid passphrase".
use super::kit::{self, icons};
use super::state::AppState;
use crate::core::models::Repository;
use crate::core::{borg, keyring};
use adw::prelude::*;
use gtk::glib;
use std::rc::Rc;

pub fn present(parent: &impl IsA<gtk::Widget>, _app: &Rc<AppState>, repo: &Repository) {
    let (dialog, content) = kit::sheet("", 460, 280);
    dialog.set_title("Update passphrase");

    let header = kit::hbox(10);
    let key = kit::icon_styled(icons::KEY, &["tint", "sched-title-icon"]);
    key.set_valign(gtk::Align::Center);
    header.append(&key);
    let titles = kit::vbox(2);
    titles.append(&kit::label("Update passphrase", &["sched-title3"]));
    titles.append(&kit::ellipsized(&repo.name, false, &["caption", "mono", "secondary"]));
    header.append(&titles);
    content.append(&header);

    content.append(&kit::caption(
        "Stores the passphrase in the keyring so scheduled backups can open the repo without a prompt. The passphrase is verified against the repo before it's saved.",
    ));

    let passphrase = gtk::PasswordEntry::new();
    passphrase.set_placeholder_text(Some("Passphrase"));
    passphrase.set_show_peek_icon(false);
    passphrase.add_css_class("sched-field");
    content.append(&passphrase);

    let error = kit::error_label();
    content.append(&error);

    let filler = kit::vbox(0);
    filler.set_vexpand(true);
    content.append(&filler);

    let verifying = kit::hbox(6);
    let spinner = kit::spinner();
    verifying.append(&spinner);
    verifying.append(&kit::label("Verifying…", &["caption", "secondary"]));
    verifying.set_visible(false);
    let cancel = kit::button("Cancel");
    let save = kit::primary_button("Save");
    save.set_sensitive(false);
    content.append(&kit::button_row(Some(verifying.upcast_ref()), &[&cancel, &save]));
    dialog.set_default_widget(Some(&save));

    passphrase.connect_changed(glib::clone!(
        #[weak]
        save,
        move |entry| save.set_sensitive(!entry.text().is_empty() && entry.is_sensitive())
    ));
    cancel.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| {
            dialog.close();
        }
    ));

    let repo = repo.clone();
    let submit = Rc::new(glib::clone!(
        #[weak]
        dialog,
        #[weak]
        passphrase,
        #[weak]
        cancel,
        #[weak]
        save,
        #[weak]
        verifying,
        #[weak]
        error,
        move || {
            let text = passphrase.text().to_string();
            if text.is_empty() || !passphrase.is_sensitive() {
                return;
            }
            let set_busy = glib::clone!(
                #[weak]
                dialog,
                #[weak]
                passphrase,
                #[weak]
                cancel,
                #[weak]
                save,
                #[weak]
                verifying,
                move |busy: bool| {
                    passphrase.set_sensitive(!busy);
                    cancel.set_sensitive(!busy);
                    save.set_sensitive(!busy && !passphrase.text().is_empty());
                    verifying.set_visible(busy);
                    dialog.set_can_close(!busy);
                }
            );
            set_busy(true);
            kit::set_error(&error, None);
            let repo = repo.clone();
            let repo_id = repo.id;
            kit::bg(
                move || {
                    borg::verify_passphrase(&repo, &text)?;
                    keyring::set_passphrase(&text, &repo.id)
                },
                move |result| {
                    set_busy(false);
                    match result {
                        Ok(()) => {
                            // Drop the cached passphrase so the next borg
                            // call re-reads the one we just stored.
                            borg::forget(&repo_id);
                            dialog.close();
                        }
                        Err(e) => {
                            kit::set_error(&error, Some(&e));
                            passphrase.grab_focus();
                        }
                    }
                },
            );
        }
    ));

    passphrase.connect_activate(glib::clone!(
        #[strong]
        submit,
        move |_| submit()
    ));
    save.connect_clicked(move |_| submit());

    dialog.present(Some(parent));
    passphrase.grab_focus();
    if let Ok(text) = std::env::var("LUNA_DEBUG_PASSPHRASE") {
        // Debug hook: type and submit a passphrase.
        passphrase.set_text(&text);
        passphrase.emit_activate();
    }
}
