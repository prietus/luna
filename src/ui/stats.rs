//! Point-in-time overview of every repo the app knows about, aggregated by
//! provider (local/Borg, BorgBase, BorgBox). No history — everything is
//! fetched fresh every time the panel opens. (StatsPanel.swift)
use super::kit::{self, Tint, icons};
use super::state::{AppState, Signal, connect_while_alive};
use crate::core::borgbase::BorgBaseRepo;
use crate::core::borgbox::BorgBoxRemoteRepo;
use crate::core::models::*;
use crate::core::{borg, borgbase, borgbox, format, snapshot};
use adw::prelude::*;
use gtk::glib;
use std::cell::{Cell, RefCell};
use std::f64::consts::PI;
use std::rc::Rc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Provider {
    Local,
    BorgBase,
    BorgBox,
}

impl Provider {
    const ALL: [Provider; 3] = [Provider::Local, Provider::BorgBase, Provider::BorgBox];

    /// Stable key shared with the status-bar snapshot.
    fn key(self) -> &'static str {
        match self {
            Provider::Local => "local",
            Provider::BorgBase => "borgBase",
            Provider::BorgBox => "borgBox",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Provider::Local => "Local/SSH",
            Provider::BorgBase => "BorgBase",
            Provider::BorgBox => "BorgBox",
        }
    }

    fn tint(self) -> Tint {
        match self {
            Provider::Local => Tint::Blue,
            Provider::BorgBase => Tint::Green,
            Provider::BorgBox => Tint::Yellow,
        }
    }
}

#[derive(Debug, Clone)]
struct StatsEntry {
    name: String,
    provider: Provider,
    /// Total bytes across all archives before compression (only for local).
    original_bytes: i64,
    /// Compressed bytes (only for local).
    compressed_bytes: i64,
    /// Bytes actually used on disk after dedup — for local it's
    /// `unique_csize` from `borg info`; for BorgBase the reported
    /// `currentUsage`; for BorgBox the server-computed `size_bytes`.
    effective_bytes: i64,
    archive_count: usize,
}

struct Panel {
    app: Rc<AppState>,
    body: gtk::Box,
    refresh: gtk::Button,
    loading: Cell<bool>,
    /// Everything we gather and chart. Built fresh from all three sources on
    /// each `load()` call.
    entries: RefCell<Vec<StatsEntry>>,
}

pub fn present(parent: &impl IsA<gtk::Widget>, app: &Rc<AppState>) {
    let (dialog, content) = kit::bare_sheet("Statistics", 760, 740);

    let refresh = kit::icon_button(icons::REFRESH, "Refresh");
    let close = kit::button("Close");
    close.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| {
            dialog.close();
        }
    ));
    content.append(&kit::panel_header(
        icons::CHART,
        "Statistics",
        "Point-in-time overview of your repositories",
        &[refresh.upcast_ref(), close.upcast_ref()],
    ));
    content.append(&kit::divider());

    let body = kit::vbox(16);
    body.set_margin_top(20);
    body.set_margin_bottom(20);
    body.set_margin_start(20);
    body.set_margin_end(20);
    content.append(&kit::scrolled(&body));

    let panel = Rc::new(Panel {
        app: app.clone(),
        body,
        refresh: refresh.clone(),
        loading: Cell::new(false),
        entries: RefCell::new(vec![]),
    });

    refresh.connect_clicked(glib::clone!(
        #[weak]
        panel,
        move |_| load(&panel)
    ));
    // The "Scheduled backups" tiles follow the stores live.
    let signals: [fn(&AppState) -> &Signal; 2] = [|a| &a.statuses.changed, |a| &a.repos.changed];
    for pick in signals {
        connect_while_alive(
            app,
            pick,
            &dialog,
            glib::clone!(
                #[weak]
                panel,
                move || render(&panel)
            ),
        );
    }

    load(&panel);
    // The dialog owns the panel state; every handler refers to it weakly.
    dialog.connect_destroy(move |_| {
        let _ = &panel;
    });
    dialog.present(Some(parent));
}

// -------------------------------------------------------------------- render

fn render(p: &Rc<Panel>) {
    kit::clear_box(&p.body);
    let entries = p.entries.borrow();

    if p.loading.get() && entries.is_empty() {
        let b = kit::vbox(8);
        b.set_halign(gtk::Align::Center);
        b.set_margin_top(40);
        b.set_margin_bottom(40);
        let spinner = kit::spinner();
        spinner.set_halign(gtk::Align::Center);
        b.append(&spinner);
        b.append(&kit::label("Loading statistics…", &["caption", "secondary"]));
        p.body.append(&b);
    }

    p.body.append(&summary_card(&entries));
    p.body.append(&group("By provider", &by_provider_card(&entries), true));
    p.body.append(&group("Size by repository", &by_repo_card(&entries), true));
    p.body.append(&group("Compression & deduplication (local repos)", &ratios_card(&entries), true));
    p.body.append(&group("Scheduled backups", &scheduled_card(&p.app), false));
}

/// SwiftUI `GroupBox(title)`: a heading above a rounded block.
fn group(title: &str, child: &impl IsA<gtk::Widget>, fill: bool) -> gtk::Box {
    let b = kit::vbox(6);
    let heading = kit::label(title, &["section-title"]);
    heading.set_margin_start(8);
    b.append(&heading);
    let card = kit::card(child);
    if !fill {
        card.set_halign(gtk::Align::Start);
    }
    b.append(&card);
    b
}

fn metric(label: &str, value: &str, icon_name: &str) -> gtk::Box {
    let row = kit::hbox(10);
    let icon = kit::icon_styled(icon_name, &["tint", "stats-metric-icon"]);
    icon.set_size_request(28, -1);
    row.append(&icon);
    let texts = kit::vbox(2);
    texts.append(&kit::label(value, &["stats-value"]));
    texts.append(&kit::label(label, &["caption", "secondary"]));
    row.append(&texts);
    row
}

fn no_data() -> gtk::Label {
    kit::label("No data.", &["caption", "secondary"])
}

fn summary_card(entries: &[StatsEntry]) -> gtk::Box {
    let row = kit::hbox(24);
    row.set_margin_top(4);
    row.set_margin_bottom(4);
    let archives: usize = entries.iter().map(|e| e.archive_count).sum();
    let usage: i64 = entries.iter().map(|e| e.effective_bytes).sum();
    row.append(&metric("Repos", &entries.len().to_string(), icons::DRIVE));
    row.append(&metric("Archives", &archives.to_string(), icons::ARCHIVE));
    row.append(&metric("Total usage", &format::bytes_binary(usage), icons::CHART));
    kit::card(&row)
}

fn dot(tint: Tint, size: i32) -> gtk::Box {
    let d = kit::hbox(0);
    d.add_css_class("stats-dot");
    d.add_css_class(&format!("d-{}", tint.name()));
    d.set_size_request(size, size);
    d.set_valign(gtk::Align::Center);
    d.set_halign(gtk::Align::Center);
    d
}

fn provider_slices(entries: &[StatsEntry]) -> Vec<(Provider, i64)> {
    Provider::ALL
        .into_iter()
        .filter(|p| entries.iter().any(|e| e.provider == *p))
        .map(|p| (p, entries.iter().filter(|e| e.provider == p).map(|e| e.effective_bytes).sum()))
        .collect()
}

fn by_provider_card(entries: &[StatsEntry]) -> gtk::Widget {
    if entries.is_empty() {
        return no_data().upcast();
    }
    let slices = provider_slices(entries);
    let row = kit::hbox(24);
    row.set_margin_top(4);
    row.set_margin_bottom(4);

    let donut = gtk::DrawingArea::new();
    donut.set_content_width(220);
    donut.set_content_height(180);
    donut.set_valign(gtk::Align::Center);
    let data = slices.clone();
    donut.set_draw_func(move |_, cr, w, h| draw_donut(cr, w as f64, h as f64, &data));
    row.append(&donut);

    let legend = kit::vbox(8);
    legend.set_hexpand(true);
    legend.set_valign(gtk::Align::Center);
    for (provider, bytes) in &slices {
        let line = kit::hbox(8);
        line.append(&dot(provider.tint(), 10));
        line.append(&kit::label(provider.label(), &["stats-medium"]));
        line.append(&kit::spacer());
        line.append(&kit::label(&format::bytes_binary(*bytes), &["caption", "secondary", "stats-digits"]));
        legend.append(&line);
    }
    row.append(&legend);
    row.upcast()
}

/// `SectorMark(innerRadius: .ratio(0.55), angularInset: 1)`: a ring starting
/// at 12 o'clock, clockwise, with a 1px gap between neighbouring sectors.
fn draw_donut(cr: &gtk::cairo::Context, w: f64, h: f64, slices: &[(Provider, i64)]) {
    let total: f64 = slices.iter().map(|(_, b)| (*b).max(0) as f64).sum();
    let outer = w.min(h) / 2.0;
    let inner = outer * 0.55;
    let (cx, cy) = (w / 2.0, h / 2.0);
    let visible: Vec<(Provider, f64)> = if total > 0.0 {
        slices.iter().filter(|(_, b)| *b > 0).map(|(p, b)| (*p, *b as f64 / total)).collect()
    } else {
        // Nothing has a size yet: split the ring evenly so it isn't blank.
        slices.iter().map(|(p, _)| (*p, 1.0 / slices.len() as f64)).collect()
    };
    let inset = if visible.len() > 1 { 1.0 } else { 0.0 };
    let mut angle = -PI / 2.0;
    for (provider, share) in visible {
        let sweep = share * 2.0 * PI;
        let (a0, a1) = (angle, angle + sweep);
        angle = a1;
        // The gap is a constant width, so the angular trim grows towards
        // the centre.
        let (trim_out, trim_in) = (inset / 2.0 / outer, inset / 2.0 / inner);
        if sweep <= 2.0 * trim_in {
            continue;
        }
        let (r, g, b) = provider.tint().rgb();
        cr.set_source_rgb(r, g, b);
        cr.new_path();
        cr.arc(cx, cy, outer, a0 + trim_out, a1 - trim_out);
        cr.arc_negative(cx, cy, inner, a1 - trim_in, a0 + trim_in);
        cr.close_path();
        let _ = cr.fill();
    }
}

/// Axis step for roughly `desired` tick marks: the smallest of
/// {1, 2, 2.5, 5, 10}·10ⁿ that covers `max` in `desired - 1` intervals.
fn nice_step(max: f64, desired: usize) -> f64 {
    if max <= 0.0 {
        return 1.0;
    }
    let raw = max / (desired.max(2) - 1) as f64;
    let magnitude = 10f64.powf(raw.log10().floor());
    for m in [1.0, 2.0, 2.5, 5.0, 10.0] {
        if m * magnitude >= raw {
            return m * magnitude;
        }
    }
    10.0 * magnitude
}

const BAR_ROW_HEIGHT: i32 = 28;

fn by_repo_card(entries: &[StatsEntry]) -> gtk::Widget {
    if entries.is_empty() {
        return no_data().upcast();
    }
    let mut top: Vec<StatsEntry> = entries.to_vec();
    top.sort_by(|a, b| b.effective_bytes.cmp(&a.effective_bytes));
    top.truncate(10);

    let max = top.first().map(|e| e.effective_bytes.max(0) as f64).unwrap_or(0.0);
    let step = nice_step(max, 4);
    let intervals = ((max / step).ceil() as usize).max(1);
    let domain = step * intervals as f64;

    let chart = kit::vbox(0);
    chart.set_margin_top(4);
    chart.set_margin_bottom(4);

    // Bars and grid are drawn; the texts are real labels laid over the plot
    // so they follow the theme font.
    let plot = gtk::DrawingArea::new();
    plot.set_content_height(top.len() as i32 * BAR_ROW_HEIGHT);
    plot.set_hexpand(true);
    let bars: Vec<(f64, Tint)> =
        top.iter().map(|e| (e.effective_bytes.max(0) as f64 / domain, e.provider.tint())).collect();
    plot.set_draw_func(move |area, cr, w, h| {
        let fg = area.color();
        draw_bars(cr, w as f64, h as f64, &bars, intervals, (fg.red() as f64, fg.green() as f64, fg.blue() as f64));
    });
    let names = kit::vbox(0);
    names.set_valign(gtk::Align::Start);
    for e in &top {
        let l = kit::ellipsized(&e.name, true, &["caption", "secondary"]);
        l.set_size_request(-1, BAR_ROW_HEIGHT);
        l.set_yalign(0.2);
        l.set_margin_start(5);
        names.append(&l);
    }
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&plot));
    overlay.add_overlay(&names);
    chart.append(&overlay);

    // Ticks are evenly spaced, so the axis labels are equal-width cells each
    // starting at its tick (the last tick sits on the right edge, unlabeled).
    let axis = kit::hbox(0);
    axis.set_homogeneous(true);
    axis.set_margin_top(4);
    for i in 0..intervals {
        let l = kit::ellipsized(&format::bytes_binary((step * i as f64) as i64), false, &["caption2", "secondary"]);
        l.set_margin_start(5);
        axis.append(&l);
    }
    chart.append(&axis);

    let legend = kit::hbox(10);
    legend.set_margin_top(6);
    for provider in Provider::ALL {
        let item = kit::hbox(5);
        item.append(&dot(provider.tint(), 8));
        item.append(&kit::label(provider.label(), &["caption", "secondary"]));
        legend.append(&item);
    }
    chart.append(&legend);
    chart.upcast()
}

fn draw_bars(cr: &gtk::cairo::Context, w: f64, h: f64, bars: &[(f64, Tint)], intervals: usize, fg: (f64, f64, f64)) {
    let row = BAR_ROW_HEIGHT as f64;
    cr.set_line_width(1.0);

    // Horizontal rules between the rows.
    cr.set_source_rgba(fg.0, fg.1, fg.2, 0.12);
    for i in 0..=bars.len() {
        let y = (i as f64 * row).min(h - 1.0).floor() + 0.5;
        cr.move_to(0.0, y);
        cr.line_to(w, y);
    }
    let _ = cr.stroke();

    // Dashed vertical grid line at every tick.
    cr.set_source_rgba(fg.0, fg.1, fg.2, 0.16);
    cr.set_dash(&[2.0, 2.0], 0.0);
    for i in 0..=intervals {
        let x = (w * i as f64 / intervals as f64).min(w - 1.0).floor() + 0.5;
        cr.move_to(x, 0.0);
        cr.line_to(x, h);
    }
    let _ = cr.stroke();
    cr.set_dash(&[], 0.0);

    let thickness = 6.0;
    for (i, (ratio, tint)) in bars.iter().enumerate() {
        if *ratio <= 0.0 {
            continue;
        }
        // Tiny repos still get a visible sliver.
        let len = (w * ratio).max(2.0);
        let y = (i + 1) as f64 * row - thickness - 2.0;
        let radius = (thickness / 2.0).min(len / 2.0).min(2.5);
        let (r, g, b) = tint.rgb();
        cr.set_source_rgb(r, g, b);
        cr.new_path();
        cr.move_to(0.0, y);
        cr.line_to(len - radius, y);
        cr.arc(len - radius, y + radius, radius, -PI / 2.0, 0.0);
        cr.line_to(len, y + thickness - radius);
        cr.arc(len - radius, y + thickness - radius, radius, 0.0, PI / 2.0);
        cr.line_to(0.0, y + thickness);
        cr.close_path();
        let _ = cr.fill();
    }
}

fn percent(ratio: f64) -> String {
    format!("{:.0}%", ratio * 100.0)
}

fn ratios_card(entries: &[StatsEntry]) -> gtk::Widget {
    let b = kit::vbox(6);
    b.set_margin_top(4);
    b.set_margin_bottom(4);
    let locals: Vec<&StatsEntry> =
        entries.iter().filter(|e| e.provider == Provider::Local && e.original_bytes > 0).collect();
    if locals.is_empty() {
        b.append(&kit::caption(
            "Only available for direct local/SSH repos. BorgBase and BorgBox repos don't expose detailed stats.",
        ));
    }
    for entry in locals {
        b.append(&ratio_row(entry));
    }
    b.upcast()
}

fn ratio_row(entry: &StatsEntry) -> gtk::Box {
    let original = entry.original_bytes as f64;
    let comp_ratio = if original > 0.0 { entry.compressed_bytes as f64 / original } else { 0.0 };
    let dedup_ratio = if original > 0.0 { entry.effective_bytes as f64 / original } else { 0.0 };

    let row = kit::hbox(10);
    let icon = kit::icon_styled(icons::DRIVE, &["tint"]);
    icon.set_size_request(20, -1);
    row.append(&icon);
    let texts = kit::vbox(2);
    texts.append(&kit::label(&entry.name, &["stats-medium"]));
    let numbers = kit::hbox(14);
    let classes = ["caption", "secondary", "stats-digits"];
    numbers.append(&kit::label(&format!("orig {}", format::bytes_binary(entry.original_bytes)), &classes));
    numbers.append(&kit::label(
        &format!("comp {} ({})", format::bytes_binary(entry.compressed_bytes), percent(comp_ratio)),
        &classes,
    ));
    numbers.append(&kit::label(
        &format!("dedup {} ({})", format::bytes_binary(entry.effective_bytes), percent(dedup_ratio)),
        &["caption", "tint", "stats-digits"],
    ));
    texts.append(&numbers);
    row.append(&texts);
    row
}

fn scheduled_card(app: &Rc<AppState>) -> gtk::Widget {
    let (mut ok, mut running, mut failed, mut total) = (0, 0, 0, 0);
    let statuses = app.statuses.statuses();
    for repo in app.repos.repositories() {
        for schedule in &repo.schedules {
            total += 1;
            let status = statuses.get(&schedule.id);
            if status.map(|s| s.running).unwrap_or(false) {
                running += 1;
            } else if status.and_then(|s| s.last_error.as_ref()).is_some() {
                failed += 1;
            } else if status.and_then(|s| s.last_success).is_some() {
                ok += 1;
            }
        }
    }
    if total == 0 {
        return kit::label("You don't have any scheduled backups configured.", &["caption", "secondary"]).upcast();
    }
    let row = kit::hbox(24);
    row.set_margin_top(4);
    row.set_margin_bottom(4);
    row.append(&metric("Scheduled", &total.to_string(), icons::HISTORY));
    row.append(&metric("Last OK", &ok.to_string(), icons::CHECK_CIRCLE));
    row.append(&metric("Running", &running.to_string(), icons::SYNC));
    row.append(&metric("With error", &failed.to_string(), icons::WARNING));
    row.upcast()
}

// ---------------------------------------------------------------------- data

fn load(p: &Rc<Panel>) {
    if p.loading.get() {
        return;
    }
    p.loading.set(true);
    p.refresh.set_sensitive(false);
    render(p);

    let repositories = p.app.repos.repositories();
    let servers = p.app.borgbox.servers();
    let weak = Rc::downgrade(p);
    kit::bg(
        move || gather(&repositories, &servers),
        move |entries| {
            let Some(p) = weak.upgrade() else { return };
            p.loading.set(false);
            p.refresh.set_sensitive(true);
            *p.entries.borrow_mut() = entries;
            render(&p);
            publish_snapshot(&p);
        },
    );
}

fn gather(repositories: &[Repository], servers: &[BorgBoxServer]) -> Vec<StatsEntry> {
    // Gather each source into its own bucket first so we can dedupe
    // a single physical repo that's reachable through more than one
    // channel (e.g. a BorgBox repo the user also added as a local SSH
    // entry). The local entry wins because it carries the richer
    // orig/comp/dedup stats from `borg info`.
    let mut borgbase_repos: Vec<BorgBaseRepo> = vec![];
    let mut borgbox_repos: Vec<(BorgBoxServer, BorgBoxRemoteRepo)> = vec![];
    let mut locals: Vec<(&Repository, BorgRepoInfo, usize)> = vec![];

    if borgbase::has_token() {
        match borgbase::repos() {
            Ok(repos) => borgbase_repos = repos,
            Err(e) => eprintln!("[StatsPanel] BorgBase repos failed: {e}"),
        }
    }

    for server in servers {
        match borgbox::list_repos(server) {
            Ok(repos) => {
                borgbox_repos.extend(repos.into_iter().filter(|r| r.is_registered()).map(|r| (server.clone(), r)))
            }
            Err(e) => eprintln!("[StatsPanel] BorgBox listRepos failed: {e}"),
        }
    }

    // Local repos — `borg info --json` for the ratios, plus
    // `borg list --json` for a real archive count (info alone
    // returns an empty archives array). Serialised so we don't
    // spawn N borg processes at once.
    for local in repositories {
        match borg::info(local) {
            Ok(info) => {
                let archives = borg::list_archives(local).map(|a| a.len()).unwrap_or(0);
                locals.push((local, info, archives));
            }
            Err(e) => eprintln!("[StatsPanel] borg info {} failed: {e}", local.name),
        }
    }

    // Dedup pass: mark BorgBox / BorgBase remotes that are "claimed"
    // by a local entry so we don't emit them a second time. The
    // local entry inherits the remote's provider label so the pie
    // and bar chart credit it to the right hosting provider.
    let mut consumed_borgbox: Vec<(uuid::Uuid, String)> = vec![];
    let mut consumed_borgbase: Vec<String> = vec![];
    let mut out: Vec<StatsEntry> = vec![];

    for (local, info, archives) in locals {
        let mut provider = Provider::Local;
        if let Some((server, repo)) = borgbox_repos.iter().find(|(s, r)| borgbox_matches(s, r, &local.url)) {
            provider = Provider::BorgBox;
            consumed_borgbox.push((server.id, repo.name.clone()));
        } else if let Some(repo) = borgbase_repos.iter().find(|r| borgbase_matches(r, &local.url)) {
            provider = Provider::BorgBase;
            consumed_borgbase.push(repo.id.clone());
        } else if looks_like_borgbase_url(&local.url) {
            // Fallback: no API match (usually because no token is saved,
            // so borgbase_repos is empty), but the URL points at a
            // borgbase.com host — still classify it as BorgBase so the
            // pie chart doesn't miscredit it to Local/SSH.
            provider = Provider::BorgBase;
        }
        out.push(StatsEntry {
            name: local.name.clone(),
            provider,
            original_bytes: info.total_size,
            compressed_bytes: info.total_csize,
            effective_bytes: info.unique_csize,
            archive_count: archives,
        });
    }

    for (server, r) in &borgbox_repos {
        if consumed_borgbox.iter().any(|(id, name)| *id == server.id && *name == r.name) {
            continue;
        }
        out.push(StatsEntry {
            name: r.name.clone(),
            provider: Provider::BorgBox,
            original_bytes: 0,
            compressed_bytes: 0,
            effective_bytes: r.size_bytes.unwrap_or(0),
            archive_count: 0,
        });
    }

    for r in &borgbase_repos {
        if consumed_borgbase.contains(&r.id) {
            continue;
        }
        // BorgBase's GraphQL returns `currentUsage` in **megabytes**
        // (decimal, float). It's easy to mistake for GB because the
        // web dashboard shows "Current Usage: X GB", but the raw API
        // field is one scale lower. Multiplying by 10⁶ gives bytes.
        let used = (r.current_usage.unwrap_or(0.0) * 1_000_000.0) as i64;
        out.push(StatsEntry {
            name: r.name.clone(),
            provider: Provider::BorgBase,
            original_bytes: 0,
            compressed_bytes: 0,
            effective_bytes: used,
            archive_count: 0,
        });
    }
    out
}

/// Writes the status-bar snapshot using the numbers we just computed in
/// `load()`. Doing it here rather than inside the writer avoids a second
/// `borg info` sweep and keeps the bar module and the stats panel trivially
/// in sync.
fn publish_snapshot(p: &Rc<Panel>) {
    let entries = p.entries.borrow();
    let mut by_provider: Vec<snapshot::ProviderUsage> = provider_slices(&entries)
        .into_iter()
        .map(|(provider, bytes)| snapshot::ProviderUsage {
            provider_key: provider.key().to_string(),
            label: provider.label().to_string(),
            bytes,
        })
        .collect();
    by_provider.sort_by(|a, b| b.bytes.cmp(&a.bytes));

    // Top 5 by deduped size so the bar module shows the same ranking as
    // the "Size by repository" bar chart. Cap it here (not in the writer)
    // so the JSON stays small.
    let mut sorted: Vec<&StatsEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| b.effective_bytes.cmp(&a.effective_bytes));
    let top_repos = sorted
        .into_iter()
        .take(5)
        .map(|e| snapshot::RepoUsage {
            name: e.name.clone(),
            provider_key: e.provider.key().to_string(),
            bytes: e.effective_bytes,
        })
        .collect();

    snapshot::write_full(
        entries.len(),
        entries.iter().map(|e| e.archive_count).sum(),
        entries.iter().map(|e| e.effective_bytes).sum(),
        by_provider,
        top_repos,
        &p.app.repos.repositories(),
        &p.app.statuses.statuses(),
    );
}

// -------------------------------------------------------------- dedup helpers

/// True when `local_url` points at the same physical repo as the given
/// BorgBox remote. Prefers the daemon-reported `ssh_url` (v0.3.0+) and falls
/// back to a URL built from the server's ssh fields + the repo's absolute
/// path.
fn borgbox_matches(server: &BorgBoxServer, repo: &BorgBoxRemoteRepo, local_url: &str) -> bool {
    let canon_local = canonicalize_ssh_url(local_url);
    if let Some(ssh_url) = repo.ssh_url.as_ref().filter(|u| !u.is_empty()) {
        if canonicalize_ssh_url(ssh_url) == canon_local {
            return true;
        }
    }
    let constructed = format!("ssh://{}@{}:{}{}", server.ssh_user, server.ssh_host, server.ssh_port, repo.path);
    canonicalize_ssh_url(&constructed) == canon_local
}

/// BorgBase stores the exact `repoPath` returned by the API as the local
/// `Repository.url` when the user creates a repo through the in-app wizard,
/// so a direct canonical comparison catches it.
fn borgbase_matches(repo: &BorgBaseRepo, local_url: &str) -> bool {
    match repo.repo_path.as_ref().filter(|p| !p.is_empty()) {
        Some(path) => canonicalize_ssh_url(path) == canonicalize_ssh_url(local_url),
        None => false,
    }
}

/// True when the URL host belongs to the BorgBase SaaS. All BorgBase repos
/// live under `*.repo.borgbase.com`, so a substring check on the lowercased
/// URL is enough to recognise them without needing an API token.
fn looks_like_borgbase_url(url: &str) -> bool {
    url.to_lowercase().contains("borgbase.com")
}

/// Normalises an ssh://user@host:port/path URL for comparison: lowercases,
/// trims a trailing slash, and drops an explicit `:22` so two URLs that
/// differ only in whether the default port was spelled out still match.
fn canonicalize_ssh_url(url: &str) -> String {
    let mut s = url.trim().to_lowercase();
    if s.ends_with('/') {
        s.pop();
    }
    s = s.replace(":22/", "/");
    if let Some(stripped) = s.strip_suffix(":22") {
        s = stripped.to_string();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_urls_compare_canonically() {
        assert_eq!(canonicalize_ssh_url(" ssh://U@Host:22/./repo/ "), "ssh://u@host/./repo");
        assert_eq!(canonicalize_ssh_url("ssh://u@host:22"), "ssh://u@host");
        assert_eq!(canonicalize_ssh_url("ssh://u@host:2222/x"), "ssh://u@host:2222/x");
    }

    #[test]
    fn axis_steps_are_round() {
        assert_eq!(nice_step(753e9, 4), 500e9);
        assert_eq!(nice_step(700e9, 4), 250e9);
        assert_eq!(nice_step(90.0, 4), 50.0);
        assert_eq!(nice_step(0.0, 4), 1.0);
    }
}
