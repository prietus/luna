//! Small modal asking for a Borg repo passphrase before running a maintenance
//! operation (BorgMac's `BorgBoxPassphraseSheet`). Used by the BorgBox server
//! panel when it couldn't resolve the passphrase automatically from a
//! matching local repository.
use super::kit::{self, icons};
use adw::prelude::*;
use gtk::glib;
use std::rc::Rc;

/// Sheet header shared by the BorgBox sheets: tinted icon, bold title and a
/// monospaced secondary subtitle.
pub fn title_block(icon_name: &str, title: &str, subtitle: &str) -> gtk::Box {
    let row = kit::hbox(10);
    let icon = kit::icon_styled(icon_name, &["tint", "bb-title-icon"]);
    icon.set_valign(gtk::Align::Center);
    row.append(&icon);
    let texts = kit::vbox(2);
    texts.set_valign(gtk::Align::Center);
    texts.append(&kit::label(title, &["title-3"]));
    texts.append(&kit::ellipsized(subtitle, true, &["caption", "mono", "secondary"]));
    row.append(&texts);
    row
}

pub fn present(parent: &impl IsA<gtk::Widget>, repo_name: &str, action: &str, on_submit: impl Fn(String) + 'static) {
    let (dialog, content) = kit::sheet("", 440, 260);
    content.add_css_class("bb-panel");
    dialog.set_title("Passphrase required");
    content.append(&title_block(icons::LOCK, "Passphrase required", &format!("{action} · {repo_name}")));
    content.append(&kit::caption(
        "The BorgBox daemon doesn't persist the passphrase — it's sent on every operation. It will be saved in the keyring so you don't have to re-enter it.",
    ));

    let entry = gtk::PasswordEntry::new();
    entry.set_placeholder_text(Some("Passphrase"));
    entry.set_show_peek_icon(true);
    content.append(&entry);

    let filler = kit::vbox(0);
    filler.set_vexpand(true);
    content.append(&filler);

    let cancel = kit::button("Cancel");
    let submit_button = kit::primary_button("Continue");
    submit_button.set_sensitive(false);
    content.append(&kit::button_row(None, &[&cancel, &submit_button]));

    cancel.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| {
            dialog.close();
        }
    ));
    entry.connect_changed(glib::clone!(
        #[weak]
        submit_button,
        move |entry| submit_button.set_sensitive(!entry.text().is_empty())
    ));

    let submit = Rc::new(glib::clone!(
        #[weak]
        dialog,
        #[weak]
        entry,
        move || {
            let passphrase = entry.text().to_string();
            if passphrase.is_empty() {
                return;
            }
            // Close first: the callback usually presents the next sheet.
            dialog.close();
            on_submit(passphrase);
        }
    ));
    entry.connect_activate(glib::clone!(
        #[strong]
        submit,
        move |_| submit()
    ));
    submit_button.connect_clicked(move |_| submit());

    dialog.present(Some(parent));
    entry.grab_focus();
}
