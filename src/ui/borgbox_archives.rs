//! Read-only archive list of a repo hosted on a BorgBox daemon (BorgMac's
//! `BorgBoxArchivesSheet`), fetched through the daemon rather than over SSH.
use super::borgbox_passphrase::title_block;
use super::kit::{self, icons};
use super::state::AppState;
use crate::core::borgbox::{self, BorgBoxRemoteArchive};
use crate::core::format;
use crate::core::models::BorgBoxServer;
use adw::prelude::*;
use gtk::{gio, glib};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

struct Sheet {
    server: BorgBoxServer,
    repo: String,
    passphrase: Option<String>,
    archives: RefCell<Vec<BorgBoxRemoteArchive>>,
    loading: Cell<bool>,
    error: RefCell<Option<String>>,
    store: gio::ListStore,
    stack: gtk::Stack,
    error_text: gtk::Label,
    count: gtk::Label,
    search: gtk::SearchEntry,
    refresh: gtk::Button,
}

fn column(
    title: &str,
    classes: &'static [&'static str],
    text: impl Fn(&BorgBoxRemoteArchive) -> String + 'static,
) -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_, obj| {
        if let Some(cell) = obj.downcast_ref::<gtk::ColumnViewCell>() {
            cell.set_child(Some(&kit::ellipsized("", false, classes)));
        }
    });
    factory.connect_bind(move |_, obj| {
        let Some(cell) = obj.downcast_ref::<gtk::ColumnViewCell>() else { return };
        let (Some(label), Some(archive)) = (
            cell.child().and_then(|w| w.downcast::<gtk::Label>().ok()),
            cell.item().and_then(|o| kit::unbox::<BorgBoxRemoteArchive>(&o)),
        ) else {
            return;
        };
        let value = text(&archive);
        label.set_text(&value);
        label.set_tooltip_text(Some(&value));
    });
    let col = gtk::ColumnViewColumn::new(Some(title), Some(factory));
    col.set_resizable(true);
    col
}

/// Accepts RFC 3339 (with or without fractional seconds) and the naive
/// "yyyy-MM-ddTHH:mm:ss.SSSSSS" borg sometimes emits without a time zone;
/// anything else is shown raw.
fn format_date(raw: Option<&str>) -> String {
    let Some(raw) = raw else { return "—".to_string() };
    if let Some(date) = format::parse_rfc3339(raw) {
        return format::medium_date_short_time(&date);
    }
    match format::parse_borg_date(raw) {
        Some(date) => format::medium_date_short_time(&date),
        None => raw.to_string(),
    }
}

fn centered_state(icon: &gtk::Image, title: &gtk::Label, detail: &gtk::Label) -> gtk::Box {
    let b = kit::vbox(8);
    b.set_valign(gtk::Align::Center);
    b.set_halign(gtk::Align::Center);
    b.set_vexpand(true);
    b.set_margin_start(20);
    b.set_margin_end(20);
    for l in [title, detail] {
        l.set_xalign(0.5);
        l.set_justify(gtk::Justification::Center);
    }
    b.append(icon);
    b.append(title);
    b.append(detail);
    b
}

pub fn present(
    parent: &impl IsA<gtk::Widget>,
    _app: &Rc<AppState>,
    server: &BorgBoxServer,
    repo: &str,
    passphrase: Option<String>,
) {
    let (dialog, content) = kit::bare_sheet("Archives", 880, 560);
    content.add_css_class("bb-panel");

    // ---------------------------------------------------------------- header
    let header = title_block(icons::ARCHIVE, "Archives", repo);
    header.set_margin_top(14);
    header.set_margin_bottom(14);
    header.set_margin_start(20);
    header.set_margin_end(20);
    header.append(&kit::spacer());
    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("Search…"));
    search.set_width_request(200);
    search.set_valign(gtk::Align::Center);
    header.append(&search);
    let refresh = kit::icon_button(icons::REFRESH, "Refresh");
    refresh.set_valign(gtk::Align::Center);
    header.append(&refresh);
    content.append(&header);
    content.append(&kit::divider());

    // ----------------------------------------------------------------- body
    let stack = gtk::Stack::new();
    stack.set_vexpand(true);

    let loading = kit::vbox(10);
    loading.set_valign(gtk::Align::Center);
    loading.set_halign(gtk::Align::Center);
    let spinner = kit::spinner();
    spinner.set_halign(gtk::Align::Center);
    loading.append(&spinner);
    loading.append(&kit::label("Loading archives…", &["secondary"]));
    stack.add_named(&loading, Some("loading"));

    let error_text = kit::caption("");
    error_text.set_selectable(true);
    error_text.set_focusable(false);
    stack.add_named(
        &centered_state(
            &kit::icon_styled(icons::WARNING, &["c-red", "bb-state-icon"]),
            &kit::label("Error loading archives", &["heading"]),
            &error_text,
        ),
        Some("error"),
    );
    stack.add_named(
        &centered_state(
            &kit::icon_styled(icons::TRAY, &["secondary", "bb-state-icon"]),
            &kit::label("No archives", &["heading", "secondary"]),
            &kit::caption("This repo doesn't have any backups yet."),
        ),
        Some("empty"),
    );

    let store = gio::ListStore::new::<glib::BoxedAnyObject>();
    let selection = gtk::SingleSelection::new(Some(store.clone()));
    selection.set_autoselect(false);
    selection.set_can_unselect(true);
    let table = gtk::ColumnView::new(Some(selection));
    table.add_css_class("mac-table");
    table.set_show_row_separators(false);
    let name_col = column("Name", &["mono"], |a| a.name.clone());
    name_col.set_expand(true);
    table.append_column(&name_col);
    let date_col = column("Date", &[], |a| format_date(a.time.as_deref()));
    date_col.set_fixed_width(180);
    table.append_column(&date_col);
    let host_col =
        column("Host", &["caption", "secondary"], |a| a.hostname.clone().unwrap_or_else(|| "—".to_string()));
    host_col.set_fixed_width(120);
    table.append_column(&host_col);
    let user_col =
        column("User", &["caption", "secondary"], |a| a.username.clone().unwrap_or_else(|| "—".to_string()));
    user_col.set_fixed_width(100);
    table.append_column(&user_col);
    let comment_col = column("Comment", &["caption", "secondary"], |a| {
        a.comment.clone().filter(|c| !c.is_empty()).unwrap_or_else(|| "—".to_string())
    });
    comment_col.set_fixed_width(190);
    table.append_column(&comment_col);
    stack.add_named(&kit::scrolled(&table), Some("table"));
    content.append(&stack);
    content.append(&kit::divider());

    // ---------------------------------------------------------------- footer
    let count = kit::label("", &["caption", "secondary"]);
    let close = kit::button("Close");
    let footer = kit::button_row(Some(count.upcast_ref()), &[&close]);
    footer.set_margin_top(12);
    footer.set_margin_bottom(12);
    footer.set_margin_start(20);
    footer.set_margin_end(20);
    content.append(&footer);

    let sheet = Rc::new(Sheet {
        server: server.clone(),
        repo: repo.to_string(),
        passphrase,
        archives: RefCell::new(vec![]),
        loading: Cell::new(false),
        error: RefCell::new(None),
        store,
        stack,
        error_text,
        count,
        search: search.clone(),
        refresh: refresh.clone(),
    });

    close.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| {
            dialog.close();
        }
    ));
    search.connect_search_changed(glib::clone!(
        #[weak]
        sheet,
        move |_| render(&sheet)
    ));
    refresh.connect_clicked(glib::clone!(
        #[weak]
        sheet,
        move |_| load(&sheet)
    ));

    // The sheet state lives as long as the dialog is open.
    let keep = RefCell::new(Some(sheet.clone()));
    dialog.connect_closed(move |_| {
        keep.borrow_mut().take();
    });

    load(&sheet);
    dialog.present(Some(parent));
}

fn filtered(sheet: &Sheet) -> Vec<BorgBoxRemoteArchive> {
    let query = sheet.search.text().trim().to_lowercase();
    let archives = sheet.archives.borrow();
    if query.is_empty() {
        return archives.clone();
    }
    let matches = |field: &Option<String>| field.as_ref().is_some_and(|v| v.to_lowercase().contains(&query));
    archives
        .iter()
        .filter(|a| a.name.to_lowercase().contains(&query) || matches(&a.hostname) || matches(&a.username))
        .cloned()
        .collect()
}

fn render(sheet: &Sheet) {
    let rows = filtered(sheet);
    sheet.store.remove_all();
    for archive in &rows {
        sheet.store.append(&kit::boxed(archive.clone()));
    }
    sheet.count.set_text(&format!("{} archive{}", rows.len(), if rows.len() == 1 { "" } else { "s" }));
    sheet.refresh.set_sensitive(!sheet.loading.get());

    let empty = sheet.archives.borrow().is_empty();
    let error = sheet.error.borrow().clone();
    let page = match (empty, sheet.loading.get(), &error) {
        (true, true, _) => "loading",
        (true, false, Some(message)) => {
            sheet.error_text.set_text(message);
            "error"
        }
        (true, false, None) => "empty",
        (false, ..) => "table",
    };
    sheet.stack.set_visible_child_name(page);
}

fn load(sheet: &Rc<Sheet>) {
    sheet.loading.set(true);
    *sheet.error.borrow_mut() = None;
    render(sheet);
    let (server, repo, passphrase) = (sheet.server.clone(), sheet.repo.clone(), sheet.passphrase.clone());
    let weak = Rc::downgrade(sheet);
    kit::bg(
        move || borgbox::remote_archives(&server, &repo, passphrase.as_deref()),
        move |result| {
            let Some(sheet) = weak.upgrade() else { return };
            sheet.loading.set(false);
            match result {
                Ok(mut archives) => {
                    archives.sort_by(|a, b| {
                        b.time.as_deref().unwrap_or("").cmp(a.time.as_deref().unwrap_or(""))
                    });
                    *sheet.archives.borrow_mut() = archives;
                }
                Err(e) if e.starts_with("BorgBox returned HTTP 404:") => {
                    *sheet.error.borrow_mut() = Some(format!(
                        "The daemon doesn't implement /repos/{}/archives yet, or the repo isn't initialized.",
                        sheet.repo
                    ));
                }
                Err(e) => *sheet.error.borrow_mut() = Some(e),
            }
            render(&sheet);
        },
    );
}
