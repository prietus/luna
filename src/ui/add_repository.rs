//! "New repository" sheet: registers an existing repository (BorgMac's
//! `AddRepositorySheet`).
use super::kit;
use super::state::AppState;
use crate::core::models::Repository;
use crate::core::sshkeys;
use adw::prelude::*;
use gtk::glib;
use std::rc::Rc;

fn is_ssh_url(url: &str) -> bool {
    let trimmed = url.trim().to_lowercase();
    trimmed.starts_with("ssh://") || trimmed.contains('@')
}

pub fn present(parent: &impl IsA<gtk::Widget>, app: &Rc<AppState>, initial_name: &str, initial_url: &str) {
    let (dialog, content) = kit::sheet("New repository", 540, -1);

    let group = kit::form_group();
    let name = adw::EntryRow::builder().title("Name (e.g. BorgBase Personal)").text(initial_name).build();
    let url = adw::EntryRow::builder().title("URL (ssh://user@host:port/./repo)").text(initial_url).build();
    url.set_input_hints(gtk::InputHints::NO_SPELLCHECK | gtk::InputHints::LOWERCASE);
    let passphrase = adw::PasswordEntryRow::builder().title("Passphrase").build();
    let keys = sshkeys::list();
    let mut options: Vec<String> = vec!["— pick one —".to_string()];
    options.extend(keys.iter().map(|k| k.display_name()));
    let option_refs: Vec<&str> = options.iter().map(String::as_str).collect();
    let key_row = kit::combo_row("SSH key", &option_refs, 0);
    group.add(&name);
    group.add(&url);
    group.add(&passphrase);
    group.add(&key_row);
    content.append(&group);

    let help = kit::caption("");
    content.append(&help);
    let error = kit::error_label();
    content.append(&error);

    let cancel = kit::button("Cancel");
    let add = kit::primary_button("Add");
    content.append(&kit::button_row(None, &[&cancel, &add]));
    dialog.set_default_widget(Some(&add));

    // Derived state: SSH key row visibility, help text, "Add" enabled.
    let update = Rc::new(glib::clone!(
        #[weak]
        name,
        #[weak]
        url,
        #[weak]
        key_row,
        #[weak]
        help,
        #[weak]
        add,
        move || {
            let ssh = is_ssh_url(&url.text());
            key_row.set_visible(ssh);
            if ssh {
                help.set_markup(
                    "For SSH repos you <b>must</b> pick the specific private key authorized for this repo. Without this, ssh will try every key in the agent and may end up using the wrong one (and the daemon will reject the connection).",
                );
            } else {
                help.set_text("For local repos use an absolute path. The passphrase is stored in the keyring.");
            }
            let can_save =
                !name.text().is_empty() && !url.text().is_empty() && !(ssh && key_row.selected() == 0);
            add.set_sensitive(can_save);
        }
    ));
    update();
    name.connect_changed(glib::clone!(
        #[strong]
        update,
        move |_| update()
    ));
    url.connect_changed(glib::clone!(
        #[strong]
        update,
        move |_| update()
    ));
    key_row.connect_selected_notify(glib::clone!(
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

    let app = app.clone();
    add.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| {
            let url_text = url.text().to_string();
            let key = match key_row.selected() {
                0 => None,
                n if is_ssh_url(&url_text) => keys.get(n as usize - 1).map(|k| k.private_path.clone()),
                _ => None,
            };
            let repo = Repository::new(&name.text(), &url_text, key);
            let id = repo.id;
            match app.repos.add(repo, &passphrase.text()) {
                Ok(()) => {
                    app.request_selection(id);
                    dialog.close();
                }
                Err(e) => kit::set_error(&error, Some(&e)),
            }
        }
    ));

    dialog.present(Some(parent));
}
