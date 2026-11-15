//! Thin wrapper around the `borg` command line tool. Every operation is a
//! subprocess; the passphrase is handed over through `BORG_PASSPHRASE` in
//! the child's environment and never touches disk or the command line.
//!
//! All functions block until borg exits — call them from a worker thread.
use super::keyring;
use super::models::{Archive, ArchiveEntry, ArchiveListResponse, BorgRepoInfo, Repository};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use uuid::Uuid;

/// Snapshot of a `borg create --progress --log-json` tick.
#[derive(Debug, Clone, PartialEq)]
pub struct BorgProgress {
    pub original_bytes: i64,
    pub compressed_bytes: i64,
    pub deduped_bytes: i64,
    pub file_count: i64,
    pub current_path: String,
}

pub const ERR_BINARY_NOT_FOUND: &str =
    "Could not find the borg binary. Install it with your package manager (the package is called 'borg' or 'borgbackup').";
pub const ERR_MISSING_PASSPHRASE: &str = "Could not unlock the repository passphrase.";

/// Handle used to stop a running borg process. Cloneable and thread-safe;
/// `cancel()` sends SIGTERM, which borg traps to write a checkpoint before
/// exiting, so the repository is left in a clean state.
#[derive(Clone, Default)]
pub struct Cancel(Arc<CancelInner>);

#[derive(Default)]
struct CancelInner {
    pid: AtomicI32,
    cancelled: AtomicBool,
}

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::SeqCst);
        let pid = self.0.pid.load(Ordering::SeqCst);
        if pid > 0 {
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::SeqCst)
    }

    fn set_pid(&self, pid: i32) {
        self.0.pid.store(pid, Ordering::SeqCst);
        // A cancel that raced with process start had no pid to signal.
        if pid > 0 && self.is_cancelled() {
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
        }
    }
}

// ------------------------------------------------------------------ binary

pub fn binary_path() -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    for extra in ["/usr/local/bin", "/usr/bin", "/bin", "/opt/borg/bin"] {
        dirs.push(PathBuf::from(extra));
    }
    dirs.push(super::paths::home().join(".local/bin"));
    dirs.into_iter().map(|d| d.join("borg")).find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

// --------------------------------------------------------- passphrase cache

fn cache() -> &'static Mutex<HashMap<Uuid, String>> {
    static CACHE: OnceLock<Mutex<HashMap<Uuid, String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Clears the in-memory passphrase cache.
pub fn forget_passphrases() {
    cache().lock().unwrap().clear();
}

pub fn forget(repo_id: &Uuid) {
    cache().lock().unwrap().remove(repo_id);
}

/// Returns a cached passphrase or reads it from the keyring.
fn passphrase_for(repo: &Repository) -> Result<String, String> {
    if let Some(p) = cache().lock().unwrap().get(&repo.id) {
        return Ok(p.clone());
    }
    let p = keyring::passphrase(&repo.id).ok_or_else(|| ERR_MISSING_PASSPHRASE.to_string())?;
    cache().lock().unwrap().insert(repo.id, p.clone());
    Ok(p)
}

// -------------------------------------------------------------- high level

/// Runs `borg info --json` with the supplied passphrase, bypassing the cache
/// and the keyring. Used by "Update passphrase" to verify a new passphrase
/// actually opens the repo before saving it.
pub fn verify_passphrase(repo: &Repository, passphrase: &str) -> Result<(), String> {
    run(&["info", "--json", &repo.url], Some(passphrase), repo.ssh_key_path.as_deref(), &RunOpts::default())
        .map(|_| ())
}

pub fn list_archives(repo: &Repository) -> Result<Vec<Archive>, String> {
    let pass = passphrase_for(repo)?;
    let data = run(&["list", "--json", &repo.url], Some(&pass), repo.ssh_key_path.as_deref(), &RunOpts::default())?;
    serde_json::from_slice::<ArchiveListResponse>(&data)
        .map(|r| r.archives)
        .map_err(|e| format!("Could not decode borg's response: {e}"))
}

/// `borg info --json <repo>`: repository-level stats for the stats panel.
pub fn info(repo: &Repository) -> Result<BorgRepoInfo, String> {
    let pass = passphrase_for(repo)?;
    let data = run(&["info", "--json", &repo.url], Some(&pass), repo.ssh_key_path.as_deref(), &RunOpts::default())?;
    BorgRepoInfo::parse(&data).map_err(|e| format!("Could not decode borg's response: {e}"))
}

/// Initializes a brand new repository. Doesn't go through the passphrase
/// cache because the repo doesn't exist in the local store yet.
pub fn init_repo(url: &str, passphrase: &str, encryption: &str, ssh_key_path: Option<&str>) -> Result<(), String> {
    run(&["init", "--encryption", encryption, url], Some(passphrase), ssh_key_path, &RunOpts::default()).map(|_| ())
}

/// The file manifest of a single archive via `borg list --json-lines`.
pub fn list_archive_entries(repo: &Repository, archive: &str) -> Result<Vec<ArchiveEntry>, String> {
    let pass = passphrase_for(repo)?;
    let target = format!("{}::{}", repo.url, archive);
    let data = run(&["list", "--json-lines", &target], Some(&pass), repo.ssh_key_path.as_deref(), &RunOpts::default())?;
    let mut entries = Vec::with_capacity(4096);
    for line in data.split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_slice::<ArchiveEntry>(line) {
            entries.push(entry);
        }
    }
    Ok(entries)
}

/// Extracts a single file to a temporary location and returns its path.
/// Used for previews.
pub fn extract_to_temp(repo: &Repository, archive: &str, entry_path: &str) -> Result<PathBuf, String> {
    let pass = passphrase_for(repo)?;
    let target = format!("{}::{}", repo.url, archive);
    let data = run(
        &["extract", "--stdout", &target, entry_path],
        Some(&pass),
        repo.ssh_key_path.as_deref(),
        &RunOpts::default(),
    )?;
    let filename = entry_path.rsplit('/').find(|s| !s.is_empty()).unwrap_or("preview");
    let destination = super::paths::temp_dir().join(filename);
    let _ = std::fs::remove_file(&destination);
    std::fs::write(&destination, data).map_err(|e| e.to_string())?;
    Ok(destination)
}

fn create_args(progress: bool, excludes: &[String], repo: &Repository, archive_name: &str, paths: &[String]) -> Vec<String> {
    let mut args: Vec<String> = ["create", "--stats", "--compression", "lz4"].map(String::from).to_vec();
    if progress {
        args.push("--log-json".into());
        args.push("--progress".into());
    }
    for pattern in excludes.iter().filter(|p| !p.is_empty()) {
        args.push("--exclude".into());
        args.push(pattern.clone());
    }
    args.push(format!("{}::{}", repo.url, archive_name));
    args.extend(paths.iter().cloned());
    args
}

/// Manual backup. `cancel` lets the caller stop it (SIGTERM to borg).
pub fn create_backup(
    repo: &Repository,
    archive_name: &str,
    paths: &[String],
    excludes: &[String],
    cancel: Option<&Cancel>,
) -> Result<(), String> {
    let pass = passphrase_for(repo)?;
    let args = create_args(false, excludes, repo, archive_name, paths);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run(&args, Some(&pass), repo.ssh_key_path.as_deref(), &RunOpts { cancel, ..Default::default() }).map(|_| ())
}

/// Backup with an externally supplied passphrase (scheduled runs). When
/// `on_progress` is set, borg runs with `--log-json --progress` and each
/// `archive_progress` event is forwarded as it arrives.
pub fn create_backup_unattended(
    repo: &Repository,
    passphrase: &str,
    archive_name: &str,
    paths: &[String],
    excludes: &[String],
    on_progress: Option<&(dyn Fn(BorgProgress) + Sync)>,
    cancel: Option<&Cancel>,
) -> Result<(), String> {
    let args = create_args(on_progress.is_some(), excludes, repo, archive_name, paths);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let line_handler = |line: &str| {
        if let (Some(cb), Some(p)) = (on_progress, parse_progress_line(line)) {
            cb(p);
        }
    };
    let opts = RunOpts {
        cancel,
        on_stderr_line: if on_progress.is_some() { Some(&line_handler) } else { None },
        ..Default::default()
    };
    run(&args, Some(passphrase), repo.ssh_key_path.as_deref(), &opts).map(|_| ())
}

fn prune_args(repo: &Repository, keep_daily: i64, keep_weekly: i64, keep_monthly: i64) -> Vec<String> {
    let mut args: Vec<String> = vec!["prune".into(), "--list".into()];
    for (flag, n) in [("--keep-daily", keep_daily), ("--keep-weekly", keep_weekly), ("--keep-monthly", keep_monthly)] {
        if n > 0 {
            args.push(flag.into());
            args.push(n.to_string());
        }
    }
    args.push(repo.url.clone());
    args
}

/// Prune + compact with an externally supplied passphrase (scheduled runs).
pub fn prune_unattended(
    repo: &Repository,
    passphrase: &str,
    keep_daily: i64,
    keep_weekly: i64,
    keep_monthly: i64,
) -> Result<(), String> {
    let args = prune_args(repo, keep_daily, keep_weekly, keep_monthly);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let key = repo.ssh_key_path.as_deref();
    run(&args, Some(passphrase), key, &RunOpts::default())?;
    // Compact is what actually returns space to the server. A failure here
    // must not fail the prune, but it should not vanish either — the
    // headless runner's log is the only place to see it.
    if let Err(e) = run(&["compact", &repo.url], Some(passphrase), key, &RunOpts::default()) {
        eprintln!("compact failed: {e}");
    }
    Ok(())
}

pub fn prune(repo: &Repository, keep_daily: i64, keep_weekly: i64, keep_monthly: i64) -> Result<(), String> {
    let pass = passphrase_for(repo)?;
    let args = prune_args(repo, keep_daily, keep_weekly, keep_monthly);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let key = repo.ssh_key_path.as_deref();
    run(&args, Some(&pass), key, &RunOpts::default())?;
    let _ = run(&["compact", &repo.url], Some(&pass), key, &RunOpts::default());
    Ok(())
}

/// Restores a whole archive into `destination`.
pub fn extract(repo: &Repository, archive: &str, destination: &Path) -> Result<(), String> {
    extract_paths(repo, archive, &[], destination)
}

/// Restores the given paths (files or folders; empty = everything) of an
/// archive into `destination`, recreating their directory structure.
pub fn extract_paths(repo: &Repository, archive: &str, entry_paths: &[String], destination: &Path) -> Result<(), String> {
    let pass = passphrase_for(repo)?;
    let target = format!("{}::{}", repo.url, archive);
    let mut args: Vec<&str> = vec!["extract", &target];
    args.extend(entry_paths.iter().map(String::as_str));
    run(
        &args,
        Some(&pass),
        repo.ssh_key_path.as_deref(),
        &RunOpts { working_directory: Some(destination), ..Default::default() },
    )
    .map(|_| ())
}

/// Mounts an archive with FUSE (`borg mount` daemonizes once mounted).
pub fn mount(repo: &Repository, archive: &str, mountpoint: &Path) -> Result<(), String> {
    let pass = passphrase_for(repo)?;
    std::fs::create_dir_all(mountpoint).map_err(|e| e.to_string())?;
    let target = format!("{}::{}", repo.url, archive);
    let mp = mountpoint.to_string_lossy();
    run(&["mount", &target, &mp], Some(&pass), repo.ssh_key_path.as_deref(), &RunOpts::default()).map(|_| ())
}

pub fn umount(mountpoint: &Path) -> Result<(), String> {
    let mp = mountpoint.to_string_lossy();
    run(&["umount", &mp], None, None, &RunOpts::default()).map(|_| ())
}

/// Forcibly releases a stuck repository lock (`borg break-lock <repo>`).
/// Only safe when no other process is writing to the repo.
pub fn break_lock(repo: &Repository) -> Result<(), String> {
    let pass = passphrase_for(repo)?;
    run(&["break-lock", &repo.url], Some(&pass), repo.ssh_key_path.as_deref(), &RunOpts::default()).map(|_| ())
}

/// Deletes a single archive. Space isn't reclaimed until the next compact.
pub fn delete_archive(repo: &Repository, archive: &str) -> Result<(), String> {
    let pass = passphrase_for(repo)?;
    let target = format!("{}::{}", repo.url, archive);
    run(&["delete", &target], Some(&pass), repo.ssh_key_path.as_deref(), &RunOpts::default()).map(|_| ())
}

// ----------------------------------------------------------- stderr parsing

/// Turns raw borg stderr into something worth showing a human. With
/// `--log-json` borg emits one JSON object per line (mixing
/// `archive_progress` ticks with `log_message` entries); we pull `.message`
/// out of each log record, drop the progress ticks and keep non-JSON lines
/// verbatim. Non-JSON input is returned unchanged.
pub fn sanitize_stderr(raw: &str) -> String {
    let trimmed_whole = raw.trim();
    if trimmed_whole.is_empty() {
        return String::new();
    }
    let (mut errors, mut warnings, mut plain, mut in_order) = (vec![], vec![], vec![], vec![]);
    let mut saw_json = false;
    for raw_line in raw.split(['\n', '\r']) {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('{') {
            if let Ok(serde_json::Value::Object(obj)) = serde_json::from_str::<serde_json::Value>(line) {
                saw_json = true;
                if obj.get("type").and_then(|t| t.as_str()) == Some("archive_progress") {
                    continue;
                }
                let Some(msg) = obj.get("message").and_then(|m| m.as_str()).map(str::trim).filter(|m| !m.is_empty())
                else {
                    continue;
                };
                in_order.push(msg.to_string());
                match obj.get("levelname").and_then(|l| l.as_str()).map(|l| l.to_uppercase()).as_deref() {
                    Some("ERROR") | Some("CRITICAL") => errors.push(msg.to_string()),
                    Some("WARNING") => warnings.push(msg.to_string()),
                    _ => {}
                }
                continue;
            }
        }
        in_order.push(line.to_string());
        plain.push(line.to_string());
    }
    if !saw_json || in_order.is_empty() {
        return trimmed_whole.to_string();
    }
    // When borg logged anything at WARNING or above, that is the cause of
    // the failure and the INFO chatter around it only buries it: put the
    // most severe lines first and drop INFO entirely in that case.
    let ordered = if errors.is_empty() && warnings.is_empty() {
        in_order
    } else {
        errors.into_iter().chain(warnings).chain(plain).collect()
    };
    let mut deduped: Vec<String> = vec![];
    for m in ordered {
        if deduped.last() != Some(&m) {
            deduped.push(m);
        }
    }
    deduped.join("\n")
}

/// Parses a single stderr line from `borg create --log-json --progress`.
/// Only `archive_progress` events return a value.
fn parse_progress_line(line: &str) -> Option<BorgProgress> {
    let obj: serde_json::Value = serde_json::from_str(line).ok()?;
    if obj.get("type")?.as_str()? != "archive_progress" {
        return None;
    }
    let num = |k: &str| obj.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
    Some(BorgProgress {
        original_bytes: num("original_size"),
        compressed_bytes: num("compressed_size"),
        deduped_bytes: num("deduplicated_size"),
        file_count: num("nfiles"),
        current_path: obj.get("path").and_then(|p| p.as_str()).unwrap_or("").to_string(),
    })
}

// --------------------------------------------------------- process plumbing

#[derive(Default)]
struct RunOpts<'a> {
    working_directory: Option<&'a Path>,
    cancel: Option<&'a Cancel>,
    /// Called for each stderr line as borg emits it.
    on_stderr_line: Option<&'a (dyn Fn(&str) + Sync)>,
}

fn run(args: &[&str], passphrase: Option<&str>, ssh_key_path: Option<&str>, opts: &RunOpts) -> Result<Vec<u8>, String> {
    let bin = binary_path().ok_or_else(|| ERR_BINARY_NOT_FOUND.to_string())?;
    let mut cmd = Command::new(bin);
    cmd.args(args);
    if let Some(dir) = opts.working_directory {
        cmd.current_dir(dir);
    }
    let existing = std::env::var("PATH").unwrap_or_default();
    cmd.env("PATH", format!("/usr/local/bin:/usr/bin:/bin:{existing}"));
    if let Some(p) = passphrase {
        cmd.env("BORG_PASSPHRASE", p);
    }
    if let Some(key) = ssh_key_path {
        cmd.env("BORG_RSH", format!("ssh -i {key} -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new"));
    }
    cmd.env("BORG_RELOCATED_REPO_ACCESS_IS_OK", "yes");
    cmd.env("BORG_UNKNOWN_UNENCRYPTED_REPO_ACCESS_IS_OK", "yes");
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| format!("Could not start borg: {e}"))?;
    if let Some(c) = opts.cancel {
        c.set_pid(child.id() as i32);
    }
    let mut stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let (out_data, err_data, status) = std::thread::scope(|scope| {
        let err_thread = scope.spawn(move || {
            let mut full: Vec<u8> = Vec::new();
            let mut reader = BufReader::new(stderr);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        full.extend_from_slice(&buf);
                        if let Some(handler) = opts.on_stderr_line {
                            let line = String::from_utf8_lossy(&buf);
                            let line = line.trim_end_matches(['\n', '\r']);
                            if !line.is_empty() {
                                handler(line);
                            }
                        }
                    }
                }
            }
            full
        });
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out);
        let status = child.wait();
        let err = err_thread.join().unwrap_or_default();
        (out, err, status)
    });
    if let Some(c) = opts.cancel {
        c.set_pid(0);
    }

    let status = status.map_err(|e| e.to_string())?;
    // Borg exit codes: 0 = success, 1 = warnings (the operation still
    // completed — e.g. an SSH banner or a few unreadable files), 2+ = real
    // error. Treat 1 as success so perfectly fine backups aren't reported
    // as failures.
    use std::os::unix::process::ExitStatusExt;
    let code = status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
    if code == 0 || code == 1 {
        Ok(out_data)
    } else {
        Err(format!("borg failed (code {code}): {}", sanitize_stderr(&String::from_utf8_lossy(&err_data))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_prefers_errors() {
        let raw = concat!(
            "{\"type\":\"log_message\",\"levelname\":\"INFO\",\"message\":\"Initializing cache\"}\n",
            "{\"type\":\"archive_progress\",\"nfiles\":3}\n",
            "Remote: banner\n",
            "{\"type\":\"log_message\",\"levelname\":\"ERROR\",\"message\":\"The storage quota has been exceeded\"}\n",
        );
        assert_eq!(sanitize_stderr(raw), "The storage quota has been exceeded\nRemote: banner");
        assert_eq!(sanitize_stderr("plain failure\n"), "plain failure");
    }

    #[test]
    fn progress_line() {
        let p = parse_progress_line(
            r#"{"type":"archive_progress","original_size":10,"compressed_size":5,"deduplicated_size":2,"nfiles":7,"path":"a/b"}"#,
        )
        .unwrap();
        assert_eq!((p.original_bytes, p.file_count, p.current_path.as_str()), (10, 7, "a/b"));
        assert!(parse_progress_line(r#"{"type":"log_message"}"#).is_none());
    }
}
