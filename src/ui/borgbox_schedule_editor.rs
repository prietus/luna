//! Create / edit form for a BorgBox maintenance schedule (BorgMac's
//! `AddEditScheduleSheet`). Presented as a sheet from the automation panel.
//! The caller supplies the server and an optional existing schedule; when
//! `None` the sheet is in create mode and loads the server's repo list so
//! the user can pick a target.
//!
//! The weekday / day fields are shown conditionally based on cadence, so
//! the user never has to manipulate a dummy value. On save we send only
//! the fields the daemon actually uses for that cadence — the server
//! ignores unused ones but being explicit avoids stale weekday/day values
//! sticking around after an edit flipped cadence.
use super::borgbox_alerts::sheet_header;
use super::kit::{self, icons};
use super::state::AppState;
use crate::core::borgbox::{self, BorgBoxSchedule, BorgBoxScheduleUpdate};
use crate::core::models::BorgBoxServer;
use adw::prelude::*;
use gtk::glib;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const KINDS: [(&str, &str); 2] =
    [("check", "Check (verify repo integrity)"), ("compact", "Compact (reclaim deleted segments)")];
const CADENCES: [(&str, &str); 3] = [("daily", "Daily"), ("weekly", "Weekly"), ("monthly", "Monthly")];
const WEEKDAY_NAMES: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];

/// Picker options as `(wire value, label)`. A value the daemon reports but
/// this build doesn't know yet is kept as an extra entry so editing such a
/// schedule doesn't silently rewrite it.
fn options(known: &[(&str, &str)], current: &str) -> Vec<(String, String)> {
    let mut list: Vec<(String, String)> = known.iter().map(|(v, l)| (v.to_string(), l.to_string())).collect();
    if !list.iter().any(|(v, _)| v == current) {
        list.push((current.to_string(), current.to_string()));
    }
    list
}

fn picker(title: &str, options: &[(String, String)], current: &str) -> adw::ComboRow {
    let labels: Vec<&str> = options.iter().map(|(_, l)| l.as_str()).collect();
    let selected = options.iter().position(|(v, _)| v == current).unwrap_or(0);
    let row = kit::combo_row(title, &labels, selected);
    // The stock item widget ellipsizes after ~20 characters, which cuts the
    // kind descriptions; show them in full like the macOS popup button.
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, obj| {
        if let Some(item) = obj.downcast_ref::<gtk::ListItem>() {
            item.set_child(Some(&kit::label("", &[])));
        }
    });
    factory.connect_bind(|_, obj| {
        let Some(item) = obj.downcast_ref::<gtk::ListItem>() else { return };
        let text = item.item().and_downcast::<gtk::StringObject>().map(|s| s.string());
        if let (Some(label), Some(text)) = (item.child().and_downcast::<gtk::Label>(), text) {
            label.set_text(&text);
        }
    });
    row.set_factory(Some(&factory));
    row
}

fn two_digit_dropdown(count: u32, selected: i64) -> gtk::DropDown {
    let items: Vec<String> = (0..count).map(|i| format!("{i:02}")).collect();
    let refs: Vec<&str> = items.iter().map(String::as_str).collect();
    let dropdown = gtk::DropDown::from_strings(&refs);
    dropdown.set_selected(selected.clamp(0, count as i64 - 1) as u32);
    dropdown.set_valign(gtk::Align::Center);
    dropdown
}

fn footer_help(cadence: &str, weekday: usize, day: i64, hour: u32, minute: u32) -> String {
    let time = format!("{hour:02}:{minute:02}");
    match cadence {
        "weekly" => format!(
            "Fires every {} at {time} in the server's local time. If a previous job is still running when the tick arrives, this one is skipped and retried on the next tick.",
            WEEKDAY_NAMES.get(weekday).copied().unwrap_or("—")
        ),
        "monthly" => format!(
            "Fires on day {day} of each month at {time}. Day values above 28 aren't accepted so short months don't silently miss."
        ),
        _ => format!("Fires every day at {time} in the server's local time."),
    }
}

pub fn present(
    parent: &impl IsA<gtk::Widget>,
    _app: &Rc<AppState>,
    server: &BorgBoxServer,
    existing: Option<&BorgBoxSchedule>,
    default_repo: Option<&str>,
    on_saved: impl Fn(BorgBoxSchedule) + 'static,
) {
    let is_editing = existing.is_some();
    let title = if is_editing { "Edit schedule" } else { "New schedule" };
    let (dialog, content) = kit::bare_sheet(title, 520, 560);

    let (header, _) = sheet_header(
        if is_editing { icons::PENCIL } else { icons::PLUS_CIRCLE },
        title,
        "Runs on the BorgBox daemon — no client involvement needed.",
        false,
    );
    header.set_margin_start(20);
    header.set_margin_end(20);
    header.set_margin_top(14);
    header.set_margin_bottom(14);
    content.append(&header);
    content.append(&kit::divider());

    let form = kit::vbox(14);
    form.add_css_class("bb2-form");
    form.set_margin_top(12);
    form.set_margin_bottom(12);
    form.set_margin_start(20);
    form.set_margin_end(20);

    // ---------------------------------------------------------------- Target
    let target = kit::form_group();
    target.set_title("Target");
    let server_row = adw::ActionRow::builder().title("Server").build();
    server_row.add_suffix(&kit::label(&server.name, &["secondary"]));
    target.add(&server_row);

    // The chosen repo name ("" while nothing is selected) and the names
    // behind the picker's entries, in order.
    let repo = Rc::new(RefCell::new(
        existing.map(|s| s.repo.clone()).or_else(|| default_repo.map(str::to_string)).unwrap_or_default(),
    ));
    let repo_entries: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(vec![]));
    let filling = Rc::new(Cell::new(false));
    let repo_picker = adw::ComboRow::builder().title("Repository").build();
    if let Some(existing) = existing {
        let row = adw::ActionRow::builder().title("Repository").build();
        row.add_suffix(&kit::label(&existing.repo, &["mono", "secondary"]));
        target.add(&row);
    } else {
        target.add(&repo_picker);
    }
    form.append(&target);

    // ------------------------------------------------------------------- Job
    let job = kit::form_group();
    job.set_title("Job");
    let kinds = options(&KINDS, existing.map_or("check", |s| s.kind.as_str()));
    let cadences = options(&CADENCES, existing.map_or("weekly", |s| s.cadence.as_str()));
    let kind = picker("Kind", &kinds, existing.map_or("check", |s| s.kind.as_str()));
    let cadence = picker("Cadence", &cadences, existing.map_or("weekly", |s| s.cadence.as_str()));
    let weekday = kit::combo_row("Day of week", &WEEKDAY_NAMES, existing.map_or(0, |s| s.weekday.clamp(0, 6)) as usize);
    let days: Vec<String> = (1..29).map(|i| i.to_string()).collect();
    let day_refs: Vec<&str> = days.iter().map(String::as_str).collect();
    let day =
        kit::combo_row("Day of month", &day_refs, (existing.map_or(1, |s| s.day.max(1)).clamp(1, 28) - 1) as usize);
    day.set_tooltip_text(Some("1–28 only, to avoid missing short months."));
    let hour = two_digit_dropdown(24, existing.map_or(3, |s| s.hour));
    let minute = two_digit_dropdown(60, existing.map_or(0, |s| s.minute));
    let time_row = adw::ActionRow::builder().title("Hour").build();
    time_row.add_suffix(&hour);
    let minute_label = kit::label("Minute", &[]);
    minute_label.set_margin_start(12);
    time_row.add_suffix(&minute_label);
    time_row.add_suffix(&minute);
    job.add(&kind);
    job.add(&cadence);
    job.add(&weekday);
    job.add(&day);
    job.add(&time_row);
    form.append(&job);

    let last = kit::vbox(6);
    let enabled_group = kit::form_group();
    let enabled = adw::SwitchRow::builder().title("Enabled").active(existing.is_none_or(|s| s.enabled)).build();
    enabled_group.add(&enabled);
    last.append(&enabled_group);
    let help = kit::caption("");
    help.set_margin_start(4);
    last.append(&help);
    form.append(&last);
    content.append(&kit::scrolled(&form));

    // Kept outside the scrolling form so a failed save is never below the fold.
    let error = kit::error_label();
    error.set_margin_start(24);
    error.set_margin_end(24);
    error.set_margin_bottom(8);
    content.append(&error);
    content.append(&kit::divider());

    let cancel = kit::button("Cancel");
    let save = kit::primary_button(if is_editing { "Save" } else { "Create" });
    let footer = kit::button_row(None, &[&cancel, &save]);
    footer.set_margin_start(20);
    footer.set_margin_end(20);
    footer.set_margin_top(12);
    footer.set_margin_bottom(12);
    content.append(&footer);
    dialog.set_default_widget(Some(&save));

    // --------------------------------------------------------- derived state
    let saving = Rc::new(Cell::new(false));
    let cadence_value = glib::clone!(
        #[weak]
        cadence,
        #[upgrade_or_default]
        move || cadences.get(cadence.selected() as usize).map(|(v, _)| v.clone()).unwrap_or_default()
    );
    let update = Rc::new(glib::clone!(
        #[weak]
        weekday,
        #[weak]
        day,
        #[weak]
        hour,
        #[weak]
        minute,
        #[weak]
        help,
        #[weak]
        save,
        #[strong]
        repo,
        #[strong]
        saving,
        #[strong]
        cadence_value,
        move || {
            let cadence = cadence_value();
            weekday.set_visible(cadence == "weekly");
            day.set_visible(cadence == "monthly");
            help.set_text(&footer_help(
                &cadence,
                weekday.selected() as usize,
                day.selected() as i64 + 1,
                hour.selected(),
                minute.selected(),
            ));
            save.set_sensitive(!saving.get() && !repo.borrow().is_empty());
        }
    ));
    update();
    for row in [&cadence, &weekday, &day] {
        row.connect_selected_notify(glib::clone!(
            #[strong]
            update,
            move |_| update()
        ));
    }
    for dropdown in [&hour, &minute] {
        dropdown.connect_selected_notify(glib::clone!(
            #[strong]
            update,
            move |_| update()
        ));
    }

    // Rebuilds the repo picker: a placeholder entry while nothing is chosen,
    // then the server's registered repos.
    let fill_repos = Rc::new(glib::clone!(
        #[weak]
        repo_picker,
        #[strong]
        repo,
        #[strong]
        repo_entries,
        #[strong]
        filling,
        move |names: &[String], loading: bool| {
            let current = repo.borrow().clone();
            let mut entries: Vec<String> = vec![];
            let mut labels: Vec<String> = vec![];
            if current.is_empty() {
                entries.push(String::new());
                labels.push(if loading { "Loading…" } else { "Select a repo…" }.to_string());
            } else if !names.contains(&current) {
                entries.push(current.clone());
                labels.push(current.clone());
            }
            for name in names {
                entries.push(name.clone());
                labels.push(name.clone());
            }
            let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            filling.set(true);
            repo_picker.set_model(Some(&gtk::StringList::new(&refs)));
            repo_picker.set_selected(entries.iter().position(|e| *e == current).unwrap_or(0) as u32);
            filling.set(false);
            *repo_entries.borrow_mut() = entries;
            repo_picker.set_sensitive(!loading && !names.is_empty());
        }
    ));
    repo_picker.connect_selected_notify(glib::clone!(
        #[strong]
        repo,
        #[strong]
        repo_entries,
        #[strong]
        filling,
        #[strong]
        update,
        move |row| {
            if filling.get() {
                return;
            }
            if let Some(name) = repo_entries.borrow().get(row.selected() as usize) {
                *repo.borrow_mut() = name.clone();
            }
            update();
        }
    ));

    if !is_editing {
        fill_repos(&[], true);
        let server_bg = server.clone();
        kit::bg(
            move || borgbox::list_repos(&server_bg),
            glib::clone!(
                #[weak]
                error,
                #[strong]
                repo,
                #[strong]
                fill_repos,
                #[strong]
                update,
                move |result: Result<Vec<borgbox::BorgBoxRemoteRepo>, String>| {
                    let names: Vec<String> = match result {
                        Ok(fetched) => fetched.into_iter().filter(|r| r.is_registered()).map(|r| r.name).collect(),
                        Err(e) => {
                            kit::set_error(&error, Some(&e));
                            vec![]
                        }
                    };
                    if repo.borrow().is_empty() {
                        if let Some(first) = names.first() {
                            *repo.borrow_mut() = first.clone();
                        }
                    }
                    fill_repos(&names, false);
                    update();
                }
            ),
        );
    }

    cancel.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| {
            dialog.close();
        }
    ));

    let server = server.clone();
    let existing = existing.cloned();
    let on_saved = Rc::new(on_saved);
    save.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        #[weak]
        error,
        move |_| {
            saving.set(true);
            update();
            kit::set_error(&error, None);

            let kind_value = kinds.get(kind.selected() as usize).map(|(v, _)| v.clone()).unwrap_or_default();
            let cadence = cadence_value();
            let (hour, minute) = (hour.selected() as i64, minute.selected() as i64);
            let weekday = (cadence == "weekly").then_some(weekday.selected() as i64);
            let day = (cadence == "monthly").then_some(day.selected() as i64 + 1);
            let is_enabled = enabled.is_active();
            let repo = repo.borrow().clone();
            let (server, existing) = (server.clone(), existing.clone());
            let (saving, update, on_saved) = (saving.clone(), update.clone(), on_saved.clone());
            kit::bg(
                move || match existing {
                    Some(existing) => {
                        let update = BorgBoxScheduleUpdate {
                            kind: Some(kind_value),
                            cadence: Some(cadence),
                            hour: Some(hour),
                            minute: Some(minute),
                            weekday,
                            day,
                            enabled: Some(is_enabled),
                        };
                        borgbox::update_schedule(&server, &existing.id, &update)
                    }
                    None => borgbox::create_schedule(
                        &server,
                        &repo,
                        &kind_value,
                        &cadence,
                        hour,
                        minute,
                        weekday.unwrap_or(0),
                        day.unwrap_or(0),
                        is_enabled,
                    ),
                },
                move |result| {
                    saving.set(false);
                    update();
                    match result {
                        Ok(saved) => {
                            on_saved(saved);
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
