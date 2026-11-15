//! Headless entry point used when systemd starts us with
//! `--run-backup <repo-uuid> <schedule-uuid>`. Loads the repository and the
//! schedule from disk, reads the passphrase non-interactively from the
//! keyring, runs `borg create` (and optional prune) with a retry loop, and
//! writes a status file that the GUI picks up.
use super::borg::{self, BorgProgress, Cancel};
use super::models::{BackupRunStatus, BackupSchedule, Repository};
use super::{format, keyring, notify, schedule, status, store};
use chrono::Utc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use uuid::Uuid;

pub const MAX_ATTEMPTS: i64 = 3;
/// Delays after attempts 1 and 2.
pub const RETRY_DELAYS_SEC: [u64; 2] = [60, 120];

// ---------------------------------------------------------------- cancellation
//
// "Stop backup" in the UI runs `systemctl --user kill --signal=TERM` on our
// unit, which delivers SIGTERM to this process. Without a handler we would
// die immediately, borg would be orphaned and the status file would stay
// `running: true` forever. So a handler:
//   1. kills the borg child,
//   2. writes a "cancelled by user" status,
//   3. exits non-zero so systemd records a failure.

static CANCEL_REQUESTED: AtomicBool = AtomicBool::new(false);
static SIGNAL_PIPE_WR: AtomicI32 = AtomicI32::new(-1);

struct RunContext {
    schedule_id: Uuid,
    cancel: Cancel,
}

fn context() -> &'static Mutex<Option<RunContext>> {
    static CTX: OnceLock<Mutex<Option<RunContext>>> = OnceLock::new();
    CTX.get_or_init(|| Mutex::new(None))
}

extern "C" fn on_signal(sig: libc::c_int) {
    // Async-signal-safe: just poke the self-pipe; a normal thread does the work.
    let fd = SIGNAL_PIPE_WR.load(Ordering::SeqCst);
    if fd >= 0 {
        let byte = sig as u8;
        unsafe {
            libc::write(fd, &byte as *const u8 as *const libc::c_void, 1);
        }
    }
}

fn install_signal_handlers() {
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return;
    }
    SIGNAL_PIPE_WR.store(fds[1], Ordering::SeqCst);
    let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGINT, handler);
    }
    let read_fd = fds[0];
    std::thread::spawn(move || {
        let mut byte = 0u8;
        loop {
            let n = unsafe { libc::read(read_fd, &mut byte as *mut u8 as *mut libc::c_void, 1) };
            if n == 1 {
                handle_cancel(if byte as i32 == libc::SIGINT { "SIGINT" } else { "SIGTERM" });
            } else if n == 0 {
                return;
            }
        }
    });
}

fn handle_cancel(signal_name: &str) {
    if CANCEL_REQUESTED.swap(true, Ordering::SeqCst) {
        return;
    }
    eprintln!("[{}] received {signal_name} — cancelling backup", Utc::now());
    let guard = context().lock().unwrap();
    if let Some(ctx) = guard.as_ref() {
        // Borg traps SIGTERM and writes a checkpoint before exiting, so the
        // repo stays consistent and the cache keeps the already-uploaded
        // chunks for the next run to reuse.
        ctx.cancel.cancel();
        write_cancelled_status(&ctx.schedule_id);
    }
    // Exit right away to minimise the window where the main loop could
    // re-enter `mark_attempt` and overwrite the cancelled status.
    std::process::exit(130);
}

fn write_cancelled_status(schedule_id: &Uuid) {
    let mut st = status::read(schedule_id).unwrap_or_default();
    st.running = false;
    st.attempt = None;
    st.max_attempts = None;
    st.runner_pid = None;
    st.last_run = Some(Utc::now());
    st.last_error = Some("cancelled by user".into());
    st.clear_progress();
    status::write(&st, schedule_id);
}

fn is_cancelled() -> bool {
    CANCEL_REQUESTED.load(Ordering::SeqCst)
}

// ------------------------------------------------------------------ entry

pub fn run(repo_id_string: &str, schedule_id_string: &str) -> ! {
    println!(
        "[{}] Luna scheduled run starting for repo={repo_id_string} schedule={schedule_id_string}",
        Utc::now()
    );
    let (Ok(repo_id), Ok(schedule_id)) = (Uuid::parse_str(repo_id_string), Uuid::parse_str(schedule_id_string)) else {
        eprintln!("invalid uuid arguments");
        std::process::exit(2);
    };
    install_signal_handlers();
    let success = perform(&repo_id, &schedule_id);
    println!("[{}] Luna scheduled run finished — success={success}", Utc::now());
    std::process::exit(if success { 0 } else { 1 });
}

fn perform(repo_id: &Uuid, schedule_id: &Uuid) -> bool {
    let Some(repo) = store::load_repository(repo_id) else {
        finalize_failure(schedule_id, "?", "?", "repository not found in repositories.json");
        return false;
    };
    let Some(sched) = repo.schedules.iter().find(|s| &s.id == schedule_id).cloned() else {
        finalize_failure(
            schedule_id,
            &repo.name,
            "?",
            &format!("schedule {schedule_id} no longer exists in the repository"),
        );
        return false;
    };
    if sched.paths.is_empty() {
        finalize_failure(schedule_id, &repo.name, &sched.display_name(), "the schedule has no paths");
        return false;
    }
    let Some(passphrase) = keyring::passphrase(repo_id) else {
        finalize_failure(schedule_id, &repo.name, &sched.display_name(), "passphrase not available in the keyring");
        return false;
    };

    // Publish the run context so the signal handler can stop borg and write
    // the cancelled status.
    let cancel = Cancel::new();
    *context().lock().unwrap() = Some(RunContext { schedule_id: *schedule_id, cancel: cancel.clone() });

    // Fresh archive name per retry: borg can commit the archive and still
    // exit non-zero, and reusing the name would turn retries into "Archive
    // already exists" failures. Chunk-level dedup makes retrying with a new
    // name effectively free.
    let mut last_error = "unknown error".to_string();
    let mut archive = archive_name(&sched.name);
    let mut attempts_allowed = MAX_ATTEMPTS;
    let mut pruned_for_quota = false;
    let mut attempt = 0;
    while attempt < attempts_allowed {
        attempt += 1;
        if is_cancelled() {
            eprintln!("[{}] cancelled — aborting retry loop", Utc::now());
            return false;
        }
        mark_attempt(schedule_id, attempt, attempts_allowed);
        println!("[{}] attempt {attempt}/{attempts_allowed} — borg create ::{archive}", Utc::now());

        let err_text = match run_attempt(&repo, &sched, &passphrase, &archive, &cancel) {
            Ok(()) => {
                finalize_success(schedule_id, &repo.name, &sched.display_name(), &archive, attempt);
                return true;
            }
            Err(e) => e,
        };
        if is_cancelled() {
            return false;
        }

        // "Archive already exists" means a previous attempt (or a racing
        // run) actually committed that archive: success, not a retry.
        if err_text.to_lowercase().contains("already exists") {
            println!("[{}] archive {archive} already exists — treating as success", Utc::now());
            finalize_success(schedule_id, &repo.name, &sched.display_name(), &archive, attempt);
            return true;
        }

        last_error = err_text.clone();
        eprintln!("[{}] attempt {attempt} failed: {last_error}", Utc::now());

        // A full storage quota is not transient: retrying the same create
        // fails identically. With retention configured, prune + compact
        // (which frees space server-side) and give create one more go.
        if is_storage_quota_exceeded(&err_text) {
            if sched.has_retention() && !pruned_for_quota {
                pruned_for_quota = true;
                println!("[{}] storage quota exceeded — running prune + compact before retrying", Utc::now());
                if let Err(prune_error) = run_prune(&repo, &sched, &passphrase) {
                    eprintln!("[{}] prune failed: {prune_error}", Utc::now());
                    last_error += &format!("\nPrune before retry failed: {prune_error}");
                    break;
                }
                attempts_allowed = attempts_allowed.max(attempt + 1);
                archive = archive_name(&sched.name);
                continue;
            }
            last_error += if sched.has_retention() {
                "\nLuna pruned and compacted the repository and retried once, but it is still over quota. Raise the quota or keep fewer archives."
            } else {
                "\nLuna did not retry. Add a retention to this schedule so it can prune, or prune the repository by hand."
            };
            break;
        }

        if attempt < attempts_allowed {
            let delay = RETRY_DELAYS_SEC[((attempt - 1) as usize).min(RETRY_DELAYS_SEC.len() - 1)];
            println!("[{}] retrying in {delay}s", Utc::now());
            std::thread::sleep(Duration::from_secs(delay));
            archive = archive_name(&sched.name);
        }
    }

    if is_cancelled() {
        // The signal handler already wrote the cancelled status.
        return false;
    }
    finalize_failure(schedule_id, &repo.name, &sched.display_name(), &last_error);
    false
}

/// Runs a single borg create (plus optional prune) attempt. Prune failures
/// do NOT fail the attempt — the backup has already committed by then.
fn run_attempt(
    repo: &Repository,
    sched: &BackupSchedule,
    passphrase: &str,
    archive: &str,
    cancel: &Cancel,
) -> Result<(), String> {
    // Throttle status-file writes to ~1 Hz: borg emits progress events much
    // faster than that on large trees and each write is an atomic rename.
    let throttle = ProgressThrottle::new(sched.id, Duration::from_secs(1));
    let on_progress = |p: BorgProgress| throttle.submit(p);
    borg::create_backup_unattended(
        repo,
        passphrase,
        archive,
        &sched.paths,
        &sched.excludes,
        Some(&on_progress),
        Some(cancel),
    )?;
    if sched.has_retention() {
        if let Err(e) = run_prune(repo, sched, passphrase) {
            eprintln!("prune failed: {e}");
        }
    }
    Ok(())
}

/// Borg's `Repository.StorageQuotaExceeded`, as logged by
/// `borg serve --storage-quota` (BorgBase, BorgBox).
fn is_storage_quota_exceeded(error: &str) -> bool {
    let lower = error.to_lowercase();
    lower.contains("storage quota") || lower.contains("storagequotaexceeded")
}

fn run_prune(repo: &Repository, sched: &BackupSchedule, passphrase: &str) -> Result<(), String> {
    borg::prune_unattended(repo, passphrase, sched.keep_daily, sched.keep_weekly, sched.keep_monthly)
}

/// `scheduled-<slug>-<UTC stamp with milliseconds>`; the milliseconds keep
/// two nearly-simultaneous runs from generating the same archive name.
pub fn archive_name(schedule_name: &str) -> String {
    let stamp = Utc::now().format("%Y-%m-%d-%H%M%S-%3f");
    let slug = format::slug(schedule_name, '-');
    if slug.is_empty() { format!("scheduled-{stamp}") } else { format!("scheduled-{slug}-{stamp}") }
}

// ---------------------------------------------------------- status updates

fn mark_attempt(schedule_id: &Uuid, attempt: i64, max: i64) {
    let mut st = status::read(schedule_id).unwrap_or_default();
    st.running = true;
    st.attempt = Some(attempt);
    st.max_attempts = Some(max);
    // Lets the GUI detect stale "running" statuses whose process is gone.
    st.runner_pid = Some(std::process::id() as i32);
    st.clear_progress();
    st.progress_started_at = Some(Utc::now());
    status::write(&st, schedule_id);
}

fn finalize_success(schedule_id: &Uuid, repo_name: &str, schedule_name: &str, archive: &str, attempt: i64) {
    let mut st = status::read(schedule_id).unwrap_or_default();
    // Remember this run's final byte count so the next run's UI can compute
    // a percentage against it.
    if let Some(bytes) = st.progress_original_bytes.filter(|b| *b > 0) {
        st.last_total_original_bytes = Some(bytes);
    }
    st.running = false;
    st.attempt = None;
    st.max_attempts = None;
    st.runner_pid = None;
    st.last_run = Some(Utc::now());
    st.last_success = Some(Utc::now());
    st.last_archive_name = Some(archive.to_string());
    st.last_error = None;
    st.clear_progress();
    status::write(&st, schedule_id);

    let mut subtitle = format!("{repo_name} / {schedule_name}");
    if attempt > 1 {
        subtitle += &format!(" (attempt {attempt})");
    }
    notify::post_success(&subtitle, archive);
}

fn finalize_failure(schedule_id: &Uuid, repo_name: &str, schedule_name: &str, error: &str) {
    let mut st: BackupRunStatus = status::read(schedule_id).unwrap_or_default();
    st.running = false;
    st.attempt = None;
    st.max_attempts = None;
    st.runner_pid = None;
    st.last_run = Some(Utc::now());
    st.last_error = Some(error.to_string());
    st.clear_progress();
    status::write(&st, schedule_id);
    notify::post_failure(&format!("{repo_name} / {schedule_name}"), error);
}

/// Path of the log the systemd unit appends this run's output to.
pub fn log_path(repo_id: &Uuid, schedule_id: &Uuid) -> std::path::PathBuf {
    schedule::log_file(repo_id, schedule_id)
}

/// Coalesces high-frequency progress events into bounded-rate writes of the
/// status JSON.
struct ProgressThrottle {
    schedule_id: Uuid,
    min_interval: Duration,
    last_write: Mutex<Option<Instant>>,
}

impl ProgressThrottle {
    fn new(schedule_id: Uuid, min_interval: Duration) -> Self {
        ProgressThrottle { schedule_id, min_interval, last_write: Mutex::new(None) }
    }

    fn submit(&self, progress: BorgProgress) {
        {
            let mut last = self.last_write.lock().unwrap();
            let now = Instant::now();
            if last.map(|t| now.duration_since(t) < self.min_interval).unwrap_or(false) {
                return;
            }
            *last = Some(now);
        }
        if is_cancelled() {
            return;
        }
        let mut st = status::read(&self.schedule_id).unwrap_or_default();
        st.running = true;
        st.progress_original_bytes = Some(progress.original_bytes);
        st.progress_compressed_bytes = Some(progress.compressed_bytes);
        st.progress_deduped_bytes = Some(progress.deduped_bytes);
        st.progress_file_count = Some(progress.file_count);
        st.progress_current_path = Some(progress.current_path);
        st.progress_updated_at = Some(Utc::now());
        status::write(&st, &self.schedule_id);
    }
}
