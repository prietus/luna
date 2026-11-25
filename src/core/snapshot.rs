//! Overview snapshot written by the GUI and read by status bars — the Linux
//! stand-in for BorgMac's WidgetKit widget. `luna --status-json` turns
//! it into a waybar custom-module payload.
use super::models::{BackupRunStatus, Repository};
use super::{format, paths, schedule, store};
use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderUsage {
    /// Stable key — "local", "borgBase", "borgBox".
    pub provider_key: String,
    /// "Local/SSH", "BorgBase", "BorgBox".
    pub label: String,
    pub bytes: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoUsage {
    pub name: String,
    pub provider_key: String,
    pub bytes: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NextBackup {
    pub repo_name: String,
    pub schedule_name: String,
    pub fire_date: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Health {
    /// Every schedule's last run succeeded (or hasn't run yet).
    Ok,
    /// A run is currently in progress.
    Running,
    /// At least one schedule reports a failure.
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub generated_at: DateTime<Utc>,
    pub repo_count: usize,
    /// Total archives across every local repo.
    pub archive_count: usize,
    /// Deduplicated usage where available, reported usage otherwise.
    pub total_usage_bytes: i64,
    /// Largest slice first.
    pub by_provider: Vec<ProviderUsage>,
    /// Sorted by bytes descending, capped to a handful when written.
    pub top_repos: Vec<RepoUsage>,
    /// Soonest schedule across every repo; `None` without schedules.
    pub next_backup: Option<NextBackup>,
    pub health: Health,
}

impl Snapshot {
    pub fn load() -> Option<Snapshot> {
        let data = std::fs::read(paths::snapshot_file()).ok()?;
        serde_json::from_slice(&data).ok()
    }

    /// Atomic write so a concurrent reader never sees a half-written file.
    pub fn save(&self) -> Result<(), String> {
        let data = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        let target = paths::snapshot_file();
        let tmp = target.with_extension(format!("tmp{}", std::process::id()));
        std::fs::write(&tmp, data).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &target).map_err(|e| e.to_string())
    }
}

/// Finds the soonest-firing schedule and derives a health flag from the run
/// statuses.
pub fn summarize_schedules(
    repositories: &[Repository],
    statuses: &HashMap<Uuid, BackupRunStatus>,
) -> (Option<NextBackup>, Health) {
    let mut soonest: Option<NextBackup> = None;
    let (mut any_running, mut any_error) = (false, false);
    let now = Local::now();
    for repo in repositories {
        for sched in &repo.schedules {
            if let Some(status) = statuses.get(&sched.id) {
                any_running |= status.running;
                any_error |= status.last_error.is_some();
            }
            let Some(fire) = schedule::next_fire_date(sched, now) else { continue };
            let fire = fire.with_timezone(&Utc);
            if soonest.as_ref().map(|s| fire < s.fire_date).unwrap_or(true) {
                soonest = Some(NextBackup {
                    repo_name: repo.name.clone(),
                    schedule_name: sched.display_name(),
                    fire_date: fire,
                });
            }
        }
    }
    let health = if any_error {
        Health::Error
    } else if any_running {
        Health::Running
    } else {
        Health::Ok
    };
    (soonest, health)
}

/// Full snapshot, written by the stats panel once it has fresh usage numbers.
pub fn write_full(
    repo_count: usize,
    archive_count: usize,
    total_usage_bytes: i64,
    by_provider: Vec<ProviderUsage>,
    top_repos: Vec<RepoUsage>,
    repositories: &[Repository],
    statuses: &HashMap<Uuid, BackupRunStatus>,
) {
    let (next_backup, health) = summarize_schedules(repositories, statuses);
    let snapshot = Snapshot {
        generated_at: Utc::now(),
        repo_count,
        archive_count,
        total_usage_bytes,
        by_provider,
        top_repos,
        next_backup,
        health,
    };
    if let Err(e) = snapshot.save() {
        eprintln!("[snapshot] save failed: {e}");
    }
}

/// Updates only the schedule-derived fields, reusing the last persisted
/// usage numbers. Creates a usage-less snapshot when none exists yet so the
/// status bar module has something to show before the stats panel was opened.
pub fn refresh_schedules(repositories: &[Repository], statuses: &HashMap<Uuid, BackupRunStatus>) {
    let (next_backup, health) = summarize_schedules(repositories, statuses);
    let snapshot = match Snapshot::load() {
        Some(previous) => Snapshot { generated_at: Utc::now(), next_backup, health, ..previous },
        None => Snapshot {
            generated_at: Utc::now(),
            repo_count: repositories.len(),
            archive_count: 0,
            total_usage_bytes: 0,
            by_provider: vec![],
            top_repos: vec![],
            next_backup,
            health,
        },
    };
    if let Err(e) = snapshot.save() {
        eprintln!("[snapshot] save failed: {e}");
    }
}

/// Reads `repositories.json` straight from disk and refreshes the schedule
/// fields. Used after a status file changes.
pub fn refresh_schedules_from_disk(statuses: &HashMap<Uuid, BackupRunStatus>) {
    refresh_schedules(&store::load_repositories(), statuses);
}

/// Structured overview for desktop widgets (`luna --overview-json`): the
/// usage numbers of the last snapshot plus the state of every schedule.
/// Computed live from the repository list and the status files, like
/// `waybar_json`, so it is correct even when the GUI isn't running.
pub fn overview_json() -> String {
    let repos = store::load_repositories();
    let statuses = super::status::read_all();
    let (next, health) = summarize_schedules(&repos, &statuses);
    let previous = Snapshot::load();
    let now = Local::now();

    let mut schedules = vec![];
    for repo in &repos {
        for sched in &repo.schedules {
            let st = statuses.get(&sched.id);
            let live = st.filter(|s| s.running);
            schedules.push(serde_json::json!({
                "repoName": repo.name,
                "scheduleName": sched.display_name(),
                "running": live.is_some(),
                // Fraction of the previous run's size, when there was one.
                "progress": live.and_then(|s| s.progress_ratio()),
                "progressBytes": live.and_then(|s| s.progress_original_bytes),
                "lastRun": st.and_then(|s| s.last_run),
                "lastSuccess": st.and_then(|s| s.last_success),
                "lastError": st.and_then(|s| s.last_error.clone()),
                "nextFire": schedule::next_fire_date(sched, now).map(|d| d.with_timezone(&Utc)),
            }));
        }
    }

    serde_json::json!({
        "version": 1,
        "health": health,
        "repoCount": repos.len(),
        "archiveCount": previous.as_ref().map(|p| p.archive_count).unwrap_or(0),
        "totalUsageBytes": previous.as_ref().map(|p| p.total_usage_bytes).unwrap_or(0),
        "byProvider": previous.as_ref().map(|p| p.by_provider.clone()).unwrap_or_default(),
        "topRepos": previous.as_ref().map(|p| p.top_repos.clone()).unwrap_or_default(),
        "nextBackup": next,
        "schedules": schedules,
    })
    .to_string()
}

/// Payload for a waybar `custom` module (`return-type: json`): always
/// computed live from the repository list and the status files so it is
/// correct even when the GUI isn't running; usage numbers come from the
/// last snapshot the stats panel wrote.
pub fn waybar_json() -> String {
    let repos = store::load_repositories();
    let statuses = super::status::read_all();
    let (next, health) = summarize_schedules(&repos, &statuses);
    let previous = Snapshot::load();

    let mut running: Vec<String> = vec![];
    let mut failed: Vec<String> = vec![];
    for repo in &repos {
        for sched in &repo.schedules {
            let Some(st) = statuses.get(&sched.id) else { continue };
            let label = format!("{} / {}", repo.name, sched.display_name());
            if st.running {
                let mut line = label.clone();
                if let Some(r) = st.progress_ratio() {
                    line += &format!(" — {:.0}%", r * 100.0);
                } else if let Some(b) = st.progress_original_bytes.filter(|b| *b > 0) {
                    line += &format!(" — {}", format::bytes_binary(b));
                }
                running.push(line);
            } else if let Some(err) = &st.last_error {
                failed.push(format!("{label}: {}", err.lines().next().unwrap_or("")));
            }
        }
    }

    let text = match health {
        Health::Error => "󰀦 Borg".to_string(),
        Health::Running => match statuses.values().filter(|s| s.running).filter_map(|s| s.progress_ratio()).next() {
            Some(r) => format!("󰑓 {:.0}%", r * 100.0),
            None => "󰑓 Borg".to_string(),
        },
        Health::Ok => match &next {
            Some(n) => format!("󰆓 {}", format::relative_short(&n.fire_date)),
            None => "󰆓 Borg".to_string(),
        },
    };

    let mut tooltip: Vec<String> = vec![];
    let usage = previous.as_ref().map(|p| p.total_usage_bytes).unwrap_or(0);
    let archives = previous.as_ref().map(|p| p.archive_count).unwrap_or(0);
    let mut head = format!("{} {}", repos.len(), if repos.len() == 1 { "repo" } else { "repos" });
    if archives > 0 {
        head += &format!(" · {archives} archives");
    }
    if usage > 0 {
        head += &format!(" · {}", format::bytes_binary(usage));
    }
    tooltip.push(head);
    for p in previous.iter().flat_map(|p| p.by_provider.iter()).take(3) {
        tooltip.push(format!("{}: {}", p.label, format::bytes_binary(p.bytes)));
    }
    // The widget's "by repository" bars: the largest repositories.
    let top: Vec<String> = previous
        .iter()
        .flat_map(|p| p.top_repos.iter())
        .take(5)
        .map(|r| format!("{} {}", r.name, format::bytes_binary(r.bytes)))
        .collect();
    if !top.is_empty() {
        tooltip.push(format!("Largest: {}", top.join(" · ")));
    }
    if previous.as_ref().map(|p| p.by_provider.is_empty()).unwrap_or(true) {
        tooltip.push(format!("Open {} to generate statistics", paths::APP_NAME));
    }
    for line in &running {
        tooltip.push(format!("Running: {line}"));
    }
    for line in &failed {
        tooltip.push(format!("Failed: {line}"));
    }
    match &next {
        Some(n) => tooltip.push(format!(
            "Next: {} / {} — {} ({})",
            n.repo_name,
            n.schedule_name,
            format::medium_date_short_time(&n.fire_date),
            format::relative(&n.fire_date)
        )),
        None => tooltip.push("No scheduled backups".to_string()),
    }

    let class = match health {
        Health::Ok => "ok",
        Health::Running => "running",
        Health::Error => "error",
    };
    serde_json::json!({
        "text": text,
        "alt": class,
        "class": class,
        "tooltip": tooltip.join("\n"),
        "percentage": statuses.values().filter(|s| s.running).filter_map(|s| s.progress_ratio()).next()
            .map(|r| (r * 100.0) as i64).unwrap_or(0),
    })
    .to_string()
}
