//! "Prune" sheet for a repo hosted on a BorgBox daemon (BorgMac's
//! `BorgBoxPruneSheet`): starts a prune job on the server and hands the job
//! back to the caller, which follows it in its operations drawer.
use super::borgbox_passphrase::title_block;
use super::kit::{self, icons};
use super::state::AppState;
use crate::core::borgbox::{self, BorgBoxJobStart};
use crate::core::models::BorgBoxServer;
use adw::prelude::*;
use gtk::glib;
use std::rc::Rc;

pub fn present(
    parent: &impl IsA<gtk::Widget>,
    _app: &Rc<AppState>,
    server: &BorgBoxServer,
    repo: &str,
    passphrase: Option<String>,
    on_started: impl Fn(BorgBoxJobStart) + 'static,
) {
    let (dialog, content) = kit::sheet("", 440, -1);
    content.add_css_class("bb-panel");
    dialog.set_title("Prune");
    content.append(&title_block(icons::SCISSORS, "Prune", repo));
    content.append(&kit::divider());

    let retention = kit::form_group();
    retention.set_title("Retention");
    let daily = kit::spin_row("Daily", 7, 0, 365);
    let weekly = kit::spin_row("Weekly", 4, 0, 52);
    let monthly = kit::spin_row("Monthly", 12, 0, 60);
    let yearly = kit::spin_row("Yearly", 2, 0, 20);
    for row in [&daily, &weekly, &monthly, &yearly] {
        retention.add(row);
    }
    content.append(&retention);

    let options = kit::form_group();
    let dry_run = adw::SwitchRow::builder().title("Dry run only (simulate)").active(true).build();
    options.add(&dry_run);
    let footer = kit::vbox(6);
    footer.append(&options);
    let hint = kit::caption("With dry run on, borg lists what it would delete but doesn't touch anything.");
    hint.set_margin_start(12);
    footer.append(&hint);
    content.append(&footer);

    let error = kit::error_label();
    content.append(&error);

    let cancel = kit::button("Cancel");
    let run = kit::primary_button("Simulate prune");
    content.append(&kit::button_row(None, &[&cancel, &run]));
    dialog.set_default_widget(Some(&run));

    dry_run.connect_active_notify(glib::clone!(
        #[weak]
        run,
        move |row| run.set_label(if row.is_active() { "Simulate prune" } else { "Run prune" })
    ));
    cancel.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| {
            dialog.close();
        }
    ));

    let (server, repo) = (server.clone(), repo.to_string());
    let on_started = Rc::new(on_started);
    run.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        #[weak]
        error,
        move |run| {
            run.set_sensitive(false);
            kit::set_error(&error, None);
            let (server, repo, passphrase) = (server.clone(), repo.clone(), passphrase.clone());
            let keep =
                (daily.value() as i64, weekly.value() as i64, monthly.value() as i64, yearly.value() as i64);
            let dry = dry_run.is_active();
            let run = run.downgrade();
            kit::bg(
                move || {
                    borgbox::prune(&server, &repo, passphrase.as_deref(), keep.0, keep.1, keep.2, keep.3, dry)
                },
                glib::clone!(
                    #[weak]
                    dialog,
                    #[weak]
                    error,
                    #[strong]
                    on_started,
                    move |result: Result<BorgBoxJobStart, String>| {
                        if let Some(run) = run.upgrade() {
                            run.set_sensitive(true);
                        }
                        match result {
                            Ok(start) => {
                                on_started(start);
                                dialog.close();
                            }
                            Err(e) => kit::set_error(&error, Some(&e)),
                        }
                    }
                ),
            );
        }
    ));

    dialog.present(Some(parent));
}
