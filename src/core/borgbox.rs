//! BorgBox daemon REST client (port of `BorgBoxClient.swift`).
//!
//! Everything here is blocking; call it from a worker thread. Every public
//! function returns `Result<T, String>` where the error is the human-readable
//! message (same text as the Swift `BorgBoxError.errorDescription`).

#![allow(dead_code)]

use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{OnceLock, mpsc};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};

use crate::core::keyring;
use crate::core::models::BorgBoxServer;

// MARK: - Errors

pub const ERR_INVALID_URL: &str = "The BorgBox daemon URL is not valid.";
pub const ERR_INVALID_RESPONSE: &str = "Invalid response from the BorgBox daemon.";
pub const ERR_NO_TOKEN: &str = "No token saved for this server.";

fn err_http(code: u16, body: &str) -> String {
    format!("BorgBox returned HTTP {code}: {body}")
}

fn err_api(msg: &str) -> String {
    format!("BorgBox: {msg}")
}

fn err_decode(detail: &str) -> String {
    format!("Could not decode the daemon response: {detail}")
}

// MARK: - Models

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BorgBoxInfo {
    pub version: String,
    pub repo_root: Option<String>,
    pub free_bytes: Option<i64>,
    pub total_bytes: Option<i64>,
    pub uptime_sec: Option<i64>,
    pub borg_version: Option<String>,
    pub go_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BorgBoxRemoteRepo {
    pub name: String,
    pub path: String,
    pub size_bytes: Option<i64>,
    pub initialized: Option<bool>,
    pub modified_at: Option<String>,
    /// Absolute `ssh://` URL as reported by the daemon (v0.3.0+). Built from
    /// BORGBOX_SSH_* env on the server side, so it reflects the true host
    /// reachable for borg even when the HTTP daemon URL uses a different name.
    pub ssh_url: Option<String>,
    /// True when the repo is registered in `authorized_keys`. `false` means
    /// the directory exists under the repo root but no key is authorized —
    /// the user can adopt it via POST /repos/import. Older daemons omit the
    /// field, in which case we treat it as registered for backwards compat.
    pub registered: Option<bool>,
    /// Whether the repo's authorized_keys line forces `borg serve
    /// --append-only`. Older daemons omit the field; treat None as
    /// "unknown / assume off".
    pub append_only: Option<bool>,
}

impl BorgBoxRemoteRepo {
    pub fn id(&self) -> &str {
        &self.name
    }

    /// Effective registration flag: defaults to true when the daemon doesn't
    /// report the field (pre-v0.3.0) so the UI doesn't hide existing repos.
    pub fn is_registered(&self) -> bool {
        self.registered.unwrap_or(true)
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BorgBoxSystemStats {
    pub hostname: String,
    pub uptime_seconds: i64,
    pub load: Vec<f64>,
    pub storage: BorgBoxStorage,
    pub borgbox_version: Option<String>,
}

/// Swift: `BorgBoxSystemStats.Storage`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BorgBoxStorage {
    pub path: String,
    pub total_bytes: i64,
    pub used_bytes: i64,
    pub free_bytes: i64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BorgBoxSession {
    pub pid: i64,
    pub repo: String,
    pub client_ip: Option<String>,
    pub started_at: Option<String>,
    pub cpu_percent: Option<f64>,
    pub rss_bytes: Option<i64>,
}

impl BorgBoxSession {
    pub fn id(&self) -> i64 {
        self.pid
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BorgBoxJob {
    pub id: String,
    pub repo: Option<String>,
    pub kind: String,
    pub status: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub exit_code: Option<i64>,
    pub log_tail: Option<Vec<String>>,
}

impl BorgBoxJob {
    /// True when the daemon has reported a terminal state — the UI can stop
    /// polling.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status.as_str(),
            "succeeded" | "failed" | "done" | "error" | "cancelled"
        )
    }

    pub fn is_failure(&self) -> bool {
        match self.status.as_str() {
            "failed" | "error" | "cancelled" => true,
            _ => self.exit_code.is_some_and(|code| code != 0),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BorgBoxJobStart {
    pub job_id: String,
    pub status: String,
}

/// A single SSE payload from `/jobs/{id}/stream`. The daemon ships every
/// event as the same JSON shape with a `type` discriminator — match on
/// `event_type` (Swift: `type`; the wire key is `type`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BorgBoxJobStreamEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub line: Option<String>,
    pub status: Option<String>,
    pub exit_code: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BorgBoxRemoteArchive {
    pub id: String,
    pub name: String,
    pub hostname: Option<String>,
    pub username: Option<String>,
    /// ISO-8601 timestamp of when the archive was created (borg `time`).
    pub time: Option<String>,
    pub comment: Option<String>,
    /// Optional richer fields kept for forward compatibility — v0.2.0 of the
    /// daemon doesn't return them, but newer builds may.
    pub duration_seconds: Option<f64>,
    pub nfiles: Option<i64>,
    pub original_size: Option<i64>,
    pub compressed_size: Option<i64>,
    pub deduplicated_size: Option<i64>,
}

/// A maintenance schedule registered on a BorgBox daemon (v0.4.0+). Every
/// field is always present in the wire JSON — `last_run`, `last_status`,
/// `last_job_id` come back as the empty string before the schedule has fired
/// for the first time.
///
/// `weekday` is 0..6 (Sunday..Saturday) and only meaningful when
/// `cadence == "weekly"`. `day` is 1..28 and only meaningful for
/// `"monthly"`. Both come back as 0 when the cadence doesn't use them.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BorgBoxSchedule {
    pub id: String,
    pub repo: String,
    /// `"check"` or `"compact"` (the daemon might grow new kinds).
    pub kind: String,
    /// `"daily"`, `"weekly"` or `"monthly"`.
    pub cadence: String,
    pub hour: i64,
    pub minute: i64,
    pub weekday: i64,
    pub day: i64,
    pub enabled: bool,
    pub created_at: String,
    /// RFC3339 UTC timestamp of the most recent fire, or `""`.
    pub last_run: String,
    /// One of `""`, `"running"`, `"done"`, `"error"`, `"skipped"`.
    pub last_status: String,
    pub last_job_id: String,
    /// RFC3339 UTC timestamp of the next scheduled fire.
    pub next_run: String,
}

impl BorgBoxSchedule {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn last_run_date(&self) -> Option<DateTime<Utc>> {
        parse_rfc3339(&self.last_run)
    }

    pub fn next_run_date(&self) -> Option<DateTime<Utc>> {
        parse_rfc3339(&self.next_run)
    }
}

/// A "stale repo" alert registered with the BorgBox daemon. The daemon polls
/// each repo's mtime on a tick and fires the alert's webhook when the gap
/// exceeds `stale_after_hours`. The signing secret is write-only and never
/// travels back to the client.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BorgBoxAlert {
    pub id: String,
    pub repo: String,
    pub stale_after_hours: i64,
    pub webhook_url: String,
    pub enabled: bool,
    /// 0 means "never re-notify while the repo stays stale". Any positive
    /// value is the minimum gap (in hours) between repeated deliveries.
    pub renotify_hours: i64,
    /// Whether the daemon HMAC-signs this alert's webhook deliveries.
    /// Daemons older than 0.6 omit the field; missing/null means false.
    #[serde(default, deserialize_with = "null_as_default")]
    pub has_secret: bool,
    pub created_at: String,
    pub last_check_at: String,
    /// `None` before the first check; after that `"ok"` or `"stale"`.
    /// (Daemon <0.6 sent `""` instead of null; both collapse to `None`.)
    #[serde(default, deserialize_with = "empty_as_none")]
    pub last_state: Option<String>,
    pub last_write: String,
    pub last_alerted_at: String,
    pub last_error: String,
}

impl BorgBoxAlert {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn last_check_date(&self) -> Option<DateTime<Utc>> {
        parse_rfc3339(&self.last_check_at)
    }

    pub fn last_write_date(&self) -> Option<DateTime<Utc>> {
        parse_rfc3339(&self.last_write)
    }

    pub fn last_alerted_date(&self) -> Option<DateTime<Utc>> {
        parse_rfc3339(&self.last_alerted_at)
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BorgBoxKey {
    pub fingerprint: String,
    pub comment: Option<String>,
    pub algo: Option<String>,
    pub added_at: Option<String>,
    pub last_used_at: Option<String>,
}

impl BorgBoxKey {
    pub fn id(&self) -> &str {
        &self.fingerprint
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BorgBoxAddedKey {
    pub fingerprint: String,
}

/// Arguments of [`update_schedule`]: any field left `None` is omitted from
/// the PATCH body, so the daemon keeps the current value.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct BorgBoxScheduleUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cadence: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hour: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minute: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weekday: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub day: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// Arguments of [`update_alert`]: any field left `None` is omitted from the
/// PATCH body, so the daemon keeps the current value.
///
/// `secret` has three states on the wire: `None` (omit → leave unchanged),
/// `Some("")` (clear the secret → stop signing), or a non-empty string (set
/// or rotate).
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct BorgBoxAlertUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale_after_hours: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webhook_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub renotify_hours: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

fn empty_as_none<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    Ok(Option::<String>::deserialize(deserializer)?.filter(|s| !s.is_empty()))
}

/// RFC3339 with or without fractional seconds; `None` for `""` / garbage.
fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    if s.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

// MARK: - Endpoints

/// Hits `/health` with no auth. Used at server-add time to validate the URL.
pub fn health(daemon_url: &str) -> Result<(), String> {
    let url = endpoint(daemon_url, "health")?;
    let (status, _) = send("GET", &url, &[], None)?;
    // Like the Swift client, the body is ignored here.
    check_http(status, &[])
}

/// Hits `/info` with auth. Used at server-add time to validate the token.
pub fn info(daemon_url: &str, token: &str) -> Result<BorgBoxInfo, String> {
    get("info", daemon_url, token, &[])
}

pub fn list_repos(server: &BorgBoxServer) -> Result<Vec<BorgBoxRemoteRepo>, String> {
    let token = token_or_err(server)?;
    get("repos", &server.daemon_url, &token, &[])
}

#[derive(Serialize)]
struct CreateRepoBody<'a> {
    name: &'a str,
    pubkey: &'a str,
}

pub fn create_repo(
    server: &BorgBoxServer,
    name: &str,
    pubkey: &str,
) -> Result<BorgBoxRemoteRepo, String> {
    let token = token_or_err(server)?;
    post("repos", &server.daemon_url, &token, &CreateRepoBody { name, pubkey })
}

/// Adopts an existing repo directory under the daemon's repo root by adding a
/// pubkey line to `authorized_keys`. Requires v0.3.0+. Does NOT run borg
/// init — the repo must already be a valid borg repo.
pub fn import_repo(
    server: &BorgBoxServer,
    name: &str,
    pubkey: &str,
) -> Result<BorgBoxRemoteRepo, String> {
    let token = token_or_err(server)?;
    post("repos/import", &server.daemon_url, &token, &CreateRepoBody { name, pubkey })
}

pub fn repo_info(server: &BorgBoxServer, repo: &str) -> Result<BorgBoxRemoteRepo, String> {
    let token = token_or_err(server)?;
    let path = format!("repos/{}", encode_segment(repo));
    get(&path, &server.daemon_url, &token, &[])
}

pub fn delete_repo(server: &BorgBoxServer, repo: &str) -> Result<(), String> {
    let token = token_or_err(server)?;
    let path = format!("repos/{}", encode_segment(repo));
    delete_request(&path, &server.daemon_url, &token)
}

// MARK: - System / sessions

pub fn system_stats(server: &BorgBoxServer) -> Result<BorgBoxSystemStats, String> {
    let token = token_or_err(server)?;
    get("system/stats", &server.daemon_url, &token, &[])
}

pub fn sessions(server: &BorgBoxServer) -> Result<Vec<BorgBoxSession>, String> {
    let token = token_or_err(server)?;
    get("sessions", &server.daemon_url, &token, &[])
}

// MARK: - Maintenance jobs

#[derive(Serialize)]
struct CheckBody<'a> {
    repair: bool,
    verify_data: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    passphrase: Option<&'a str>,
}

/// Swift defaults: `repair = false`, `verify_data = false`.
pub fn check(
    server: &BorgBoxServer,
    repo: &str,
    passphrase: Option<&str>,
    repair: bool,
    verify_data: bool,
) -> Result<BorgBoxJobStart, String> {
    let token = token_or_err(server)?;
    let path = format!("repos/{}/check", encode_segment(repo));
    let body = CheckBody { repair, verify_data, passphrase };
    post(&path, &server.daemon_url, &token, &body)
}

pub fn job(server: &BorgBoxServer, job_id: &str) -> Result<BorgBoxJob, String> {
    let token = token_or_err(server)?;
    let path = format!("jobs/{}", encode_segment(job_id));
    get(&path, &server.daemon_url, &token, &[])
}

pub fn list_jobs(server: &BorgBoxServer) -> Result<Vec<BorgBoxJob>, String> {
    let token = token_or_err(server)?;
    get("jobs", &server.daemon_url, &token, &[])
}

#[derive(Serialize)]
struct PruneBody<'a> {
    keep_daily: i64,
    keep_weekly: i64,
    keep_monthly: i64,
    keep_yearly: i64,
    dry_run: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    passphrase: Option<&'a str>,
}

#[allow(clippy::too_many_arguments)]
pub fn prune(
    server: &BorgBoxServer,
    repo: &str,
    passphrase: Option<&str>,
    keep_daily: i64,
    keep_weekly: i64,
    keep_monthly: i64,
    keep_yearly: i64,
    dry_run: bool,
) -> Result<BorgBoxJobStart, String> {
    let token = token_or_err(server)?;
    let path = format!("repos/{}/prune", encode_segment(repo));
    let body = PruneBody {
        keep_daily,
        keep_weekly,
        keep_monthly,
        keep_yearly,
        dry_run,
        passphrase,
    };
    post(&path, &server.daemon_url, &token, &body)
}

#[derive(Serialize)]
struct CompactBody<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    passphrase: Option<&'a str>,
}

pub fn compact(
    server: &BorgBoxServer,
    repo: &str,
    passphrase: Option<&str>,
) -> Result<BorgBoxJobStart, String> {
    let token = token_or_err(server)?;
    let path = format!("repos/{}/compact", encode_segment(repo));
    post(&path, &server.daemon_url, &token, &CompactBody { passphrase })
}

pub fn break_lock(server: &BorgBoxServer, repo: &str) -> Result<(), String> {
    let token = token_or_err(server)?;
    let path = format!("repos/{}/break-lock", encode_segment(repo));
    let url = endpoint(&server.daemon_url, &path)?;
    let auth = bearer(&token);
    let (status, data) = send("POST", &url, &[("Authorization", &auth)], None)?;
    check_http(status, &data)
}

// MARK: - Remote archives

/// Lists the archives of a repo through the daemon. The passphrase, when
/// given, travels in the `X-Borg-Passphrase` header.
pub fn remote_archives(
    server: &BorgBoxServer,
    repo: &str,
    passphrase: Option<&str>,
) -> Result<Vec<BorgBoxRemoteArchive>, String> {
    let token = token_or_err(server)?;
    let path = format!("repos/{}/archives", encode_segment(repo));
    let mut extra: Vec<(&str, &str)> = Vec::new();
    if let Some(passphrase) = passphrase {
        extra.push(("X-Borg-Passphrase", passphrase));
    }
    get(&path, &server.daemon_url, &token, &extra)
}

// MARK: - Per-repo SSH keys

pub fn list_keys(server: &BorgBoxServer, repo: &str) -> Result<Vec<BorgBoxKey>, String> {
    let token = token_or_err(server)?;
    let path = format!("repos/{}/keys", encode_segment(repo));
    get(&path, &server.daemon_url, &token, &[])
}

#[derive(Serialize)]
struct AddKeyBody<'a> {
    public_key: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    comment: Option<&'a str>,
}

pub fn add_key(
    server: &BorgBoxServer,
    repo: &str,
    public_key: &str,
    comment: Option<&str>,
) -> Result<BorgBoxAddedKey, String> {
    let token = token_or_err(server)?;
    let path = format!("repos/{}/keys", encode_segment(repo));
    post(&path, &server.daemon_url, &token, &AddKeyBody { public_key, comment })
}

pub fn delete_key(server: &BorgBoxServer, repo: &str, fingerprint: &str) -> Result<(), String> {
    let token = token_or_err(server)?;
    let path = format!(
        "repos/{}/keys/{}",
        encode_segment(repo),
        encode_segment(fingerprint)
    );
    delete_request(&path, &server.daemon_url, &token)
}

// MARK: - Schedules (daemon scheduler, v0.4.0+)

pub fn list_schedules(server: &BorgBoxServer) -> Result<Vec<BorgBoxSchedule>, String> {
    let token = token_or_err(server)?;
    get("schedules", &server.daemon_url, &token, &[])
}

pub fn schedules_for_repo(
    server: &BorgBoxServer,
    repo: &str,
) -> Result<Vec<BorgBoxSchedule>, String> {
    let token = token_or_err(server)?;
    let path = format!("repos/{}/schedules", encode_segment(repo));
    get(&path, &server.daemon_url, &token, &[])
}

pub fn get_schedule(server: &BorgBoxServer, id: &str) -> Result<BorgBoxSchedule, String> {
    let token = token_or_err(server)?;
    let path = format!("schedules/{}", encode_segment(id));
    get(&path, &server.daemon_url, &token, &[])
}

#[allow(clippy::too_many_arguments)]
pub fn create_schedule(
    server: &BorgBoxServer,
    repo: &str,
    kind: &str,
    cadence: &str,
    hour: i64,
    minute: i64,
    weekday: i64,
    day: i64,
    enabled: bool,
) -> Result<BorgBoxSchedule, String> {
    let token = token_or_err(server)?;
    let path = format!("repos/{}/schedules", encode_segment(repo));
    let body = BorgBoxScheduleUpdate {
        kind: Some(kind.to_owned()),
        cadence: Some(cadence.to_owned()),
        hour: Some(hour),
        minute: Some(minute),
        weekday: Some(weekday),
        day: Some(day),
        enabled: Some(enabled),
    };
    post(&path, &server.daemon_url, &token, &body)
}

/// Partial update. Any field of `update` left `None` is omitted from the
/// body, so the daemon keeps the current value. When cadence/hour/minute/
/// weekday/day change the server recalculates `next_run`; a bare `enabled`
/// toggle does not, which is deliberate so disabling/re-enabling doesn't
/// reset the schedule.
pub fn update_schedule(
    server: &BorgBoxServer,
    id: &str,
    update: &BorgBoxScheduleUpdate,
) -> Result<BorgBoxSchedule, String> {
    let token = token_or_err(server)?;
    let path = format!("schedules/{}", encode_segment(id));
    patch(&path, &server.daemon_url, &token, update)
}

pub fn delete_schedule(server: &BorgBoxServer, id: &str) -> Result<(), String> {
    let token = token_or_err(server)?;
    let path = format!("schedules/{}", encode_segment(id));
    delete_request(&path, &server.daemon_url, &token)
}

// MARK: - Stale alerts

pub fn list_alerts(server: &BorgBoxServer) -> Result<Vec<BorgBoxAlert>, String> {
    let token = token_or_err(server)?;
    get("alerts", &server.daemon_url, &token, &[])
}

pub fn alerts_for_repo(server: &BorgBoxServer, repo: &str) -> Result<Vec<BorgBoxAlert>, String> {
    let token = token_or_err(server)?;
    let path = format!("repos/{}/alerts", encode_segment(repo));
    get(&path, &server.daemon_url, &token, &[])
}

pub fn get_alert(server: &BorgBoxServer, id: &str) -> Result<BorgBoxAlert, String> {
    let token = token_or_err(server)?;
    let path = format!("alerts/{}", encode_segment(id));
    get(&path, &server.daemon_url, &token, &[])
}

/// Creates a stale alert. A `secret` of `None` or `Some("")` means "no
/// signing" — the field is not sent at all in either case.
pub fn create_alert(
    server: &BorgBoxServer,
    repo: &str,
    stale_after_hours: i64,
    webhook_url: &str,
    enabled: bool,
    renotify_hours: i64,
    secret: Option<&str>,
) -> Result<BorgBoxAlert, String> {
    let token = token_or_err(server)?;
    let path = format!("repos/{}/alerts", encode_segment(repo));
    let body = BorgBoxAlertUpdate {
        stale_after_hours: Some(stale_after_hours),
        webhook_url: Some(webhook_url.to_owned()),
        enabled: Some(enabled),
        renotify_hours: Some(renotify_hours),
        secret: secret.filter(|s| !s.is_empty()).map(str::to_owned),
    };
    post(&path, &server.daemon_url, &token, &body)
}

/// Partial update; see [`BorgBoxAlertUpdate`] for the `secret` semantics.
pub fn update_alert(
    server: &BorgBoxServer,
    id: &str,
    update: &BorgBoxAlertUpdate,
) -> Result<BorgBoxAlert, String> {
    let token = token_or_err(server)?;
    let path = format!("alerts/{}", encode_segment(id));
    patch(&path, &server.daemon_url, &token, update)
}

pub fn delete_alert(server: &BorgBoxServer, id: &str) -> Result<(), String> {
    let token = token_or_err(server)?;
    let path = format!("alerts/{}", encode_segment(id));
    delete_request(&path, &server.daemon_url, &token)
}

/// Synchronously fires one `event: "test"` delivery against the configured
/// webhook. Returns on 2xx (`{"status":"ok"}`); fails with `"BorgBox: <reason>"`
/// carrying the daemon's reason string when the webhook rejects (e.g.
/// `"webhook returned 404"`).
pub fn test_alert(server: &BorgBoxServer, id: &str) -> Result<(), String> {
    #[derive(Deserialize)]
    struct AlertTestResponse {
        #[serde(rename = "status")]
        _status: String,
    }
    let token = token_or_err(server)?;
    let path = format!("alerts/{}/test", encode_segment(id));
    let url = endpoint(&server.daemon_url, &path)?;
    let auth = bearer(&token);
    let headers = [("Authorization", auth.as_str()), ("Accept", "application/json")];
    let (status, data) = send("POST", &url, &headers, None)?;
    check_http(status, &data)?;
    decode::<AlertTestResponse>(&data).map(|_| ())
}

// MARK: - Repo append-only toggle

/// PATCHes an existing repo's metadata. Currently only exposes the
/// `append_only` flag.
pub fn update_repo(
    server: &BorgBoxServer,
    repo: &str,
    append_only: bool,
) -> Result<BorgBoxRemoteRepo, String> {
    #[derive(Serialize)]
    struct RepoUpdateBody {
        append_only: bool,
    }
    let token = token_or_err(server)?;
    let path = format!("repos/{}", encode_segment(repo));
    patch(&path, &server.daemon_url, &token, &RepoUpdateBody { append_only })
}

// MARK: - Job stream (SSE)

/// Incremental parser for the daemon's SSE stream, fed one line at a time
/// (line terminator already stripped).
#[derive(Debug)]
struct SseParser {
    current_event: String,
    data_lines: Vec<String>,
}

impl SseParser {
    fn new() -> Self {
        SseParser {
            current_event: "message".to_owned(),
            data_lines: Vec::new(),
        }
    }

    /// Returns `(event, finished)`: `event` is the payload dispatched by this
    /// line (only blank lines dispatch), `finished` is true once the block
    /// tagged `event: end` has been terminated.
    fn feed(&mut self, line: &str) -> (Option<BorgBoxJobStreamEvent>, bool) {
        if line.is_empty() {
            // Blank line dispatches the accumulated event. Payloads that are
            // not valid JSON for our event shape are silently dropped.
            let event = if self.data_lines.is_empty() {
                None
            } else {
                serde_json::from_str::<BorgBoxJobStreamEvent>(&self.data_lines.join("\n")).ok()
            };
            let finished = self.current_event == "end";
            self.current_event = "message".to_owned();
            self.data_lines.clear();
            return (event, finished);
        }
        if line.starts_with(':') {
            return (None, false); // heartbeat / comment
        }
        if let Some((field, value)) = line.split_once(':') {
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => self.current_event = value.to_owned(),
                "data" => self.data_lines.push(value.to_owned()),
                _ => {}
            }
        }
        (None, false)
    }
}

/// How often `stream_job` re-checks the cancel flag while the stream is idle.
const STREAM_CANCEL_POLL: Duration = Duration::from_millis(100);

/// Long jobs (e.g. check on big repos) can stay silent for a long time; like
/// the Swift client, only give up after an hour without any byte.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(3600);

/// Subscribes to `/jobs/{id}/stream` (SSE, daemon v0.3.0+) and blocks, calling
/// `on_event` (on the calling thread) once per SSE block, until the daemon
/// sends `event: end` or closes the stream (`Ok`), `cancel` becomes true
/// (`Ok`), or an HTTP/transport error occurs (`Err`).
///
/// Cancellation is noticed within ~100 ms, including while connecting: the
/// network I/O runs on a helper thread that feeds lines back over a channel.
/// After a cancel that helper thread (and its connection) lingers until the
/// daemon sends its next line/heartbeat or closes the stream — at worst until
/// the one hour idle timeout — and then exits on its own.
pub fn stream_job(
    server: &BorgBoxServer,
    job_id: &str,
    cancel: &AtomicBool,
    on_event: &mut dyn FnMut(BorgBoxJobStreamEvent),
) -> Result<(), String> {
    let token = keyring::borgbox_token(&server.id).ok_or_else(|| ERR_NO_TOKEN.to_owned())?;
    let path = format!("jobs/{}/stream", encode_segment(job_id));
    let url = endpoint(&server.daemon_url, &path)?;
    let auth = bearer(&token);
    if cancel.load(Ordering::Relaxed) {
        return Ok(());
    }

    let (tx, rx) = mpsc::channel::<Result<String, String>>();
    std::thread::Builder::new()
        .name("borgbox-sse".to_owned())
        .spawn(move || stream_reader(url, auth, tx))
        .map_err(|e| e.to_string())?;

    let mut parser = SseParser::new();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        match rx.recv_timeout(STREAM_CANCEL_POLL) {
            Ok(Ok(line)) => {
                let (event, finished) = parser.feed(&line);
                if let Some(event) = event {
                    on_event(event);
                }
                if finished {
                    return Ok(());
                }
            }
            Ok(Err(message)) => return Err(message),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

/// Helper-thread body of `stream_job`: performs the request and forwards the
/// response line by line. Stops as soon as the receiver is gone.
fn stream_reader(url: String, auth: String, tx: mpsc::Sender<Result<String, String>>) {
    let agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(REQUEST_TIMEOUT))
        .timeout_per_read(Some(STREAM_IDLE_TIMEOUT))
        .timeout_per_write(Some(REQUEST_TIMEOUT))
        .build()
        .new_agent();
    let headers = [("Authorization", auth.as_str()), ("Accept", "text/event-stream")];
    let response = match run(&agent, "GET", &url, &headers, None) {
        Ok(response) => response,
        Err(message) => {
            let _ = tx.send(Err(message));
            return;
        }
    };
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let _ = tx.send(Err(err_http(status, "")));
        return;
    }

    let mut reader = BufReader::new(response.into_body().into_reader());
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => return, // end of stream
            Ok(_) => {
                // A trailing fragment without a newline (stream cut mid-line)
                // is still forwarded, like `URLSession.AsyncBytes.lines`.
                if buf.last() == Some(&b'\n') {
                    buf.pop();
                }
                if buf.last() == Some(&b'\r') {
                    buf.pop();
                }
                let line = String::from_utf8_lossy(&buf).into_owned();
                if tx.send(Ok(line)).is_err() {
                    return; // stream_job returned (cancelled or finished)
                }
            }
            Err(e) => {
                let _ = tx.send(Err(e.to_string()));
                return;
            }
        }
    }
}

// MARK: - Token helpers

fn token_or_err(server: &BorgBoxServer) -> Result<String, String> {
    keyring::borgbox_token(&server.id).ok_or_else(|| ERR_NO_TOKEN.to_owned())
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

/// Percent-encodes everything outside Foundation's `urlPathAllowed` set
/// (ASCII alphanumerics and `!$&'()*+,-./:=@_~`). Note that `/` is kept.
fn encode_segment(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || b"!$&'()*+,-./:=@_~".contains(&b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
    }
    out
}

// MARK: - HTTP plumbing

/// Same as URLSession's default request timeout (60 s without progress).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Builds `<daemon_url>/api/v1/<path>`, keeping any path prefix (and query)
/// already present in the daemon URL. `path` must already be percent-encoded.
fn endpoint(daemon_url: &str, path: &str) -> Result<String, String> {
    let invalid = || ERR_INVALID_URL.to_owned();
    if daemon_url
        .chars()
        .any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(invalid());
    }
    let (scheme, rest) = daemon_url.split_once("://").ok_or_else(invalid)?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return Err(invalid());
    }
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    if authority.is_empty() {
        return Err(invalid());
    }
    let tail = tail.split('#').next().unwrap_or("");
    let (existing_path, query) = match tail.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (tail, None),
    };
    let existing = existing_path.trim_matches('/');
    let segments: Vec<&str> = [existing, "api", "v1", path]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();
    let mut url = format!("{scheme}://{authority}/{}", segments.join("/"));
    if let Some(query) = query {
        url.push('?');
        url.push_str(query);
    }
    if url.parse::<ureq::http::Uri>().is_err() {
        return Err(invalid());
    }
    Ok(url)
}

fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(REQUEST_TIMEOUT))
            .timeout_per_read(Some(REQUEST_TIMEOUT))
            .timeout_per_write(Some(REQUEST_TIMEOUT))
            .build()
            .new_agent()
    })
}

/// Sends one request and returns the raw response (any status).
fn run(
    agent: &ureq::Agent,
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<String>,
) -> Result<ureq::http::Response<ureq::Body>, String> {
    use ureq::http::{HeaderValue, Request};

    let mut builder = Request::builder().method(method).uri(url);
    for (name, value) in headers {
        // from_bytes (unlike from_str) tolerates non-ASCII, e.g. passphrases.
        let value = HeaderValue::from_bytes(value.as_bytes())
            .map_err(|_| format!("Invalid characters in the {name} header value."))?;
        builder = builder.header(*name, value);
    }
    let result = match body {
        Some(body) => {
            let request = builder.body(body).map_err(|e| e.to_string())?;
            agent.run(request)
        }
        None => {
            let request = builder.body(()).map_err(|e| e.to_string())?;
            agent.run(request)
        }
    };
    result.map_err(|e| match e {
        ureq::Error::BadUri(_) => ERR_INVALID_URL.to_owned(),
        other => other.to_string(),
    })
}

/// Sends one request with the shared agent and returns `(status, body)`.
fn send(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<String>,
) -> Result<(u16, Vec<u8>), String> {
    let mut response = run(agent(), method, url, headers, body)?;
    let status = response.status().as_u16();
    let data = response
        .body_mut()
        .with_config()
        .limit(256 * 1024 * 1024)
        .read_to_vec()
        .map_err(|e| e.to_string())?;
    Ok((status, data))
}

fn get<T: DeserializeOwned>(
    path: &str,
    daemon_url: &str,
    token: &str,
    extra_headers: &[(&str, &str)],
) -> Result<T, String> {
    let url = endpoint(daemon_url, path)?;
    let auth = bearer(token);
    let mut headers = vec![("Authorization", auth.as_str()), ("Accept", "application/json")];
    headers.extend_from_slice(extra_headers);
    let (status, data) = send("GET", &url, &headers, None)?;
    check_http(status, &data)?;
    decode(&data)
}

fn send_json<B: Serialize, T: DeserializeOwned>(
    method: &str,
    path: &str,
    daemon_url: &str,
    token: &str,
    body: &B,
) -> Result<T, String> {
    let url = endpoint(daemon_url, path)?;
    let auth = bearer(token);
    let headers = [
        ("Authorization", auth.as_str()),
        ("Content-Type", "application/json"),
        ("Accept", "application/json"),
    ];
    let body = serde_json::to_string(body).map_err(|e| e.to_string())?;
    let (status, data) = send(method, &url, &headers, Some(body))?;
    check_http(status, &data)?;
    decode(&data)
}

fn post<B: Serialize, T: DeserializeOwned>(
    path: &str,
    daemon_url: &str,
    token: &str,
    body: &B,
) -> Result<T, String> {
    send_json("POST", path, daemon_url, token, body)
}

fn patch<B: Serialize, T: DeserializeOwned>(
    path: &str,
    daemon_url: &str,
    token: &str,
    body: &B,
) -> Result<T, String> {
    send_json("PATCH", path, daemon_url, token, body)
}

/// DELETE with no response body.
fn delete_request(path: &str, daemon_url: &str, token: &str) -> Result<(), String> {
    let url = endpoint(daemon_url, path)?;
    let auth = bearer(token);
    let (status, data) = send("DELETE", &url, &[("Authorization", &auth)], None)?;
    check_http(status, &data)
}

fn decode<T: DeserializeOwned>(data: &[u8]) -> Result<T, String> {
    serde_json::from_slice(data).map_err(|e| {
        let preview: String = String::from_utf8_lossy(data).chars().take(200).collect();
        err_decode(&format!("{e}. Response: {preview}"))
    })
}

fn check_http(status: u16, data: &[u8]) -> Result<(), String> {
    #[derive(Deserialize)]
    struct ApiError {
        error: String,
    }
    if (200..300).contains(&status) {
        return Ok(());
    }
    if let Ok(api_err) = serde_json::from_slice::<ApiError>(data) {
        return Err(err_api(&api_err.error));
    }
    let text: String = String::from_utf8_lossy(data).chars().take(400).collect();
    Err(err_http(status, &text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_building() {
        assert_eq!(
            endpoint("http://nas.local:8080", "repos").unwrap(),
            "http://nas.local:8080/api/v1/repos"
        );
        assert_eq!(
            endpoint("https://nas.local/", "health").unwrap(),
            "https://nas.local/api/v1/health"
        );
        assert_eq!(
            endpoint("https://example.com/borgbox/", "repos/my%20repo/keys").unwrap(),
            "https://example.com/borgbox/api/v1/repos/my%20repo/keys"
        );
        assert_eq!(
            endpoint("http://[::1]:9000//x//", "jobs/abc/stream").unwrap(),
            "http://[::1]:9000/x/api/v1/jobs/abc/stream"
        );
        for bad in ["", "nas.local:8080", "ftp://nas", "http://", "http://a b", "http:///x"] {
            assert_eq!(endpoint(bad, "info"), Err(ERR_INVALID_URL.to_owned()), "{bad:?}");
        }
    }

    #[test]
    fn segment_encoding() {
        assert_eq!(encode_segment("laptop-01_a.b"), "laptop-01_a.b");
        assert_eq!(encode_segment("my repo"), "my%20repo");
        assert_eq!(encode_segment("SHA256:ab+c/d="), "SHA256:ab+c/d=");
        assert_eq!(encode_segment("a?b#c%d"), "a%3Fb%23c%25d");
        assert_eq!(encode_segment("é"), "%C3%A9");
    }

    #[test]
    fn http_error_mapping() {
        assert_eq!(check_http(204, b""), Ok(()));
        assert_eq!(
            check_http(409, br#"{"error":"repo exists"}"#),
            Err("BorgBox: repo exists".to_owned())
        );
        assert_eq!(
            check_http(502, b"Bad Gateway"),
            Err("BorgBox returned HTTP 502: Bad Gateway".to_owned())
        );
        assert_eq!(check_http(500, b""), Err("BorgBox returned HTTP 500: ".to_owned()));
        let err = decode::<BorgBoxJobStart>(br#"{"status":"queued"}"#).unwrap_err();
        assert!(err.starts_with("Could not decode the daemon response: missing field `job_id`"));
        assert!(err.ends_with(r#"Response: {"status":"queued"}"#));
    }

    #[test]
    fn repo_and_job_decoding() {
        let repo: BorgBoxRemoteRepo = serde_json::from_str(
            r#"{"name":"mac","path":"/srv/borg/mac","size_bytes":123,"initialized":true,
                "modified_at":"2026-04-18T10:00:00Z","ssh_url":"ssh://borg@nas:22/srv/borg/mac",
                "registered":false,"append_only":true,"future_field":1}"#,
        )
        .unwrap();
        assert_eq!(repo.id(), "mac");
        assert!(!repo.is_registered());
        assert_eq!(repo.append_only, Some(true));

        let old: BorgBoxRemoteRepo =
            serde_json::from_str(r#"{"name":"mac","path":"/srv/borg/mac"}"#).unwrap();
        assert!(old.is_registered());
        assert_eq!(old.size_bytes, None);

        let job: BorgBoxJob = serde_json::from_str(
            r#"{"id":"j1","repo":null,"kind":"check","status":"done","exit_code":2,"log_tail":["a","b"]}"#,
        )
        .unwrap();
        assert!(job.is_terminal());
        assert!(job.is_failure());
        let running: BorgBoxJob =
            serde_json::from_str(r#"{"id":"j2","kind":"prune","status":"running"}"#).unwrap();
        assert!(!running.is_terminal());
        assert!(!running.is_failure());

        let stats: BorgBoxSystemStats = serde_json::from_str(
            r#"{"hostname":"nas","uptime_seconds":99,"load":[0.1,1,2.5],
                "storage":{"path":"/srv","total_bytes":10,"used_bytes":4,"free_bytes":6}}"#,
        )
        .unwrap();
        assert_eq!(stats.load, vec![0.1, 1.0, 2.5]);
        assert_eq!(stats.storage.free_bytes, 6);
        assert_eq!(stats.borgbox_version, None);
    }

    #[test]
    fn alert_and_schedule_decoding() {
        let base = r#""id":"a1","repo":"mac","stale_after_hours":48,
            "webhook_url":"https://hooks.example/x","enabled":true,"renotify_hours":0,
            "created_at":"2026-04-18T10:00:00Z","last_check_at":"2026-04-18T11:00:00.5Z",
            "last_write":"","last_alerted_at":"","last_error":"""#;

        let old: BorgBoxAlert =
            serde_json::from_str(&format!("{{{base},\"last_state\":\"\"}}")).unwrap();
        assert!(!old.has_secret);
        assert_eq!(old.last_state, None);
        assert_eq!(old.last_write_date(), None);
        assert_eq!(old.last_check_date().unwrap().timestamp(), 1776510000);

        let new: BorgBoxAlert = serde_json::from_str(&format!(
            "{{{base},\"last_state\":\"stale\",\"has_secret\":true}}"
        ))
        .unwrap();
        assert!(new.has_secret);
        assert_eq!(new.last_state.as_deref(), Some("stale"));

        let null: BorgBoxAlert =
            serde_json::from_str(&format!("{{{base},\"last_state\":null}}")).unwrap();
        assert_eq!(null.last_state, None);
        let missing: BorgBoxAlert = serde_json::from_str(&format!("{{{base}}}")).unwrap();
        assert_eq!(missing.last_state, None);

        let schedule: BorgBoxSchedule = serde_json::from_str(
            r#"{"id":"s1","repo":"mac","kind":"check","cadence":"weekly","hour":3,"minute":30,
                "weekday":0,"day":0,"enabled":true,"created_at":"2026-04-18T10:00:00Z",
                "last_run":"","last_status":"","last_job_id":"","next_run":"2026-04-19T03:30:00Z"}"#,
        )
        .unwrap();
        assert_eq!(schedule.last_run_date(), None);
        assert_eq!(schedule.next_run_date().unwrap().to_rfc3339(), "2026-04-19T03:30:00+00:00");
    }

    #[test]
    fn request_bodies() {
        let body = CheckBody { repair: false, verify_data: true, passphrase: None };
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            r#"{"repair":false,"verify_data":true}"#
        );
        assert_eq!(serde_json::to_string(&CompactBody { passphrase: None }).unwrap(), "{}");
        assert_eq!(
            serde_json::to_string(&CompactBody { passphrase: Some("s") }).unwrap(),
            r#"{"passphrase":"s"}"#
        );
        let toggle = BorgBoxScheduleUpdate { enabled: Some(false), ..Default::default() };
        assert_eq!(serde_json::to_string(&toggle).unwrap(), r#"{"enabled":false}"#);
        let clear = BorgBoxAlertUpdate { secret: Some(String::new()), ..Default::default() };
        assert_eq!(serde_json::to_string(&clear).unwrap(), r#"{"secret":""}"#);
    }

    #[test]
    fn sse_parsing() {
        let stream = [
            ": heartbeat",
            "event: log",
            r#"data: {"type":"log","line":"Starting check"}"#,
            "",
            "",
            "data:{\"type\":\"log\",",
            "data: \"line\":\"split\"}",
            "ignored line without colon",
            "id: 7",
            "",
            "data: not json",
            "",
            "event: end",
            r#"data: {"type":"end","status":"done","exit_code":0}"#,
            "",
            r#"data: {"type":"log","line":"never reached"}"#,
            "",
        ];
        let mut parser = SseParser::new();
        let mut events = Vec::new();
        let mut finished_at = None;
        for (i, line) in stream.iter().enumerate() {
            let (event, finished) = parser.feed(line);
            events.extend(event);
            if finished {
                finished_at = Some(i);
                break;
            }
        }
        assert_eq!(finished_at, Some(14));
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].event_type, "log");
        assert_eq!(events[0].line.as_deref(), Some("Starting check"));
        assert_eq!(events[1].line.as_deref(), Some("split"));
        assert_eq!(
            events[2],
            BorgBoxJobStreamEvent {
                event_type: "end".to_owned(),
                line: None,
                status: Some("done".to_owned()),
                exit_code: Some(0),
            }
        );
    }
}
