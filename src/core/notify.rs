//! Desktop notifications from processes without a GUI (the `--run-backup`
//! runner). Goes through `notify-send`, which talks to whatever
//! org.freedesktop.Notifications daemon the session runs. The GUI process
//! uses `gio::Notification` instead (see `ui::notifications`) to get action
//! buttons.
use std::process::{Command, Stdio};

pub const ICON_NAME: &str = "us.priet.Luna";

pub fn send(title: &str, subtitle: &str, body: &str, urgent: bool) {
    let text = match (subtitle.is_empty(), body.is_empty()) {
        (true, _) => body.to_string(),
        (false, true) => subtitle.to_string(),
        (false, false) => format!("{subtitle}\n{body}"),
    };
    let result = Command::new("notify-send")
        .args(["--app-name", super::paths::APP_NAME, "--icon", ICON_NAME])
        .args(["--urgency", if urgent { "critical" } else { "normal" }])
        .arg("--")
        .arg(title)
        .arg(text)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status();
    if let Err(e) = result {
        eprintln!("notify-send notification failed: {e}");
    }
}

pub fn post_success(repo_name: &str, archive_name: &str) {
    send("Backup complete", repo_name, archive_name, false);
}

pub fn post_failure(repo_name: &str, error_message: &str) {
    send("Backup failed", repo_name, error_message, true);
}
