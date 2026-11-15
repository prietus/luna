//! "Prune" sheet for a local/SSH repository (BorgMac's `PruneSheet`).
use super::kit;
use super::state::AppState;
use crate::core::borg;
use crate::core::models::Repository;
use adw::prelude::*;
use gtk::glib;
use std::rc::Rc;

pub fn present(
    parent: &impl IsA<gtk::Widget>,
    _app: &Rc<AppState>,
    repo: &Repository,
    on_completed: impl Fn() + 'static,
) {
    let (dialog, content) = kit::sheet("Prune", 480, -1);
    content.append(&kit::caption(
        "Deletes archives according to a retention policy. Set 0 to disable a rule.",
    ));

    let group = kit::form_group();
    let daily = kit::spin_row("Keep daily", 7, 0, 365);
    let weekly = kit::spin_row("Keep weekly", 4, 0, 104);
    let monthly = kit::spin_row("Keep monthly", 6, 0, 120);
    group.add(&daily);
    group.add(&weekly);
    group.add(&monthly);
    content.append(&group);

    let error = kit::error_label();
    content.append(&error);

    let spinner = gtk::Spinner::new();
    let cancel = kit::button("Cancel");
    let run = kit::primary_button("Run prune");
    content.append(&kit::button_row(Some(spinner.upcast_ref()), &[&cancel, &run]));
    dialog.set_default_widget(Some(&run));

    cancel.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| {
            dialog.close();
        }
    ));

    let repo = repo.clone();
    let on_completed = Rc::new(on_completed);
    run.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        #[weak]
        cancel,
        #[weak]
        spinner,
        #[weak]
        error,
        move |run| {
            run.set_sensitive(false);
            cancel.set_sensitive(false);
            dialog.set_can_close(false);
            spinner.set_spinning(true);
            kit::set_error(&error, None);
            let (d, w, m) = (daily.value() as i64, weekly.value() as i64, monthly.value() as i64);
            let repo = repo.clone();
            let on_completed = on_completed.clone();
            let run = run.clone();
            kit::bg(
                move || borg::prune(&repo, d, w, m),
                move |result| {
                    run.set_sensitive(true);
                    cancel.set_sensitive(true);
                    dialog.set_can_close(true);
                    spinner.set_spinning(false);
                    match result {
                        Ok(()) => {
                            on_completed();
                            dialog.close();
                        }
                        Err(e) => kit::set_error(&error, Some(&e)),
                    }
                },
            );
        }
    ));

    dialog.present(Some(parent));
}
