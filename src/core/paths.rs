//! XDG locations used by the app (the Linux counterparts of
//! `~/Library/Application Support/BorgMac`, `~/Library/Caches/BorgMac`,
//! `~/Library/Logs/BorgMac` and `~/Library/LaunchAgents`).
use std::path::PathBuf;

pub const APP_ID: &str = "us.priet.Luna";
pub const APP_NAME: &str = "Luna";
const DIR_NAME: &str = "luna";

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home().join(fallback),
    }
}

fn ensure(dir: PathBuf) -> PathBuf {
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Repository list, per-schedule status, status snapshot.
pub fn data_dir() -> PathBuf {
    ensure(xdg("XDG_DATA_HOME", ".local/share").join(DIR_NAME))
}

pub fn status_dir() -> PathBuf {
    ensure(data_dir().join("status"))
}

/// Cached archive manifests for the browser.
pub fn archive_cache_dir() -> PathBuf {
    ensure(xdg("XDG_CACHE_HOME", ".cache").join(DIR_NAME).join("archives"))
}

pub fn cache_dir() -> PathBuf {
    ensure(xdg("XDG_CACHE_HOME", ".cache").join(DIR_NAME))
}

/// One log per scheduled backup run.
pub fn logs_dir() -> PathBuf {
    ensure(xdg("XDG_STATE_HOME", ".local/state").join(DIR_NAME).join("logs"))
}

/// systemd user units (the launchd LaunchAgents equivalent).
pub fn systemd_user_dir() -> PathBuf {
    ensure(xdg("XDG_CONFIG_HOME", ".config").join("systemd/user"))
}

/// Scratch space for previews extracted from archives.
pub fn temp_dir() -> PathBuf {
    ensure(std::env::temp_dir().join(APP_NAME))
}

pub fn repositories_file() -> PathBuf {
    data_dir().join("repositories.json")
}

pub fn borgbox_servers_file() -> PathBuf {
    data_dir().join("borgbox_servers.json")
}

pub fn snapshot_file() -> PathBuf {
    data_dir().join("widget-snapshot.json")
}

/// Replaces a leading `$HOME` with `~` for display.
pub fn abbreviate_home(path: &str) -> String {
    let home = home();
    let home = home.to_string_lossy();
    if !home.is_empty() && home != "/" {
        if let Some(rest) = path.strip_prefix(home.as_ref()) {
            if rest.is_empty() || rest.starts_with('/') {
                return format!("~{rest}");
            }
        }
    }
    path.to_string()
}

pub fn expand_tilde(path: &str) -> String {
    if path == "~" {
        return home().to_string_lossy().into_owned();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home().join(rest).to_string_lossy().into_owned();
    }
    path.to_string()
}
