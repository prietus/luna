//! Global overview of every maintenance schedule registered on every
//! configured BorgBox server (BorgMac's `AutomationPanel`). Fetches fresh
//! data from each daemon on open and after any create/update/delete so the
//! table stays in sync with the server state without needing a long-lived
//! store.
//!
//! Only covers BorgBox-side schedules (check / compact). Local-repo backup
//! schedules live in the repository store and are surfaced elsewhere; mixing
//! both kinds in one table would conflate very different lifecycles.
use super::borgbox_alerts::sheet_header;
use super::borgbox_schedule_editor;
use super::kit::{self, icons};
use super::state::{AppState, connect_while_alive};
use crate::core::borgbox::{self, BorgBoxSchedule, BorgBoxScheduleUpdate};
use crate::core::format;
use crate::core::models::BorgBoxServer;
use adw::prelude::*;
use chrono::Local;
use gtk::{gio, glib};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// Flat denormalised row — one per schedule across all servers. Built from
/// the schedule plus the server it came from so the table can show both
/// columns without a second lookup.
#[derive(Clone, PartialEq)]
struct ScheduleRow {
    server: BorgBoxServer,
    schedule: BorgBoxSchedule,
}

impl ScheduleRow {
    fn id(&self) -> String {
        format!("{}::{}", self.server.id, self.schedule.id)
    }
}

struct Panel {
    app: Rc<AppState>,
    /// Content box of the sheet; nested sheets and alerts present over it.
    host: gtk::Box,
    stack: gtk::Stack,
    store: gio::ListStore,
    error: gtk::Label,
    add_button: gtk::MenuButton,
    refresh: gtk::Button,
    rows: RefCell<Vec<ScheduleRow>>,
    loading: Cell<bool>,
    /// Row id of the schedule whose `enabled` toggle is in flight, to
    /// disable further clicks.
    toggling: RefCell<Option<String>>,
}

pub fn present(parent: &impl IsA<gtk::Widget>, app: &Rc<AppState>) {
    let (dialog, content) = kit::bare_sheet("Automation", 880, 620);

    // ---------------------------------------------------------------- header
    let (header, _) =
        sheet_header(icons::HISTORY, "Automation", "Scheduled maintenance on your BorgBox servers", false);
    header.set_margin_start(20);
    header.set_margin_end(20);
    header.set_margin_top(14);
    header.set_margin_bottom(14);
    let add_button = gtk::MenuButton::new();
    let add_label = kit::hbox(4);
    add_label.append(&kit::icon(icons::PLUS));
    add_label.append(&gtk::Label::new(Some("Add schedule…")));
    add_button.set_child(Some(&add_label));
    add_button.add_css_class("flat");
    add_button.set_valign(gtk::Align::Center);
    add_button.set_tooltip_text(Some("Create a new schedule on one of your BorgBox servers"));
    header.append(&add_button);
    let refresh = kit::icon_button(icons::REFRESH, "Refresh");
    refresh.add_css_class("plain");
    refresh.set_valign(gtk::Align::Center);
    header.append(&refresh);
    let close = kit::button("Close");
    close.set_valign(gtk::Align::Center);
    header.append(&close);
    content.append(&header);
    content.append(&kit::divider());

    // --------------------------------------------------------------- content
    let error = kit::error_label();
    error.set_margin_start(20);
    error.set_margin_end(20);
    error.set_margin_top(8);
    content.append(&error);

    let stack = gtk::Stack::new();
    stack.set_vexpand(true);
    stack.add_named(
        &kit::empty_state(
            icons::SERVER,
            "No BorgBox servers",
            "Add a BorgBox server from the sidebar to create maintenance schedules.",
        ),
        Some("no-servers"),
    );
    let loading = kit::vbox(8);
    loading.set_valign(gtk::Align::Center);
    loading.set_halign(gtk::Align::Center);
    loading.append(&kit::spinner());
    loading.append(&kit::label("Loading schedules…", &["secondary"]));
    stack.add_named(&loading, Some("loading"));
    let empty = kit::empty_state(
        icons::CLOCK_QUESTION,
        "No schedules yet",
        "Nothing is scheduled on any of your BorgBox servers. Create one to get weekly check or compact runs.",
    );
    let empty_add = kit::button("Add schedule…");
    empty_add.set_halign(gtk::Align::Center);
    empty.set_child(Some(&empty_add));
    stack.add_named(&empty, Some("empty"));

    let store = gio::ListStore::new::<glib::BoxedAnyObject>();
    let table = gtk::ColumnView::new(Some(gtk::NoSelection::new(Some(store.clone()))));
    table.add_css_class("mac-table");
    table.add_css_class("bb2-schedules");
    stack.add_named(&kit::scrolled(&table), Some("table"));
    content.append(&stack);

    let panel = Rc::new(Panel {
        app: app.clone(),
        host: content.clone(),
        stack,
        store,
        error,
        add_button: add_button.clone(),
        refresh: refresh.clone(),
        rows: RefCell::new(vec![]),
        loading: Cell::new(false),
        toggling: RefCell::new(None),
    });
    add_columns(&panel, &table);

    // "Add schedule…" lists the servers; the entries carry the server id.
    let actions = gio::SimpleActionGroup::new();
    let add = gio::SimpleAction::new("add", Some(glib::VariantTy::STRING));
    add.connect_activate(glib::clone!(
        #[weak]
        panel,
        move |_, parameter| {
            let id = parameter.and_then(|p| p.str().map(str::to_string)).unwrap_or_default();
            let server = panel.app.borgbox.servers().into_iter().find(|s| s.id.to_string() == id);
            if let Some(server) = server {
                edit(&panel, &server, None);
            }
        }
    ));
    actions.add_action(&add);
    header.insert_action_group("automation", Some(&actions));

    empty_add.connect_clicked(glib::clone!(
        #[weak]
        panel,
        move |_| {
            if let Some(first) = panel.app.borgbox.first() {
                edit(&panel, &first, None);
            }
        }
    ));
    refresh.connect_clicked(glib::clone!(
        #[weak]
        panel,
        move |_| load(&panel)
    ));
    close.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| {
            dialog.close();
        }
    ));
    connect_while_alive(
        app,
        |a| &a.borgbox.changed,
        &content,
        glib::clone!(
            #[weak]
            panel,
            move || load(&panel)
        ),
    );

    // Handlers only hold weak references; the panel state lives until the
    // dialog is closed.
    let keep = RefCell::new(Some(panel.clone()));
    dialog.connect_closed(move |_| {
        keep.borrow_mut().take();
    });

    load(&panel);
    dialog.present(Some(parent));
}

// MARK: - Table

fn column(
    table: &gtk::ColumnView,
    title: &str,
    width: i32,
    build: impl Fn(&ScheduleRow) -> gtk::Widget + 'static,
) {
    let factory = gtk::SignalListItemFactory::new();
    // Cells are rebuilt on every bind: the table is small and several cells
    // carry per-row controls (switch, buttons).
    factory.connect_bind(move |_, obj| {
        let Some(cell) = obj.downcast_ref::<gtk::ColumnViewCell>() else { return };
        let Some(row) = cell.item().and_then(|o| kit::unbox::<ScheduleRow>(&o)) else { return };
        let widget = build(&row);
        widget.set_valign(gtk::Align::Center);
        cell.set_child(Some(&widget));
    });
    factory.connect_unbind(|_, obj| {
        if let Some(cell) = obj.downcast_ref::<gtk::ColumnViewCell>() {
            cell.set_child(gtk::Widget::NONE);
        }
    });
    let col = gtk::ColumnViewColumn::new(Some(title), Some(factory));
    col.set_resizable(true);
    if width > 0 {
        col.set_fixed_width(width);
    } else {
        col.set_expand(true);
    }
    table.append_column(&col);
}

fn add_columns(panel: &Rc<Panel>, table: &gtk::ColumnView) {
    column(table, "Server", 120, |row| kit::ellipsized(&row.server.name, false, &[]).upcast());
    column(table, "Repo", 0, |row| kit::ellipsized(&row.schedule.repo, true, &["mono"]).upcast());
    column(table, "Kind", 100, |row| {
        let cell = kit::hbox(4);
        cell.append(&kit::icon_styled(icon_for_kind(&row.schedule.kind), &["tint"]));
        cell.append(&kit::label(&capitalized(&row.schedule.kind), &[]));
        cell.upcast()
    });
    column(table, "Cadence", 132, |row| kit::ellipsized(&cadence_description(&row.schedule), false, &[]).upcast());
    column(table, "Next", 112, |row| {
        let label = kit::ellipsized(&next_run_label(&row.schedule), false, &[]);
        if !row.schedule.enabled {
            label.add_css_class("secondary");
        }
        let tooltip = absolute_date_tooltip(&row.schedule);
        if !tooltip.is_empty() {
            label.set_tooltip_text(Some(&tooltip));
        }
        label.upcast()
    });
    column(table, "Last", 124, |row| {
        let cell = kit::hbox(4);
        let (icon, color) = icon_for_status(&row.schedule.last_status);
        cell.append(&kit::icon_styled(icon, &[color, "bb2-status-icon"]));
        cell.append(&kit::ellipsized(&last_run_label(&row.schedule), false, &["secondary"]));
        cell.set_tooltip_text(Some(&last_run_tooltip(&row.schedule)));
        cell.upcast()
    });
    column(
        table,
        "Enabled",
        76,
        glib::clone!(
            #[weak]
            panel,
            #[upgrade_or_else]
            || gtk::Box::new(gtk::Orientation::Horizontal, 0).upcast(),
            move |row| {
                let switch = gtk::Switch::new();
                switch.set_active(row.schedule.enabled);
                switch.set_halign(gtk::Align::Start);
                switch.set_sensitive(panel.toggling.borrow().as_deref() != Some(row.id().as_str()));
                switch.set_tooltip_text(Some(if row.schedule.enabled {
                    "Pause this schedule"
                } else {
                    "Resume this schedule"
                }));
                switch.connect_state_set(glib::clone!(
                    #[strong]
                    row,
                    #[weak]
                    panel,
                    #[upgrade_or]
                    glib::Propagation::Proceed,
                    move |switch, enabled| {
                        switch.set_sensitive(false);
                        toggle_enabled(&panel, &row, enabled);
                        glib::Propagation::Proceed
                    }
                ));
                switch.upcast()
            }
        ),
    );
    column(
        table,
        "Actions",
        84,
        glib::clone!(
            #[weak]
            panel,
            #[upgrade_or_else]
            || gtk::Box::new(gtk::Orientation::Horizontal, 0).upcast(),
            move |row| {
                let cell = kit::hbox(6);
                let edit_button = kit::icon_button(icons::PENCIL, "Edit schedule");
                edit_button.add_css_class("plain");
                edit_button.connect_clicked(glib::clone!(
                    #[weak]
                    panel,
                    #[strong]
                    row,
                    move |_| edit(&panel, &row.server, Some(&row.schedule))
                ));
                cell.append(&edit_button);
                let delete_button = kit::icon_button(icons::TRASH, "Delete schedule");
                delete_button.add_css_class("plain");
                delete_button.add_css_class("secondary");
                delete_button.connect_clicked(glib::clone!(
                    #[weak]
                    panel,
                    #[strong]
                    row,
                    move |_| confirm_delete(&panel, &row)
                ));
                cell.append(&delete_button);
                cell.upcast()
            }
        ),
    );
}

/// Pushes `rows` into the table and picks the page to show.
fn sync(panel: &Rc<Panel>) {
    let rows = panel.rows.borrow().clone();
    let items: Vec<glib::BoxedAnyObject> = rows.iter().cloned().map(kit::boxed).collect();
    panel.store.splice(0, panel.store.n_items(), &items);

    let servers = panel.app.borgbox.servers();
    let page = if servers.is_empty() {
        "no-servers"
    } else if !rows.is_empty() {
        "table"
    } else if panel.loading.get() {
        "loading"
    } else {
        "empty"
    };
    panel.stack.set_visible_child_name(page);

    panel.add_button.set_visible(!servers.is_empty());
    panel.add_button.set_sensitive(!panel.loading.get());
    panel.refresh.set_sensitive(!panel.loading.get());
    let menu = gio::Menu::new();
    for server in &servers {
        let item = gio::MenuItem::new(Some(&server.name), None);
        item.set_action_and_target_value(Some("automation.add"), Some(&server.id.to_string().to_variant()));
        menu.append_item(&item);
    }
    panel.add_button.set_menu_model(Some(&menu));
}

// MARK: - Actions

fn load(panel: &Rc<Panel>) {
    panel.loading.set(true);
    kit::set_error(&panel.error, None);
    sync(panel);

    let servers = panel.app.borgbox.servers();
    let weak = Rc::downgrade(panel);
    kit::bg(
        move || {
            let mut collected: Vec<ScheduleRow> = vec![];
            let mut failures: Vec<String> = vec![];
            for server in servers {
                match borgbox::list_schedules(&server) {
                    Ok(schedules) => collected
                        .extend(schedules.into_iter().map(|schedule| ScheduleRow { server: server.clone(), schedule })),
                    Err(e) => failures.push(format!("{}: {e}", server.name)),
                }
            }
            (collected, failures)
        },
        move |(mut collected, failures)| {
            let Some(panel) = weak.upgrade() else { return };
            panel.loading.set(false);
            sort_rows(&mut collected);
            *panel.rows.borrow_mut() = collected;
            let message = failures.join("\n");
            kit::set_error(&panel.error, (!failures.is_empty()).then_some(message.as_str()));
            sync(&panel);
        },
    );
}

fn edit(panel: &Rc<Panel>, server: &BorgBoxServer, existing: Option<&BorgBoxSchedule>) {
    let weak = Rc::downgrade(panel);
    let target = server.clone();
    borgbox_schedule_editor::present(&panel.host, &panel.app, server, existing, None, move |saved| {
        if let Some(panel) = weak.upgrade() {
            upsert(&panel, &target, saved);
        }
    });
}

fn toggle_enabled(panel: &Rc<Panel>, row: &ScheduleRow, enabled: bool) {
    *panel.toggling.borrow_mut() = Some(row.id());
    let (server, id) = (row.server.clone(), row.schedule.id.clone());
    let target = row.server.clone();
    let weak = Rc::downgrade(panel);
    kit::bg(
        move || {
            let update = BorgBoxScheduleUpdate { enabled: Some(enabled), ..Default::default() };
            borgbox::update_schedule(&server, &id, &update)
        },
        move |result| {
            let Some(panel) = weak.upgrade() else { return };
            *panel.toggling.borrow_mut() = None;
            match result {
                Ok(updated) => upsert(&panel, &target, updated),
                Err(e) => {
                    kit::set_error(&panel.error, Some(&e));
                    // Put the switch back to the daemon's state.
                    sync(&panel);
                }
            }
        },
    );
}

fn confirm_delete(panel: &Rc<Panel>, row: &ScheduleRow) {
    let weak = Rc::downgrade(panel);
    let target = row.clone();
    kit::confirm(
        &panel.host,
        "Delete schedule?",
        &format!(
            "This removes the {} schedule for {} on {}. The daemon stops firing it immediately.",
            row.schedule.kind, row.schedule.repo, row.server.name
        ),
        "Delete",
        "Cancel",
        true,
        move || {
            if let Some(panel) = weak.upgrade() {
                delete(&panel, &target);
            }
        },
    );
}

fn delete(panel: &Rc<Panel>, row: &ScheduleRow) {
    let (server, id) = (row.server.clone(), row.schedule.id.clone());
    let row_id = row.id();
    let weak = Rc::downgrade(panel);
    kit::bg(
        move || borgbox::delete_schedule(&server, &id),
        move |result| {
            let Some(panel) = weak.upgrade() else { return };
            match result {
                Ok(()) => {
                    panel.rows.borrow_mut().retain(|r| r.id() != row_id);
                    sync(&panel);
                }
                Err(e) => kit::set_error(&panel.error, Some(&e)),
            }
        },
    );
}

/// Replaces the existing row for the same (server, schedule id) pair or
/// inserts a new one, then re-sorts. Called after a successful
/// create/update so we never round-trip a full GET just to pick up one row
/// the daemon already returned.
fn upsert(panel: &Rc<Panel>, server: &BorgBoxServer, schedule: BorgBoxSchedule) {
    let new_row = ScheduleRow { server: server.clone(), schedule };
    {
        let mut rows = panel.rows.borrow_mut();
        match rows.iter().position(|r| r.id() == new_row.id()) {
            Some(idx) => rows[idx] = new_row,
            None => rows.push(new_row),
        }
        sort_rows(&mut rows);
    }
    sync(panel);
}

fn sort_rows(rows: &mut [ScheduleRow]) {
    let key = |s: &str| (s.to_lowercase(), s.to_string());
    rows.sort_by(|a, b| {
        key(&a.server.name)
            .cmp(&key(&b.server.name))
            .then_with(|| key(&a.schedule.repo).cmp(&key(&b.schedule.repo)))
            .then_with(|| a.schedule.kind.cmp(&b.schedule.kind))
    });
}

// MARK: - Formatting helpers

fn icon_for_kind(kind: &str) -> &'static str {
    match kind {
        "check" => "security-high-symbolic",    // checkmark.shield
        "compact" => icons::BOX,                // shippingbox.and.arrow.backward
        _ => "emblem-system-symbolic",          // gearshape
    }
}

fn capitalized(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars.flat_map(char::to_lowercase)).collect(),
        None => String::new(),
    }
}

fn cadence_description(s: &BorgBoxSchedule) -> String {
    let time = format!("{:02}:{:02}", s.hour, s.minute);
    match s.cadence.as_str() {
        "daily" => format!("Daily at {time}"),
        "weekly" => format!("{} at {time}", weekday_name(s.weekday)),
        "monthly" => format!("Day {} at {time}", s.day),
        other => format!("{other} at {time}"),
    }
}

fn weekday_name(n: i64) -> &'static str {
    const NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    usize::try_from(n).ok().and_then(|i| NAMES.get(i).copied()).unwrap_or("—")
}

fn next_run_label(s: &BorgBoxSchedule) -> String {
    if !s.enabled {
        return "Paused".to_string();
    }
    match s.next_run_date() {
        Some(date) => format::relative_short(&date),
        None => "—".to_string(),
    }
}

fn last_run_label(s: &BorgBoxSchedule) -> String {
    if s.last_status == "running" {
        return "running…".to_string();
    }
    match s.last_run_date() {
        Some(date) => format::relative_short(&date),
        None => "never".to_string(),
    }
}

fn last_run_tooltip(s: &BorgBoxSchedule) -> String {
    if s.last_run.is_empty() {
        return "This schedule has never fired.".to_string();
    }
    let mut parts: Vec<String> = vec![];
    if let Some(date) = s.last_run_date() {
        parts.push(format::medium_date_short_time(&date.with_timezone(&Local)));
    }
    match s.last_status.as_str() {
        "done" => parts.push("Completed successfully.".to_string()),
        "running" => parts.push("Currently running.".to_string()),
        "error" => parts.push("Last run failed.".to_string()),
        "skipped" => parts.push("Skipped: repo wasn't on disk at fire time.".to_string()),
        _ => {}
    }
    if !s.last_job_id.is_empty() {
        parts.push(format!("Job: {}", s.last_job_id));
    }
    parts.join("\n")
}

fn absolute_date_tooltip(s: &BorgBoxSchedule) -> String {
    s.next_run_date().map(|date| format::medium_date_short_time(&date.with_timezone(&Local))).unwrap_or_default()
}

/// Icon and colour class for a `last_status` value.
fn icon_for_status(status: &str) -> (&'static str, &'static str) {
    match status {
        "done" => (icons::CHECK_CIRCLE, "c-green"),
        "running" => (icons::SYNC, "c-orange"),
        "error" => (icons::WARNING, "c-red"),
        "skipped" => ("dialog-information-symbolic", "c-yellow"), // exclamationmark.circle
        _ => ("radio-symbolic", "secondary"),                     // circle.dotted
    }
}
