//! Edit sheet for a BorgBase repository (BorgMac's `BorgBaseEditRepoSheet`).
//! Covers "General" (name, quota, inactivity alert), access control and
//! server-side scheduled compaction.
//!
//! Only fields the user actually changed are sent to the API (the partial
//! `update_repo` helper), so nothing else on the repo is clobbered by a blind
//! rewrite.
use super::borg_wizard::{Segmented, sheet_header};
use super::kit::{self, icons};
use super::state::AppState;
use crate::core::borgbase::{self, BorgBaseRepo, BorgBaseRepoUpdate, BorgBaseSSHKey};
use crate::core::format;
use adw::prelude::*;
use gtk::glib;
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

/// The interval unit is a free-form string on the BorgBase side — we offer the
/// three common buckets and preserve any exotic value the server reports
/// unchanged (until the user saves with the schedule on).
const UNITS: [(&str, &str); 3] = [("day", "day(s)"), ("week", "week(s)"), ("month", "month(s)")];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Access {
    None,
    Full,
    AppendOnly,
}

const ACCESS_LEVELS: [(Access, &str); 3] = [(Access::None, "None"), (Access::Full, "Full"), (Access::AppendOnly, "Append")];

/// Parses the user-entered GB string to MB (what the API expects). `None`
/// for invalid/empty input so the form can disable Save.
fn parse_quota_mb(input: &str) -> Option<i64> {
    let gb: f64 = input.trim().replace(',', ".").parse().ok()?;
    (gb.is_finite() && gb > 0.0).then(|| (gb * 1000.0) as i64)
}

/// Stored as MB, shown as GB. Strips the trailing .0 for whole numbers.
fn quota_gb_text(quota_mb: Option<f64>) -> String {
    match quota_mb.filter(|q| *q > 0.0) {
        Some(q) => {
            let gb = q / 1000.0;
            if gb.fract() == 0.0 { format!("{}", gb as i64) } else { format!("{gb:.2}") }
        }
        None => String::new(),
    }
}

fn formatted_created_at(repo: &BorgBaseRepo) -> Option<String> {
    let raw = repo.created_at.as_deref().filter(|s| !s.is_empty())?;
    Some(match format::parse_rfc3339(raw) {
        Some(date) => date.with_timezone(&chrono::Local).format("%-d %b %Y").to_string(),
        None => raw.to_string(),
    })
}

/// IANA name of the system time zone (`TimeZone.current.identifier`).
fn local_timezone() -> String {
    let id = glib::TimeZone::local().identifier().to_string();
    if id.contains('/') || id == "UTC" {
        return id;
    }
    std::fs::read_link("/etc/localtime")
        .ok()
        .and_then(|p| p.to_string_lossy().split_once("zoneinfo/").map(|(_, tz)| tz.to_string()))
        .unwrap_or_else(|| "UTC".to_string())
}

/// The values of the form, as the user left them.
struct Form {
    name: String,
    quota_enabled: bool,
    quota_mb: Option<i64>,
    alert_days: i64,
    compaction_enabled: bool,
    compaction_interval: i64,
    compaction_unit: &'static str,
    compaction_hour: i64,
    compaction_timezone: String,
    access: BTreeMap<String, Access>,
}

/// Diff between the repo as fetched and the form: only what changed is set.
fn build_update(repo: &BorgBaseRepo, form: &Form) -> BorgBaseRepoUpdate {
    let mut update = BorgBaseRepoUpdate::default();

    let trimmed = form.name.trim();
    if trimmed != repo.name {
        update.name = Some(trimmed.to_string());
    }
    if form.quota_enabled != repo.quota_enabled.unwrap_or(false) {
        update.quota_enabled = Some(form.quota_enabled);
    }
    // Only push quota when it's enabled — sending quota:0 with the toggle on
    // would let BorgBase reject the request for being at/under the current
    // usage. When the toggle is off the server ignores the value anyway.
    if form.quota_enabled {
        let stored_mb = repo.quota.unwrap_or(0.0) as i64;
        update.quota = form.quota_mb.filter(|mb| *mb != stored_mb);
    }
    if form.alert_days != repo.alert_days.unwrap_or(0) {
        update.alert_days = Some(form.alert_days);
    }
    if form.compaction_enabled != repo.compaction_enabled.unwrap_or(false) {
        update.compaction_enabled = Some(form.compaction_enabled);
    }
    // Only push interval/unit/hour/tz when the schedule is on. Server ignores
    // them otherwise, and sending them unchanged when the repo is freshly
    // initialised would serialise whatever random defaults we picked into
    // the user's settings.
    if form.compaction_enabled {
        if form.compaction_interval != repo.compaction_interval.unwrap_or(-1) {
            update.compaction_interval = Some(form.compaction_interval);
        }
        let current_unit = repo.compaction_interval_unit.as_deref().map(str::to_lowercase).unwrap_or_default();
        if form.compaction_unit != current_unit {
            update.compaction_interval_unit = Some(form.compaction_unit.to_string());
        }
        if form.compaction_hour != repo.compaction_hour.unwrap_or(-1) {
            update.compaction_hour = Some(form.compaction_hour);
        }
        if form.compaction_timezone != repo.compaction_hour_timezone.as_deref().unwrap_or("") {
            update.compaction_hour_timezone = Some(form.compaction_timezone.clone());
        }
    }

    // ACL diff: if ANY of the lists changed, send them all together. BorgBase
    // validates "a key can't be in two roles at once" across the submitted
    // payload, so moving a key between lists only works when both the
    // destination and the source list are sent in the same mutation —
    // otherwise the server sees the key in two roles and rejects with
    // "Can't use a SSH key for different roles."
    let with = |level: Access| -> BTreeSet<String> {
        form.access.iter().filter(|(_, l)| **l == level).map(|(id, _)| id.clone()).collect()
    };
    let (desired_full, desired_append) = (with(Access::Full), with(Access::AppendOnly));
    let current_full: BTreeSet<String> = repo.full_access_keys.clone().unwrap_or_default().into_iter().collect();
    let current_append: BTreeSet<String> = repo.append_only_keys.clone().unwrap_or_default().into_iter().collect();
    if desired_full != current_full || desired_append != current_append {
        update.full_access_keys = Some(desired_full.into_iter().collect());
        update.append_only_keys = Some(desired_append.into_iter().collect());
    }
    update
}

/// Initial per-key access. BorgBase stores disjoint id lists on the repo; in
/// the UI we collapse them into one pick-per-key because a key can never
/// appear in more than one list at a time anyway.
fn seed_access(repo: &BorgBaseRepo) -> BTreeMap<String, Access> {
    let mut map = BTreeMap::new();
    for id in repo.full_access_keys.iter().flatten() {
        map.insert(id.clone(), Access::Full);
    }
    for id in repo.append_only_keys.iter().flatten() {
        map.insert(id.clone(), Access::AppendOnly);
    }
    map
}

/// The steppers can't show a server value outside their range. As long as
/// the user leaves such a stepper alone, keep reporting the server's value so
/// saving doesn't silently rewrite it.
fn unclamped(shown: i64, server: i64, min: i64, max: i64) -> i64 {
    if shown == server.clamp(min, max) { server } else { shown }
}

fn note(text: &str) -> gtk::Label {
    let l = kit::caption(text);
    l.add_css_class("bb-form-note");
    l
}

pub fn present(
    parent: &impl IsA<gtk::Widget>,
    _app: &Rc<AppState>,
    repo: &BorgBaseRepo,
    account_keys: &[BorgBaseSSHKey],
    on_saved: impl Fn() + 'static,
) {
    let (dialog, content) = kit::sheet("", 520, 560);
    dialog.set_title(&format!("Edit {}", repo.name));
    content.set_spacing(14);
    content.append(&sheet_header(
        icons::DRIVE_WRENCH,
        &format!("Edit {}", repo.name),
        "Settings stored on BorgBase — changes apply server-side.",
    ));
    content.append(&kit::divider());

    let form = kit::vbox(14);
    form.add_css_class("bb-compact");
    form.set_margin_end(10);

    // -------------------------------------------------------------- General
    let general = kit::form_group();
    general.set_title("General");
    let name = adw::EntryRow::builder().title("Name").text(&repo.name).activates_default(true).build();
    general.add(&name);
    if let Some(created) = formatted_created_at(repo) {
        let row = adw::ActionRow::builder().title("Created").build();
        row.add_suffix(&kit::label(&created, &["secondary"]));
        general.add(&row);
    }
    let id_row = adw::ActionRow::builder().title("Repository ID").build();
    let id_label = kit::label(&repo.id, &["mono", "secondary"]);
    id_label.set_selectable(true);
    id_row.add_suffix(&id_label);
    general.add(&id_row);
    form.append(&general);

    // ---------------------------------------------------------------- Quota
    let quota = kit::form_group();
    quota.set_title("Quota");
    let quota_enabled = adw::SwitchRow::builder().title("Enforce quota").active(repo.quota_enabled.unwrap_or(false)).build();
    let quota_gb = adw::EntryRow::builder().title("Limit").text(quota_gb_text(repo.quota)).activates_default(true).build();
    quota_gb.set_input_purpose(gtk::InputPurpose::Number);
    quota_gb.add_suffix(&kit::label("GB", &["secondary"]));
    let quota_note = note(
        "BorgBase will reject writes that push the repo past this limit. Leave the toggle off for unlimited usage.",
    );
    quota.add(&quota_enabled);
    quota.add(&quota_gb);
    quota.add(&quota_note);
    form.append(&quota);

    // ----------------------------------------------------- Inactivity alert
    let alert = kit::form_group();
    alert.set_title("Inactivity alert");
    let alert_days = kit::spin_row("", repo.alert_days.unwrap_or(0).clamp(0, 365), 0, 365);
    alert.add(&alert_days);
    alert.add(&note(
        "BorgBase emails you if the repo hasn't received a backup within this window. Set to 0 to turn it off.",
    ));
    form.append(&alert);

    // ------------------------------------------------------- Access control
    let access_group = kit::form_group();
    access_group.set_title("Access control");
    let seeded = seed_access(repo);
    let mut pickers: Vec<(String, Segmented)> = vec![];
    if account_keys.is_empty() {
        let empty = adw::ActionRow::new();
        empty.set_activatable(false);
        empty.add_prefix(&kit::caption("No SSH keys on this BorgBase account. Add one from the SSH Keys tab first."));
        access_group.add(&empty);
    } else {
        for key in account_keys {
            let row = adw::ActionRow::builder().title(glib::markup_escape_text(&key.name)).title_lines(1).build();
            if let Some(fp) = key.hash_md5.as_deref().filter(|fp| !fp.is_empty()) {
                row.set_subtitle(fp);
                row.set_subtitle_lines(1);
                row.add_css_class("bb-key-row");
            }
            let level = seeded.get(&key.id).copied().unwrap_or(Access::None);
            let labels: Vec<&str> = ACCESS_LEVELS.iter().map(|l| l.1).collect();
            let picker = Segmented::new(&labels, ACCESS_LEVELS.iter().position(|l| l.0 == level).unwrap_or(0));
            picker.widget.set_size_request(260, -1);
            picker.widget.set_halign(gtk::Align::End);
            row.add_suffix(&picker.widget);
            access_group.add(&row);
            pickers.push((key.id.clone(), picker));
        }
        access_group.add(&note("Full: read & write. Append: backups yes, delete/modify no."));
    }
    form.append(&access_group);

    // ------------------------------------------------- Scheduled compaction
    let compaction = kit::form_group();
    compaction.set_title("Scheduled compaction");
    let compaction_enabled = adw::SwitchRow::builder()
        .title("Auto-compact on a schedule")
        .active(repo.compaction_enabled.unwrap_or(false))
        .build();
    let interval = kit::spin_row("Every", repo.compaction_interval.unwrap_or(1).clamp(1, 30), 1, 30);
    let unit_labels: Vec<&str> = UNITS.iter().map(|u| u.1).collect();
    let server_unit = repo.compaction_interval_unit.as_deref().map(str::to_lowercase);
    let unit_index = server_unit.and_then(|u| UNITS.iter().position(|known| known.0 == u)).unwrap_or(1);
    let unit = gtk::DropDown::from_strings(&unit_labels);
    unit.set_selected(unit_index as u32);
    unit.set_valign(gtk::Align::Center);
    interval.add_suffix(&unit);
    let timezone = repo.compaction_hour_timezone.clone().filter(|tz| !tz.is_empty()).unwrap_or_else(local_timezone);
    let hour = kit::spin_row("At", repo.compaction_hour.unwrap_or(3).clamp(0, 23), 0, 23);
    hour.set_subtitle(&format!("({})", glib::markup_escape_text(&timezone)));
    hour.set_subtitle_lines(1);
    // A stepper, not a text field: "04:00" is neither numeric nor typed in.
    hour.set_numeric(false);
    hour.set_editable(false);
    hour.set_width_chars(5);
    hour.connect_output(|row| {
        row.set_text(&format!("{:02}:00", row.value() as i64));
        true
    });
    hour.update();
    let compaction_note = note(
        "BorgBase will run `borg compact` on this repo automatically in its own infra, reclaiming space freed by pruned archives without touching your computer.",
    );
    compaction.add(&compaction_enabled);
    compaction.add(&interval);
    compaction.add(&hour);
    compaction.add(&compaction_note);
    form.append(&compaction);

    content.append(&kit::scrolled(&form));

    // --------------------------------------------------------------- footer
    let spinner = gtk::Spinner::new();
    let cancel = kit::button("Cancel");
    let save = kit::primary_button("Save");
    content.append(&kit::button_row(Some(spinner.upcast_ref()), &[&cancel, &save]));
    dialog.set_default_widget(Some(&save));

    // Derived state: conditional rows, stepper caption, "Save" enabled.
    let saving = Rc::new(Cell::new(false));
    let update = Rc::new(glib::clone!(
        #[weak]
        name,
        #[weak]
        quota_enabled,
        #[weak]
        quota_gb,
        #[weak]
        quota_note,
        #[weak]
        alert_days,
        #[weak]
        compaction_enabled,
        #[weak]
        interval,
        #[weak]
        hour,
        #[weak]
        compaction_note,
        #[weak]
        save,
        #[strong]
        saving,
        move || {
            let quota_on = quota_enabled.is_active();
            quota_gb.set_visible(quota_on);
            quota_note.set_visible(quota_on);
            let days = alert_days.value() as i64;
            alert_days.set_title(&if days == 0 {
                "Alerts disabled".to_string()
            } else {
                format!("Alert after {days} day{} with no activity", if days == 1 { "" } else { "s" })
            });
            if days == 0 {
                alert_days.add_css_class("bb-dim-title");
            } else {
                alert_days.remove_css_class("bb-dim-title");
            }
            let compact_on = compaction_enabled.is_active();
            interval.set_visible(compact_on);
            hour.set_visible(compact_on);
            compaction_note.set_visible(compact_on);
            let can_save = !saving.get()
                && !name.text().trim().is_empty()
                && !(quota_on && parse_quota_mb(&quota_gb.text()).is_none());
            save.set_sensitive(can_save);
        }
    ));
    update();
    name.connect_changed(glib::clone!(
        #[strong]
        update,
        move |_| update()
    ));
    quota_gb.connect_changed(glib::clone!(
        #[strong]
        update,
        move |_| update()
    ));
    alert_days.connect_value_notify(glib::clone!(
        #[strong]
        update,
        move |_| update()
    ));
    for switch in [&quota_enabled, &compaction_enabled] {
        switch.connect_active_notify(glib::clone!(
            #[strong]
            update,
            move |_| update()
        ));
    }

    cancel.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| {
            dialog.close();
        }
    ));

    let repo = repo.clone();
    let on_saved = Rc::new(on_saved);
    save.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        #[weak]
        form,
        #[weak]
        cancel,
        #[weak]
        spinner,
        move |_| {
            let mut access = seeded.clone();
            for (id, picker) in &pickers {
                let level = ACCESS_LEVELS.get(picker.selected()).map(|l| l.0).unwrap_or(Access::None);
                access.insert(id.clone(), level);
            }
            let values = Form {
                name: name.text().to_string(),
                quota_enabled: quota_enabled.is_active(),
                quota_mb: parse_quota_mb(&quota_gb.text()),
                alert_days: unclamped(alert_days.value() as i64, repo.alert_days.unwrap_or(0), 0, 365),
                compaction_enabled: compaction_enabled.is_active(),
                compaction_interval: unclamped(interval.value() as i64, repo.compaction_interval.unwrap_or(1).max(1), 1, 30),
                compaction_unit: UNITS.get(unit.selected() as usize).map(|u| u.0).unwrap_or("week"),
                compaction_hour: hour.value() as i64,
                compaction_timezone: timezone.clone(),
                access,
            };
            let changes = build_update(&repo, &values);

            let set_saving = glib::clone!(
                #[strong]
                saving,
                #[strong]
                update,
                #[weak]
                dialog,
                #[weak]
                form,
                #[weak]
                cancel,
                #[weak]
                spinner,
                move |on: bool| {
                    saving.set(on);
                    form.set_sensitive(!on);
                    cancel.set_sensitive(!on);
                    dialog.set_can_close(!on);
                    spinner.set_spinning(on);
                    update();
                }
            );
            set_saving(true);
            let id = repo.id.clone();
            let on_saved = on_saved.clone();
            kit::bg(
                move || borgbase::update_repo(&id, &changes),
                move |result| {
                    set_saving(false);
                    match result {
                        Ok(()) => {
                            on_saved();
                            dialog.close();
                        }
                        Err(e) => kit::alert(&form, "Error", &e),
                    }
                },
            );
        }
    ));

    dialog.present(Some(parent));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(json: &str) -> BorgBaseRepo {
        serde_json::from_str(json).expect("valid repo json")
    }

    fn form_of(repo: &BorgBaseRepo) -> Form {
        Form {
            name: repo.name.clone(),
            quota_enabled: repo.quota_enabled.unwrap_or(false),
            quota_mb: parse_quota_mb(&quota_gb_text(repo.quota)),
            alert_days: repo.alert_days.unwrap_or(0),
            compaction_enabled: repo.compaction_enabled.unwrap_or(false),
            compaction_interval: repo.compaction_interval.unwrap_or(1).max(1),
            compaction_unit: "week",
            compaction_hour: repo.compaction_hour.unwrap_or(3),
            compaction_timezone: "Europe/Madrid".to_string(),
            access: seed_access(repo),
        }
    }

    #[test]
    fn quota_parsing() {
        assert_eq!(parse_quota_mb(" 2,5 "), Some(2500));
        assert_eq!(parse_quota_mb("100"), Some(100_000));
        assert_eq!(parse_quota_mb("0"), None);
        assert_eq!(parse_quota_mb("abc"), None);
        assert_eq!(quota_gb_text(Some(250_000.0)), "250");
        assert_eq!(quota_gb_text(Some(1500.0)), "1.50");
        assert_eq!(quota_gb_text(None), "");
    }

    #[test]
    fn untouched_form_sends_nothing() {
        let r = repo(
            r#"{"id":"1","name":"a","quota":5000,"quotaEnabled":true,"alertDays":3,"fullAccessKeys":["7"],"appendOnlyKeys":["8"]}"#,
        );
        assert_eq!(build_update(&r, &form_of(&r)), BorgBaseRepoUpdate::default());
    }

    #[test]
    fn only_changes_are_sent() {
        let r = repo(r#"{"id":"1","name":"a","quota":5000,"quotaEnabled":false,"fullAccessKeys":["7"]}"#);
        let mut f = form_of(&r);
        f.name = " b ".to_string();
        f.quota_enabled = true;
        f.quota_mb = Some(6000);
        f.alert_days = 2;
        f.compaction_enabled = true;
        f.access.insert("7".to_string(), Access::AppendOnly);
        f.access.insert("9".to_string(), Access::None);
        let u = build_update(&r, &f);
        assert_eq!(u.name.as_deref(), Some("b"));
        assert_eq!(u.quota_enabled, Some(true));
        assert_eq!(u.quota, Some(6000));
        assert_eq!(u.alert_days, Some(2));
        assert_eq!(u.compaction_enabled, Some(true));
        assert_eq!(u.compaction_interval, Some(1));
        assert_eq!(u.compaction_interval_unit.as_deref(), Some("week"));
        assert_eq!(u.compaction_hour, Some(3));
        assert_eq!(u.compaction_hour_timezone.as_deref(), Some("Europe/Madrid"));
        assert_eq!(u.full_access_keys, Some(vec![]));
        assert_eq!(u.append_only_keys, Some(vec!["7".to_string()]));
    }
}
