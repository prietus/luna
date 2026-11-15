//! Per-schedule run status files, shared between the GUI and the headless
//! `--run-backup` runner. Keyed by schedule id because one repository can
//! have several schedules with independent run histories.
use super::models::BackupRunStatus;
use super::paths;
use chrono::Utc;
use std::collections::HashMap;
use std::path::PathBuf;
use uuid::Uuid;

pub fn file(schedule_id: &Uuid) -> PathBuf {
    paths::status_dir().join(format!("{schedule_id}.json"))
}

pub fn read(schedule_id: &Uuid) -> Option<BackupRunStatus> {
    let data = std::fs::read(file(schedule_id)).ok()?;
    serde_json::from_slice(&data).ok()
}

/// Atomic write (temp file + rename) so a concurrent reader never sees a
/// half-written payload.
pub fn write(status: &BackupRunStatus, schedule_id: &Uuid) {
    let Ok(data) = serde_json::to_vec(status) else { return };
    let target = file(schedule_id);
    let tmp = target.with_extension(format!("json.tmp{}", std::process::id()));
    if std::fs::write(&tmp, data).is_ok() {
        let _ = std::fs::rename(&tmp, &target);
    }
}

pub fn delete(schedule_id: &Uuid) {
    let _ = std::fs::remove_file(file(schedule_id));
}

/// True when the status's `runner_pid` is no longer a live process. A
/// missing pid also counts as dead so a stale "running" never wedges the UI.
fn is_runner_dead(status: &BackupRunStatus) -> bool {
    let Some(pid) = status.runner_pid.filter(|p| *p > 0) else { return true };
    if unsafe { libc::kill(pid, 0) } == 0 {
        return false;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// Reads every status file. A status that claims to be running while its
/// runner process is gone (crash, reboot, SIGKILL) is rewritten as failed so
/// the UI doesn't show a phantom "in progress" forever.
pub fn read_all() -> HashMap<Uuid, BackupRunStatus> {
    let mut out = HashMap::new();
    let Ok(entries) = std::fs::read_dir(paths::status_dir()) else { return out };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()).and_then(|s| Uuid::parse_str(s).ok()) else {
            continue;
        };
        if let Some(mut status) = read(&id) {
            if status.running && is_runner_dead(&status) {
                status.running = false;
                status.attempt = None;
                status.max_attempts = None;
                status.runner_pid = None;
                status.clear_progress();
                status.last_run = Some(Utc::now());
                status.last_error = Some("backup process died without finishing".into());
                write(&status, &id);
            }
            out.insert(id, status);
        }
    }
    out
}
