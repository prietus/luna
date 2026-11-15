//! Installs / removes systemd user timers that trigger scheduled backups by
//! invoking this binary with `--run-backup <repo-id> <schedule-id>` (the
//! Linux counterpart of the launchd agents BorgMac uses).
//!
//! Unit names are scoped by both the repo UUID and the schedule UUID so a
//! single repo can host several jobs with independent frequencies and paths.
use super::models::{BackupSchedule, Frequency};
use super::paths;
use chrono::{DateTime, Datelike, Duration, Local, TimeZone, Timelike};
use std::path::PathBuf;
use std::process::Command;
use uuid::Uuid;

pub const UNIT_PREFIX: &str = "luna-backup-";

pub fn unit_name(repo_id: &Uuid, schedule_id: &Uuid) -> String {
    format!("{UNIT_PREFIX}{repo_id}-{schedule_id}")
}

pub fn service_file(repo_id: &Uuid, schedule_id: &Uuid) -> PathBuf {
    paths::systemd_user_dir().join(format!("{}.service", unit_name(repo_id, schedule_id)))
}

pub fn timer_file(repo_id: &Uuid, schedule_id: &Uuid) -> PathBuf {
    paths::systemd_user_dir().join(format!("{}.timer", unit_name(repo_id, schedule_id)))
}

pub fn log_file(repo_id: &Uuid, schedule_id: &Uuid) -> PathBuf {
    paths::logs_dir().join(format!("backup-{repo_id}-{schedule_id}.log"))
}

pub fn is_installed(repo_id: &Uuid, schedule_id: &Uuid) -> bool {
    timer_file(repo_id, schedule_id).exists()
}

// ------------------------------------------------------- install / uninstall

pub fn install(repo_id: &Uuid, schedule: &BackupSchedule) -> Result<(), String> {
    let binary = std::env::current_exe()
        .map_err(|_| "Could not determine the path to the Luna binary.".to_string())?;
    let unit = unit_name(repo_id, &schedule.id);
    std::fs::write(service_file(repo_id, &schedule.id), make_service(repo_id, schedule, &binary.to_string_lossy()))
        .map_err(|e| e.to_string())?;
    std::fs::write(timer_file(repo_id, &schedule.id), make_timer(schedule)).map_err(|e| e.to_string())?;
    systemctl(&["daemon-reload"])?;
    // `enable --now` on an already-active timer keeps the old calendar
    // spec loaded; restart so an edited schedule takes effect.
    systemctl(&["enable", &format!("{unit}.timer")])?;
    systemctl(&["restart", &format!("{unit}.timer")])?;
    Ok(())
}

pub fn uninstall(repo_id: &Uuid, schedule_id: &Uuid) {
    let unit = unit_name(repo_id, schedule_id);
    let _ = systemctl(&["disable", "--now", &format!("{unit}.timer")]);
    // Like `launchctl bootout`: a run in flight goes down with its schedule
    // instead of finishing against units and a status file that are gone.
    let _ = systemctl(&["stop", &format!("{unit}.service")]);
    let _ = std::fs::remove_file(timer_file(repo_id, schedule_id));
    let _ = std::fs::remove_file(service_file(repo_id, schedule_id));
    let _ = systemctl(&["daemon-reload"]);
}

/// Cascading uninstall when a repository is deleted — removes every unit
/// whose name starts with `luna-backup-<repoUUID>-`.
pub fn uninstall_all(repo_id: &Uuid) {
    let prefix = format!("{UNIT_PREFIX}{repo_id}-");
    let Ok(entries) = std::fs::read_dir(paths::systemd_user_dir()) else { return };
    let mut removed = false;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(&prefix) {
            continue;
        }
        if name.ends_with(".timer") {
            let _ = systemctl(&["disable", "--now", &name]);
        }
        let _ = std::fs::remove_file(entry.path());
        removed = true;
    }
    if removed {
        let _ = systemctl(&["daemon-reload"]);
    }
}

/// Triggers the job for a schedule immediately, regardless of its calendar.
/// Used by "Run now".
pub fn kickstart(repo_id: &Uuid, schedule_id: &Uuid) -> Result<(), String> {
    let unit = unit_name(repo_id, schedule_id);
    systemctl(&["start", "--no-block", &format!("{unit}.service")]).map(|_| ())
}

/// Sends SIGTERM to the currently-running instance of a scheduled backup.
/// The runner forwards it to borg, which writes a checkpoint and exits
/// cleanly. Silently does nothing when the unit isn't running.
pub fn cancel(repo_id: &Uuid, schedule_id: &Uuid) {
    let unit = unit_name(repo_id, schedule_id);
    let _ = systemctl(&["kill", "--kill-whom=main", "--signal=TERM", &format!("{unit}.service")]);
}

// ------------------------------------------------------------ next fire date

/// Client-side prediction of the next firing, for display only.
pub fn next_fire_date(schedule: &BackupSchedule, after: DateTime<Local>) -> Option<DateTime<Local>> {
    let at = |date: chrono::NaiveDate, hour: i64, minute: i64| -> Option<DateTime<Local>> {
        let naive = date.and_hms_opt(hour.clamp(0, 23) as u32, minute.clamp(0, 59) as u32, 0)?;
        Local.from_local_datetime(&naive).earliest()
    };
    match schedule.frequency {
        Frequency::Hourly => {
            // `OnCalendar=hourly` fires at the top of every hour.
            let top = after.with_minute(0)?.with_second(0)?.with_nanosecond(0)?;
            Some(top + Duration::hours(1))
        }
        Frequency::Daily => {
            let candidate = at(after.date_naive(), schedule.hour, schedule.minute)?;
            if candidate > after {
                Some(candidate)
            } else {
                at(after.date_naive() + Duration::days(1), schedule.hour, schedule.minute)
            }
        }
        Frequency::Weekly => {
            // schedule.weekday: 0 = Sunday … 6 = Saturday.
            let current = after.weekday().num_days_from_sunday() as i64;
            let days_ahead = (schedule.weekday - current).rem_euclid(7);
            let day = after.date_naive() + Duration::days(days_ahead);
            let candidate = at(day, schedule.hour, schedule.minute)?;
            if candidate > after { Some(candidate) } else { at(day + Duration::days(7), schedule.hour, schedule.minute) }
        }
    }
}

// ----------------------------------------------------------- unit generation

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

pub fn on_calendar(schedule: &BackupSchedule) -> String {
    let (h, m) = (schedule.hour.clamp(0, 23), schedule.minute.clamp(0, 59));
    match schedule.frequency {
        Frequency::Hourly => "hourly".to_string(),
        Frequency::Daily => format!("*-*-* {h:02}:{m:02}:00"),
        Frequency::Weekly => {
            format!("{} *-*-* {h:02}:{m:02}:00", WEEKDAYS[schedule.weekday.rem_euclid(7) as usize])
        }
    }
}

/// Quotes one argument for a systemd `Exec*=` line.
fn systemd_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%").replace('$', "$$"))
}

/// `Environment=` lines for the XDG base directories this process was
/// started with. The systemd user manager does not inherit them from the
/// session, and the runner must read the same repositories.json, status and
/// log directories as the GUI that installed the unit.
fn xdg_environment() -> String {
    ["XDG_DATA_HOME", "XDG_STATE_HOME", "XDG_CACHE_HOME", "XDG_CONFIG_HOME"]
        .iter()
        .filter_map(|var| {
            let value = std::env::var(var).ok().filter(|v| !v.is_empty())?;
            Some(format!("Environment={}\n", systemd_quote(&format!("{var}={value}"))))
        })
        .collect()
}

fn make_service(repo_id: &Uuid, schedule: &BackupSchedule, binary: &str) -> String {
    let log = log_file(repo_id, &schedule.id);
    let log = log.to_string_lossy().replace('%', "%%");
    format!(
        "[Unit]\n\
         Description=Luna scheduled backup ({name})\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         {env}ExecStart={bin} --run-backup {repo_id} {sched_id}\n\
         StandardOutput=append:{log}\n\
         StandardError=append:{log}\n\
         Nice=10\n\
         IOSchedulingClass=best-effort\n\
         IOSchedulingPriority=7\n\
         TimeoutStopSec=120\n",
        name = schedule.display_name().replace(['\n', '\r'], " ").replace('%', "%%"),
        env = xdg_environment(),
        bin = systemd_quote(binary),
        sched_id = schedule.id,
    )
}

fn make_timer(schedule: &BackupSchedule) -> String {
    format!(
        "[Unit]\n\
         Description=Luna backup schedule ({name})\n\
         \n\
         [Timer]\n\
         OnCalendar={cal}\n\
         Persistent=true\n\
         \n\
         [Install]\n\
         WantedBy=timers.target\n",
        name = schedule.display_name().replace(['\n', '\r'], " ").replace('%', "%%"),
        cal = on_calendar(schedule),
    )
}

// --------------------------------------------------------- systemctl plumbing

fn systemctl(args: &[&str]) -> Result<String, String> {
    let out = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .map_err(|e| format!("Could not run systemctl: {e}"))?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if out.status.success() {
        Ok(text)
    } else {
        Err(format!("systemctl failed (code {}): {}", out.status.code().unwrap_or(-1), text.trim()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_specs() {
        let mut s = BackupSchedule { hour: 22, minute: 5, weekday: 0, ..Default::default() };
        assert_eq!(on_calendar(&s), "*-*-* 22:05:00");
        s.frequency = Frequency::Weekly;
        assert_eq!(on_calendar(&s), "Sun *-*-* 22:05:00");
        s.frequency = Frequency::Hourly;
        assert_eq!(on_calendar(&s), "hourly");
    }

    #[test]
    fn next_fire() {
        let now = Local.with_ymd_and_hms(2026, 11, 15, 12, 30, 0).unwrap(); // a Sunday
        let mut s = BackupSchedule { hour: 3, minute: 0, weekday: 0, ..Default::default() };
        assert_eq!(next_fire_date(&s, now).unwrap(), Local.with_ymd_and_hms(2026, 11, 16, 3, 0, 0).unwrap());
        s.frequency = Frequency::Weekly;
        assert_eq!(next_fire_date(&s, now).unwrap(), Local.with_ymd_and_hms(2026, 11, 22, 3, 0, 0).unwrap());
        s.weekday = 2;
        assert_eq!(next_fire_date(&s, now).unwrap(), Local.with_ymd_and_hms(2026, 11, 17, 3, 0, 0).unwrap());
        s.frequency = Frequency::Hourly;
        assert_eq!(next_fire_date(&s, now).unwrap(), Local.with_ymd_and_hms(2026, 11, 15, 13, 0, 0).unwrap());
    }
}
