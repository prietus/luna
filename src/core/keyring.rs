//! Secret storage through the freedesktop Secret Service (GNOME Keyring,
//! KeePassXC, KWallet…) — the Linux counterpart of the macOS Keychain.
//!
//! We drive `secret-tool` (shipped with libsecret) instead of linking a
//! client library: secrets travel over stdin/stdout, never on a command
//! line, and the exact same code path works from the GUI and from the
//! headless `--run-backup` runner started by systemd.
use std::io::Write;
use std::process::{Command, Stdio};
use uuid::Uuid;

const SERVICE_REPO: &str = "us.priet.Luna";
const SERVICE_BORGBASE: &str = "us.priet.Luna.BorgBase";
const SERVICE_BORGBOX: &str = "us.priet.Luna.BorgBox";
const SERVICE_BORGBOX_REPO: &str = "us.priet.Luna.BorgBox.repo";
const BORGBASE_ACCOUNT: &str = "api-token";

fn store(service: &str, account: &str, label: &str, secret: &str) -> Result<(), String> {
    let mut child = Command::new("secret-tool")
        .args(["store", "--label", label, "service", service, "account", account])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Could not run secret-tool (is libsecret installed?): {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(secret.as_bytes()).map_err(|e| e.to_string())?;
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(format!(
            "The keyring returned an error ({})",
            if msg.is_empty() { format!("exit {}", out.status.code().unwrap_or(-1)) } else { msg }
        ))
    }
}

fn lookup(service: &str, account: &str) -> Option<String> {
    let out = Command::new("secret-tool")
        .args(["lookup", "service", service, "account", account])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mut s = String::from_utf8(out.stdout).ok()?;
    if s.ends_with('\n') {
        s.pop();
    }
    if s.is_empty() { None } else { Some(s) }
}

fn clear(service: &str, account: &str) {
    let _ = Command::new("secret-tool")
        .args(["clear", "service", service, "account", account])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

// ------------------------------------------------------ repository passphrases

pub fn set_passphrase(passphrase: &str, repo_id: &Uuid) -> Result<(), String> {
    store(SERVICE_REPO, &repo_id.to_string(), "Luna repository passphrase", passphrase)
}

/// Reads the stored passphrase. Never prompts beyond what the keyring itself
/// does when it is locked; safe for scheduled (unattended) runs.
pub fn passphrase(repo_id: &Uuid) -> Option<String> {
    lookup(SERVICE_REPO, &repo_id.to_string())
}

pub fn delete_passphrase(repo_id: &Uuid) {
    clear(SERVICE_REPO, &repo_id.to_string());
}

// ------------------------------------------------------------ BorgBase token

pub fn set_borgbase_token(token: &str) -> Result<(), String> {
    store(SERVICE_BORGBASE, BORGBASE_ACCOUNT, "Luna BorgBase API token", token)
}

pub fn borgbase_token() -> Option<String> {
    lookup(SERVICE_BORGBASE, BORGBASE_ACCOUNT)
}

pub fn delete_borgbase_token() {
    clear(SERVICE_BORGBASE, BORGBASE_ACCOUNT);
}

// ------------------------------------- BorgBox tokens (keyed by server UUID)

pub fn set_borgbox_token(token: &str, server_id: &Uuid) -> Result<(), String> {
    store(SERVICE_BORGBOX, &server_id.to_string(), "Luna BorgBox token", token)
}

pub fn borgbox_token(server_id: &Uuid) -> Option<String> {
    lookup(SERVICE_BORGBOX, &server_id.to_string())
}

pub fn delete_borgbox_token(server_id: &Uuid) {
    clear(SERVICE_BORGBOX, &server_id.to_string());
}

// ---------------- BorgBox repo passphrases (keyed by `<serverUUID>.<repo>`)

fn borgbox_repo_account(server_id: &Uuid, repo: &str) -> String {
    format!("{server_id}.{repo}")
}

pub fn set_borgbox_repo_passphrase(passphrase: &str, server_id: &Uuid, repo: &str) -> Result<(), String> {
    store(
        SERVICE_BORGBOX_REPO,
        &borgbox_repo_account(server_id, repo),
        "Luna BorgBox repository passphrase",
        passphrase,
    )
}

pub fn borgbox_repo_passphrase(server_id: &Uuid, repo: &str) -> Option<String> {
    lookup(SERVICE_BORGBOX_REPO, &borgbox_repo_account(server_id, repo))
}

pub fn delete_borgbox_repo_passphrase(server_id: &Uuid, repo: &str) {
    clear(SERVICE_BORGBOX_REPO, &borgbox_repo_account(server_id, repo));
}
