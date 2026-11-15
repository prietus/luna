//! Data models. JSON field names match the macOS app (camelCase) so a
//! `repositories.json` copied over from BorgMac loads unchanged.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------- schedules

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Frequency {
    Hourly,
    #[default]
    Daily,
    Weekly,
}

impl Frequency {
    pub const ALL: [Frequency; 3] = [Frequency::Hourly, Frequency::Daily, Frequency::Weekly];
    pub fn label(self) -> &'static str {
        match self {
            Frequency::Hourly => "Hourly",
            Frequency::Daily => "Daily",
            Frequency::Weekly => "Weekly",
        }
    }
}

fn default_hour() -> i64 {
    3
}
fn default_weekday() -> i64 {
    1
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupSchedule {
    /// Missing ids (legacy files) get a fresh UUID on decode.
    #[serde(default = "Uuid::new_v4")]
    pub id: Uuid,
    /// Short user-visible label. Can be empty; the UI falls back to the frequency.
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub frequency: Frequency,
    /// 0–23. Ignored for hourly.
    #[serde(default = "default_hour")]
    pub hour: i64,
    /// 0–59. Ignored for hourly.
    #[serde(default)]
    pub minute: i64,
    /// 0 = Sunday … 6 = Saturday. Only used for weekly.
    #[serde(default = "default_weekday")]
    pub weekday: i64,
    #[serde(default)]
    pub paths: Vec<String>,
    /// Borg exclude patterns passed as `--exclude <pattern>`.
    #[serde(default)]
    pub excludes: Vec<String>,
    #[serde(default)]
    pub keep_daily: i64,
    #[serde(default)]
    pub keep_weekly: i64,
    #[serde(default)]
    pub keep_monthly: i64,
}

impl Default for BackupSchedule {
    fn default() -> Self {
        BackupSchedule {
            id: Uuid::new_v4(),
            name: String::new(),
            frequency: Frequency::Daily,
            hour: 3,
            minute: 0,
            weekday: 1,
            paths: vec![],
            excludes: vec![],
            keep_daily: 0,
            keep_weekly: 0,
            keep_monthly: 0,
        }
    }
}

impl BackupSchedule {
    /// True when any `--keep-*` value is set, i.e. the scheduled run should
    /// prune (and compact) the repository after backing up.
    pub fn has_retention(&self) -> bool {
        self.keep_daily > 0 || self.keep_weekly > 0 || self.keep_monthly > 0
    }

    /// Falls back to the frequency if the user didn't name the schedule.
    pub fn display_name(&self) -> String {
        if self.name.is_empty() { self.frequency.label().to_string() } else { self.name.clone() }
    }

    /// Common exclusion patterns offered as a one-click preset in both the
    /// schedule editor and the manual backup sheet.
    pub const COMMON_EXCLUDE_PRESETS: [&'static str; 6] = [
        "**/node_modules",
        "**/__pycache__",
        "**/.venv",
        "**/target",
        "sh:**/.cache",
        "sh:**/.local/share/Trash",
    ];
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BackupRunStatus {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_success: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_archive_name: Option<String>,
    pub running: bool,
    /// Current attempt number (1-based) while running.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<i64>,

    // Live progress (populated while running), from
    // `borg create --log-json --progress` events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress_original_bytes: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress_compressed_bytes: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress_deduped_bytes: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress_file_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress_current_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress_updated_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress_started_at: Option<DateTime<Utc>>,

    /// Final `original_size` of the previous successful run; the UI divides
    /// the live progress by this to show a percentage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_total_original_bytes: Option<i64>,

    /// PID of the headless `--run-backup` process that wrote this status.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runner_pid: Option<i32>,
}

impl BackupRunStatus {
    pub fn clear_progress(&mut self) {
        self.progress_original_bytes = None;
        self.progress_compressed_bytes = None;
        self.progress_deduped_bytes = None;
        self.progress_file_count = None;
        self.progress_current_path = None;
        self.progress_updated_at = None;
        self.progress_started_at = None;
    }

    /// 0…1 when both the live byte count and the previous run's total are known.
    pub fn progress_ratio(&self) -> Option<f64> {
        match (self.progress_original_bytes, self.last_total_original_bytes) {
            (Some(done), Some(total)) if total > 0 => Some((done as f64 / total as f64).min(1.0)),
            _ => None,
        }
    }
}

// ------------------------------------------------------------- repositories

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Repository {
    pub id: Uuid,
    pub name: String,
    pub url: String,
    /// Optional private SSH key dedicated to this repository (passed to borg
    /// through BORG_RSH so each repo uses its own key).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_key_path: Option<String>,
    /// Zero or more systemd-timer-backed backup schedules.
    pub schedules: Vec<BackupSchedule>,
}

impl Repository {
    pub fn new(name: &str, url: &str, ssh_key_path: Option<String>) -> Self {
        Repository {
            id: Uuid::new_v4(),
            name: name.to_string(),
            url: url.to_string(),
            ssh_key_path,
            schedules: vec![],
        }
    }
}

impl<'de> Deserialize<'de> for Repository {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Raw {
            id: Option<Uuid>,
            name: String,
            url: String,
            ssh_key_path: Option<String>,
            schedules: Option<Vec<BackupSchedule>>,
            /// Legacy single-schedule key from the pre-multi-job layout.
            schedule: Option<BackupSchedule>,
        }
        let raw = Raw::deserialize(d)?;
        let schedules = match (raw.schedules, raw.schedule) {
            (Some(list), _) => list,
            (None, Some(legacy)) => vec![legacy],
            (None, None) => vec![],
        };
        Ok(Repository {
            id: raw.id.unwrap_or_else(Uuid::new_v4),
            name: raw.name,
            url: raw.url,
            ssh_key_path: raw.ssh_key_path,
            schedules,
        })
    }
}

/// A registered BorgBox server (self-hosted REST daemon that manages borg repos).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BorgBoxServer {
    #[serde(default = "Uuid::new_v4")]
    pub id: Uuid,
    /// User-facing label, e.g. "Home NAS".
    pub name: String,
    /// `http://hive.local:9999` (no /api/v1 suffix).
    #[serde(rename = "daemonURL")]
    pub daemon_url: String,
    pub ssh_host: String,
    pub ssh_user: String,
    pub ssh_port: i64,
}

// ----------------------------------------------------------------- archives

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct Archive {
    pub name: String,
    #[serde(rename = "id")]
    pub archive_id: String,
    pub start: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArchiveListResponse {
    pub archives: Vec<Archive>,
}

/// A single entry inside a Borg archive, as returned by `borg list --json-lines`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArchiveEntry {
    #[serde(rename = "type")]
    pub kind: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime: Option<String>,
}

impl ArchiveEntry {
    pub fn is_directory(&self) -> bool {
        self.kind == "d"
    }
    pub fn is_symlink(&self) -> bool {
        self.kind == "l"
    }
    pub fn size_bytes(&self) -> u64 {
        self.size.unwrap_or(0)
    }
    pub fn name(&self) -> &str {
        self.path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(&self.path)
    }
}

/// Decoded `borg info --json <repo>` — only what the stats panel uses.
#[derive(Debug, Clone, PartialEq)]
pub struct BorgRepoInfo {
    pub repository_id: String,
    pub location: String,
    pub last_modified: Option<String>,
    /// Sum of original file sizes across all archives.
    pub total_size: i64,
    /// Sum after compression.
    pub total_csize: i64,
    /// Bytes actually stored after dedup.
    pub unique_csize: i64,
    pub total_unique_chunks: Option<i64>,
    pub total_chunks: Option<i64>,
    pub archive_count: usize,
}

impl BorgRepoInfo {
    pub fn parse(data: &[u8]) -> Result<BorgRepoInfo, String> {
        let v: serde_json::Value = serde_json::from_slice(data).map_err(|e| e.to_string())?;
        let repo = v.get("repository").ok_or("missing key: repository")?;
        let stats = v
            .get("cache")
            .and_then(|c| c.get("stats"))
            .ok_or("missing key: cache.stats")?;
        let need_str = |o: &serde_json::Value, k: &str| -> Result<String, String> {
            o.get(k).and_then(|x| x.as_str()).map(str::to_string).ok_or(format!("missing key: {k}"))
        };
        let need_i64 = |k: &str| -> Result<i64, String> {
            stats.get(k).and_then(|x| x.as_i64()).ok_or(format!("missing key: {k}"))
        };
        Ok(BorgRepoInfo {
            repository_id: need_str(repo, "id")?,
            location: need_str(repo, "location")?,
            last_modified: repo.get("last_modified").and_then(|x| x.as_str()).map(str::to_string),
            total_size: need_i64("total_size")?,
            total_csize: need_i64("total_csize")?,
            unique_csize: need_i64("unique_csize")?,
            total_unique_chunks: stats.get("total_unique_chunks").and_then(|x| x.as_i64()),
            total_chunks: stats.get("total_chunks").and_then(|x| x.as_i64()),
            archive_count: v.get("archives").and_then(|a| a.as_array()).map(|a| a.len()).unwrap_or(0),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_single_schedule_is_wrapped() {
        let json = r#"[{"name":"a","url":"/x","schedule":{"frequency":"weekly","paths":["/home"]}}]"#;
        let repos: Vec<Repository> = serde_json::from_str(json).unwrap();
        assert_eq!(repos[0].schedules.len(), 1);
        assert_eq!(repos[0].schedules[0].frequency, Frequency::Weekly);
        assert_eq!(repos[0].schedules[0].hour, 3);
    }

    #[test]
    fn mac_repository_file_round_trips() {
        let json = r#"[{"id":"0E0A1C5E-7C0B-4F0A-9B5E-0123456789AB","name":"foo","url":"ssh://x@y/./repo","sshKeyPath":"/k","schedules":[{"id":"1E0A1C5E-7C0B-4F0A-9B5E-0123456789AB","name":"n","frequency":"daily","hour":22,"minute":20,"weekday":1,"paths":["/a"],"excludes":[],"keepDaily":7,"keepWeekly":0,"keepMonthly":0}]}]"#;
        let repos: Vec<Repository> = serde_json::from_str(json).unwrap();
        assert_eq!(repos[0].schedules[0].keep_daily, 7);
        let out = serde_json::to_string(&repos).unwrap();
        assert!(out.contains("\"sshKeyPath\":\"/k\""));
        assert!(out.contains("\"keepDaily\":7"));
    }
}
