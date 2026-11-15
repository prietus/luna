//! BorgBase GraphQL API client (port of `BorgBaseClient.swift`).
//!
//! Everything here is blocking; call it from a worker thread. Every public
//! function returns `Result<T, String>` where the error is the human-readable
//! message (same text as the Swift `BorgBaseError.errorDescription`).

#![allow(dead_code)]

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use chrono::{DateTime, NaiveDateTime, Utc};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::core::keyring;

// MARK: - Errors

pub const ERR_NO_TOKEN: &str = "No BorgBase API token configured.";
pub const ERR_INVALID_RESPONSE: &str = "Invalid response from the BorgBase server.";
pub const ERR_UNAUTHENTICATED: &str =
    "The BorgBase API token is no longer valid. Paste a new one.";

/// True when `err` is the "token rejected" error, so the UI can bounce the
/// user back to the token entry screen (Swift: `BorgBaseError.unauthenticated`).
pub fn is_unauthenticated(err: &str) -> bool {
    err.starts_with(ERR_UNAUTHENTICATED)
}

/// True when `err` is the "no token configured" error (Swift: `.noToken`).
pub fn is_no_token(err: &str) -> bool {
    err == ERR_NO_TOKEN
}

fn err_http(code: u16, body: &str) -> String {
    format!("BorgBase returned HTTP {code}: {body}")
}

fn err_graphql(msg: &str) -> String {
    format!("BorgBase GraphQL: {msg}")
}

// MARK: - Models

/// A BorgBase repository. Decoding is lenient like the Swift type: only `id`
/// is required, every other field silently becomes `None` (or `""` for
/// `name`) when missing, null or of the wrong type.
#[derive(Debug, Clone, PartialEq)]
pub struct BorgBaseRepo {
    pub id: String,
    pub name: String,
    pub repo_path: Option<String>,
    pub current_usage: Option<f64>,
    pub quota: Option<f64>,
    pub quota_enabled: Option<bool>,
    pub encryption: Option<String>,
    pub last_modified: Option<String>,
    pub created_at: Option<String>,
    pub alert_days: Option<i64>,
    /// Flattened from the nested `server { region }` object.
    pub region: Option<String>,
    pub compaction_enabled: Option<bool>,
    pub compaction_interval: Option<i64>,
    pub compaction_interval_unit: Option<String>,
    pub compaction_hour: Option<i64>,
    pub compaction_hour_timezone: Option<String>,
    pub full_access_keys: Option<Vec<String>>,
    pub append_only_keys: Option<Vec<String>>,
}

fn v_string(obj: &Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key)?.as_str().map(str::to_owned)
}

fn v_f64(obj: &Map<String, Value>, key: &str) -> Option<f64> {
    obj.get(key)?.as_f64()
}

fn v_bool(obj: &Map<String, Value>, key: &str) -> Option<bool> {
    obj.get(key)?.as_bool()
}

fn v_int(obj: &Map<String, Value>, key: &str) -> Option<i64> {
    let v = obj.get(key)?;
    if let Some(i) = v.as_i64() {
        return Some(i);
    }
    // Swift's JSONDecoder accepts integral floats (e.g. `3.0`) as Int.
    let f = v.as_f64()?;
    (f.is_finite() && f.fract() == 0.0 && f.abs() < 9.0e15).then_some(f as i64)
}

fn v_strings(obj: &Map<String, Value>, key: &str) -> Option<Vec<String>> {
    obj.get(key)?
        .as_array()?
        .iter()
        .map(|v| v.as_str().map(str::to_owned))
        .collect()
}

impl<'de> Deserialize<'de> for BorgBaseRepo {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let value = Value::deserialize(deserializer)?;
        let obj = value
            .as_object()
            .ok_or_else(|| D::Error::custom("expected a repository object"))?;
        let id = v_string(obj, "id").ok_or_else(|| D::Error::missing_field("id"))?;
        let region = obj
            .get("server")
            .and_then(Value::as_object)
            .and_then(|server| v_string(server, "region"));
        Ok(BorgBaseRepo {
            id,
            name: v_string(obj, "name").unwrap_or_default(),
            repo_path: v_string(obj, "repoPath"),
            current_usage: v_f64(obj, "currentUsage"),
            quota: v_f64(obj, "quota"),
            quota_enabled: v_bool(obj, "quotaEnabled"),
            encryption: v_string(obj, "encryption"),
            last_modified: v_string(obj, "lastModified"),
            created_at: v_string(obj, "createdAt"),
            alert_days: v_int(obj, "alertDays"),
            region,
            compaction_enabled: v_bool(obj, "compactionEnabled"),
            compaction_interval: v_int(obj, "compactionInterval"),
            compaction_interval_unit: v_string(obj, "compactionIntervalUnit"),
            compaction_hour: v_int(obj, "compactionHour"),
            compaction_hour_timezone: v_string(obj, "compactionHourTimezone"),
            full_access_keys: v_strings(obj, "fullAccessKeys"),
            append_only_keys: v_strings(obj, "appendOnlyKeys"),
        })
    }
}

impl BorgBaseRepo {
    /// Parses BorgBase's `lastModified` ISO-8601 string. BorgBase returns
    /// timestamps with or without fractional seconds, and sometimes without a
    /// timezone (treated as UTC).
    pub fn last_modified_date(&self) -> Option<DateTime<Utc>> {
        let s = self.last_modified.as_deref()?;
        if s.is_empty() {
            return None;
        }
        if let Ok(d) = DateTime::parse_from_rfc3339(s) {
            return Some(d.with_timezone(&Utc));
        }
        // Some responses omit the timezone; treat as UTC.
        NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
            .ok()
            .map(|naive| naive.and_utc())
    }

    /// True when the repo hasn't been modified within its configured
    /// inactivity alert window. Returns false if either value is missing or
    /// alerts are disabled (`alert_days` is 0 / None).
    pub fn is_stale(&self) -> bool {
        let Some(days) = self.alert_days.filter(|d| *d > 0) else {
            return false;
        };
        let Some(date) = self.last_modified_date() else {
            return false;
        };
        let elapsed = Utc::now().signed_duration_since(date).num_milliseconds() as f64 / 1000.0;
        elapsed > days as f64 * 86400.0
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BorgBaseSSHKey {
    pub id: String,
    pub name: String,
    pub key_type: Option<String>,
    pub bits: Option<i64>,
    pub hash_md5: Option<String>,
    pub added_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BorgBaseAddedKey {
    pub id: String,
    pub hash_md5: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BorgBaseAddedRepo {
    pub id: String,
    pub name: String,
    pub repo_path: String,
}

/// Arguments of [`update_repo`]: a partial update, only the `Some` fields are
/// sent to the server so every other setting is preserved. `quota` is in
/// megabytes, matching what `BorgBaseRepo::quota` returns.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BorgBaseRepoUpdate {
    pub name: Option<String>,
    pub quota: Option<i64>,
    pub quota_enabled: Option<bool>,
    pub alert_days: Option<i64>,
    pub compaction_enabled: Option<bool>,
    pub compaction_interval: Option<i64>,
    pub compaction_interval_unit: Option<String>,
    pub compaction_hour: Option<i64>,
    pub compaction_hour_timezone: Option<String>,
    pub full_access_keys: Option<Vec<String>>,
    pub append_only_keys: Option<Vec<String>>,
}

// MARK: - Token

/// In-memory token cache so we hit the keyring at most once per app session.
static CACHED_TOKEN: Mutex<Option<String>> = Mutex::new(None);

fn cache() -> std::sync::MutexGuard<'static, Option<String>> {
    CACHED_TOKEN.lock().unwrap_or_else(|e| e.into_inner())
}

/// Caches `token` in memory for this session. Like the Swift `setToken`, this
/// does NOT write to the keyring; see [`save_token`] for that.
pub fn set_token(token: &str) {
    *cache() = Some(token.to_owned());
}

/// Drops the in-memory token. Like the Swift `clearToken`, this does NOT
/// delete the keyring entry; see [`forget_token`] for that.
pub fn clear_token() {
    *cache() = None;
}

/// Returns true if a token is available (in cache or keyring). Reads the
/// keyring at most once and caches the result.
pub fn has_token() -> bool {
    let mut cached = cache();
    if cached.is_some() {
        return true;
    }
    match keyring::borgbase_token() {
        Some(stored) if !stored.is_empty() => {
            *cached = Some(stored);
            true
        }
        _ => false,
    }
}

/// Convenience (not in the Swift client): stores the token in the keyring and
/// caches it.
pub fn save_token(token: &str) -> Result<(), String> {
    keyring::set_borgbase_token(token)?;
    set_token(token);
    Ok(())
}

/// Convenience (not in the Swift client): deletes the token from the keyring
/// and from the cache.
pub fn forget_token() {
    keyring::delete_borgbase_token();
    clear_token();
}

fn token() -> Result<String, String> {
    let mut cached = cache();
    if let Some(t) = cached.as_ref().filter(|t| !t.is_empty()) {
        return Ok(t.clone());
    }
    match keyring::borgbase_token() {
        Some(stored) if !stored.is_empty() => {
            *cached = Some(stored.clone());
            Ok(stored)
        }
        _ => Err(ERR_NO_TOKEN.to_owned()),
    }
}

// MARK: - API

const REPOS_QUERY: &str = "{
  repoList {
    id
    name
    repoPath
    currentUsage
    quota
    quotaEnabled
    encryption
    lastModified
    createdAt
    alertDays
    compactionEnabled
    compactionInterval
    compactionIntervalUnit
    compactionHour
    compactionHourTimezone
    fullAccessKeys
    appendOnlyKeys
    server { region }
  }
}";

const SSH_KEYS_QUERY: &str = "{
  sshList {
    id
    name
    keyType
    bits
    hashMd5
    addedAt
  }
}";

const SSH_ADD_MUTATION: &str = "mutation sshAdd($name: String!, $keyData: String!) {
  sshAdd(name: $name, keyData: $keyData) {
    keyAdded { id hashMd5 }
  }
}";

const REPO_ADD_MUTATION: &str = "mutation repoAdd(
  $name: String!,
  $quotaEnabled: Boolean!,
  $fullAccessKeys: [String]!,
  $region: String!
) {
  repoAdd(
    name: $name,
    quotaEnabled: $quotaEnabled,
    fullAccessKeys: $fullAccessKeys,
    region: $region
  ) {
    repoAdded { id name repoPath }
  }
}";

pub fn repos() -> Result<Vec<BorgBaseRepo>, String> {
    #[derive(Deserialize)]
    struct Response {
        #[serde(rename = "repoList")]
        repo_list: Vec<BorgBaseRepo>,
    }
    Ok(send::<Response>(REPOS_QUERY, None)?.repo_list)
}

pub fn ssh_keys() -> Result<Vec<BorgBaseSSHKey>, String> {
    #[derive(Deserialize)]
    struct Response {
        #[serde(rename = "sshList")]
        ssh_list: Vec<BorgBaseSSHKey>,
    }
    Ok(send::<Response>(SSH_KEYS_QUERY, None)?.ssh_list)
}

/// Uploads a new SSH public key to the user's BorgBase account. `key_data`
/// should be the contents of an `id_*.pub` file (full line, e.g.
/// `ssh-ed25519 AAAA... comment`). Requires a write-scoped token.
pub fn add_ssh_key(name: &str, key_data: &str) -> Result<BorgBaseAddedKey, String> {
    #[derive(Deserialize)]
    struct Inner {
        #[serde(rename = "keyAdded")]
        key_added: BorgBaseAddedKey,
    }
    #[derive(Deserialize)]
    struct Wrap {
        #[serde(rename = "sshAdd")]
        ssh_add: Inner,
    }
    let vars = json!({ "name": name, "keyData": key_data });
    Ok(send::<Wrap>(SSH_ADD_MUTATION, Some(vars))?.ssh_add.key_added)
}

/// Creates a new repository in BorgBase. Requires a write-scoped token.
/// `full_access_keys` is a list of SSH key ids (returned by [`add_ssh_key`])
/// that will have full read/write access to the new repo. The Swift defaults
/// are `quota_enabled = false`, `region = "eu"`.
pub fn create_repo(
    name: &str,
    full_access_keys: &[String],
    quota_enabled: bool,
    region: &str,
) -> Result<BorgBaseAddedRepo, String> {
    #[derive(Deserialize)]
    struct Inner {
        #[serde(rename = "repoAdded")]
        repo_added: BorgBaseAddedRepo,
    }
    #[derive(Deserialize)]
    struct Wrap {
        #[serde(rename = "repoAdd")]
        repo_add: Inner,
    }
    let vars = json!({
        "name": name,
        "quotaEnabled": quota_enabled,
        "fullAccessKeys": full_access_keys,
        "region": region,
    });
    Ok(send::<Wrap>(REPO_ADD_MUTATION, Some(vars))?.repo_add.repo_added)
}

/// Renames a repository. Convenience wrapper around [`update_repo`].
pub fn rename_repo(id: &str, new_name: &str) -> Result<(), String> {
    update_repo(
        id,
        &BorgBaseRepoUpdate {
            name: Some(new_name.to_owned()),
            ..Default::default()
        },
    )
}

/// Builds the `repoEdit` mutation and its variables; `None` when there is
/// nothing to change.
fn repo_edit_request(id: &str, update: &BorgBaseRepoUpdate) -> Option<(String, Value)> {
    let mut arg_decls = vec!["$id: String!".to_owned()];
    let mut arg_uses = vec!["id: $id".to_owned()];
    let mut vars = Map::new();
    vars.insert("id".to_owned(), json!(id));

    let mut add_var = |key: &str, value: Option<Value>, gql_type: &str| {
        if let Some(value) = value {
            arg_decls.push(format!("${key}: {gql_type}"));
            arg_uses.push(format!("{key}: ${key}"));
            vars.insert(key.to_owned(), value);
        }
    };

    let u = update;
    add_var("name", u.name.as_ref().map(|v| json!(v)), "String");
    add_var("quota", u.quota.map(|v| json!(v)), "Int");
    add_var("quotaEnabled", u.quota_enabled.map(|v| json!(v)), "Boolean");
    add_var("alertDays", u.alert_days.map(|v| json!(v)), "Int");
    add_var("compactionEnabled", u.compaction_enabled.map(|v| json!(v)), "Boolean");
    add_var("compactionInterval", u.compaction_interval.map(|v| json!(v)), "Int");
    add_var(
        "compactionIntervalUnit",
        u.compaction_interval_unit.as_ref().map(|v| json!(v)),
        "String",
    );
    add_var("compactionHour", u.compaction_hour.map(|v| json!(v)), "Int");
    add_var(
        "compactionHourTimezone",
        u.compaction_hour_timezone.as_ref().map(|v| json!(v)),
        "String",
    );
    add_var("fullAccessKeys", u.full_access_keys.as_ref().map(|v| json!(v)), "[String]");
    add_var("appendOnlyKeys", u.append_only_keys.as_ref().map(|v| json!(v)), "[String]");

    if arg_decls.len() <= 1 {
        return None;
    }
    let query = format!(
        "mutation repoEdit({}) {{\n  repoEdit({}) {{\n    __typename\n  }}\n}}",
        arg_decls.join(", "),
        arg_uses.join(", ")
    );
    Some((query, Value::Object(vars)))
}

/// Partial update of a repo on BorgBase. Only the `Some` fields of `update`
/// are sent to the server, so every other setting is preserved. A fully empty
/// update returns `Ok(())` without a round trip.
pub fn update_repo(id: &str, update: &BorgBaseRepoUpdate) -> Result<(), String> {
    // Nothing to change — save a round trip.
    let Some((query, vars)) = repo_edit_request(id, update) else {
        return Ok(());
    };
    send_expecting_object(&query, Some(vars), "repoEdit")
}

/// Triggers a server-side `borg compact` on the given repo. BorgBase runs the
/// operation asynchronously in its own infra — no job id comes back, just a
/// success envelope.
pub fn compact_repo(id: &str) -> Result<(), String> {
    let query = "mutation repoCompact($id: String!) {
  repoCompact(id: $id) {
    __typename
  }
}";
    send_expecting_object(query, Some(json!({ "id": id })), "repoCompact")
}

/// Deletes a repository permanently. There is no undo — the repo's data is
/// erased server-side. Requires a write-scoped token.
pub fn delete_repo(id: &str) -> Result<(), String> {
    let query = "mutation repoDelete($id: String!) {
  repoDelete(id: $id) {
    __typename
  }
}";
    send_expecting_object(query, Some(json!({ "id": id })), "repoDelete")
}

/// Deletes an SSH public key from the user's BorgBase account. Any repo whose
/// ACL referenced only this key will become unreachable.
pub fn delete_ssh_key(id: &str) -> Result<(), String> {
    let query = "mutation sshDelete($id: String!) {
  sshDelete(id: $id) {
    __typename
  }
}";
    send_expecting_object(query, Some(json!({ "id": id })), "sshDelete")
}

// MARK: - HTTP plumbing

const ENDPOINT: &str = "https://api.borgbase.com/graphql";

/// Debug builds can point the client at a local mock server.
fn endpoint() -> String {
    #[cfg(debug_assertions)]
    if let Ok(url) = std::env::var("LUNA_BORGBASE_URL") {
        return url;
    }
    ENDPOINT.to_owned()
}

/// Same as URLSession's default request timeout (60 s without progress).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

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

fn request_body(query: &str, variables: Option<Value>) -> String {
    let mut body = Map::new();
    body.insert("query".to_owned(), json!(query));
    if let Some(variables) = variables {
        body.insert("variables".to_owned(), variables);
    }
    Value::Object(body).to_string()
}

/// Performs the POST and returns `(status, body)`.
fn post(token: &str, body: String) -> Result<(u16, Vec<u8>), String> {
    use ureq::http::{HeaderValue, Request};

    let auth = HeaderValue::from_bytes(format!("Bearer {token}").as_bytes())
        .map_err(|_| "The BorgBase API token contains invalid characters.".to_owned())?;
    let request = Request::builder()
        .method("POST")
        .uri(endpoint())
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("Authorization", auth)
        .body(body)
        .map_err(|e| e.to_string())?;
    let mut response = agent().run(request).map_err(|e| e.to_string())?;
    let status = response.status().as_u16();
    let data = response
        .body_mut()
        .with_config()
        .limit(64 * 1024 * 1024)
        .read_to_vec()
        .map_err(|e| e.to_string())?;
    Ok((status, data))
}

/// Interprets an HTTP response as a GraphQL envelope and returns its `data`.
fn handle_response(status: u16, data: &[u8]) -> Result<Value, String> {
    if status == 401 || status == 403 {
        clear_token();
        let snippet: String = String::from_utf8_lossy(data).chars().take(200).collect();
        return Err(format!("{ERR_UNAUTHENTICATED}\n\nBorgBase returned HTTP {status}: {}", snippet.trim()));
    }
    if status != 200 {
        let snippet: String = String::from_utf8_lossy(data).chars().take(400).collect();
        return Err(err_http(status, &snippet));
    }

    let envelope: Value =
        serde_json::from_slice(data).map_err(|_| ERR_INVALID_RESPONSE.to_owned())?;
    let Some(envelope) = envelope.as_object() else {
        return Err(ERR_INVALID_RESPONSE.to_owned());
    };

    match envelope.get("errors") {
        None | Some(Value::Null) => {}
        Some(Value::Array(errors)) => {
            let mut messages = Vec::with_capacity(errors.len());
            for entry in errors {
                let message = entry
                    .get("message")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ERR_INVALID_RESPONSE.to_owned())?;
                messages.push(message);
            }
            if !messages.is_empty() {
                let joined = messages.join("; ");
                let lower = joined.to_lowercase();
                // BorgBase returns 200 even on auth failures and reports the
                // problem as a GraphQL error — detect it so the wizard can
                // bounce the user back to the token entry screen.
                if lower.contains("not authenticated") || lower.contains("unauthorized") {
                    clear_token();
                    // Keep what the server actually said: a token can also be
                    // rejected for a missing scope, which reads very
                    // differently from an expired one.
                    return Err(format!("{ERR_UNAUTHENTICATED}\n\nBorgBase said: {joined}"));
                }
                return Err(err_graphql(&joined));
            }
        }
        Some(_) => return Err(ERR_INVALID_RESPONSE.to_owned()),
    }

    match envelope.get("data") {
        None | Some(Value::Null) => Err(ERR_INVALID_RESPONSE.to_owned()),
        Some(payload) => Ok(payload.clone()),
    }
}

fn send_raw(query: &str, variables: Option<Value>) -> Result<Value, String> {
    let token = token()?;
    let (status, data) = post(&token, request_body(query, variables))?;
    handle_response(status, &data).map_err(|e| {
        if !is_unauthenticated(&e) {
            return e;
        }
        // Describe what was sent (never the token itself) so a mangled
        // paste can be told apart from a token the server really rejects.
        let odd = token.chars().filter(|c| !c.is_ascii_graphic()).count();
        let mut note = format!("Token sent: {} characters", token.chars().count());
        if odd > 0 {
            note += &format!(", {odd} of them spaces or non-ASCII (likely a bad paste)");
        }
        format!("{e}\n{note}.")
    })
}

fn send<T: DeserializeOwned>(query: &str, variables: Option<Value>) -> Result<T, String> {
    let payload = send_raw(query, variables)?;
    serde_json::from_value(payload).map_err(|e| format!("{ERR_INVALID_RESPONSE} ({e})"))
}

/// For mutations that only select `__typename`: succeeds when `data.<field>`
/// is an object (Swift decodes it into an empty struct).
fn send_expecting_object(
    query: &str,
    variables: Option<Value>,
    field: &str,
) -> Result<(), String> {
    let payload = send_raw(query, variables)?;
    match payload.get(field) {
        Some(Value::Object(_)) => Ok(()),
        _ => Err(ERR_INVALID_RESPONSE.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_decoding_is_lenient() {
        let json = r#"{
            "id": "abc",
            "name": "laptop",
            "repoPath": "abc@abc.repo.borgbase.com:repo",
            "currentUsage": 1234.5,
            "quota": 1000,
            "quotaEnabled": true,
            "alertDays": "oops",
            "compactionInterval": 2.0,
            "fullAccessKeys": ["1", "2"],
            "appendOnlyKeys": [1, 2],
            "server": { "region": "eu" }
        }"#;
        let repo: BorgBaseRepo = serde_json::from_str(json).unwrap();
        assert_eq!(repo.id, "abc");
        assert_eq!(repo.name, "laptop");
        assert_eq!(repo.current_usage, Some(1234.5));
        assert_eq!(repo.quota, Some(1000.0));
        assert_eq!(repo.quota_enabled, Some(true));
        assert_eq!(repo.alert_days, None);
        assert_eq!(repo.compaction_interval, Some(2));
        assert_eq!(repo.full_access_keys, Some(vec!["1".to_owned(), "2".to_owned()]));
        assert_eq!(repo.append_only_keys, None);
        assert_eq!(repo.region.as_deref(), Some("eu"));
        assert_eq!(repo.encryption, None);

        let minimal: BorgBaseRepo = serde_json::from_str(r#"{"id":"x","server":null}"#).unwrap();
        assert_eq!(minimal.name, "");
        assert_eq!(minimal.region, None);
        assert!(serde_json::from_str::<BorgBaseRepo>(r#"{"name":"x"}"#).is_err());
    }

    #[test]
    fn last_modified_parsing_and_staleness() {
        let mut repo: BorgBaseRepo = serde_json::from_str(r#"{"id":"x"}"#).unwrap();
        assert_eq!(repo.last_modified_date(), None);
        assert!(!repo.is_stale());

        for s in [
            "2024-03-01T10:20:30.123456+00:00",
            "2024-03-01T10:20:30Z",
            "2024-03-01T10:20:30",
            "2024-03-01T10:20:30.5",
        ] {
            repo.last_modified = Some(s.to_owned());
            let d = repo.last_modified_date().unwrap_or_else(|| panic!("{s}"));
            assert_eq!(d.timestamp(), 1709288430, "{s}");
        }

        repo.alert_days = Some(1);
        assert!(repo.is_stale());
        repo.alert_days = Some(0);
        assert!(!repo.is_stale());
        repo.alert_days = Some(1);
        repo.last_modified = Some(Utc::now().to_rfc3339());
        assert!(!repo.is_stale());
        repo.last_modified = Some("garbage".to_owned());
        assert!(!repo.is_stale());
    }

    #[test]
    fn envelope_handling() {
        let ok = handle_response(200, br#"{"data":{"sshList":[]}}"#).unwrap();
        assert_eq!(ok, json!({"sshList": []}));

        assert_eq!(
            handle_response(200, br#"{"data":null,"errors":[{"message":"a"},{"message":"b"}]}"#),
            Err("BorgBase GraphQL: a; b".to_owned())
        );
        let rejected = handle_response(200, br#"{"errors":[{"message":"User is Not Authenticated"}]}"#).unwrap_err();
        assert!(is_unauthenticated(&rejected));
        assert!(rejected.ends_with("BorgBase said: User is Not Authenticated"));
        assert!(is_unauthenticated(&handle_response(401, b"").unwrap_err()));
        assert_eq!(
            handle_response(500, b"boom"),
            Err("BorgBase returned HTTP 500: boom".to_owned())
        );
        assert_eq!(handle_response(200, b"{}"), Err(ERR_INVALID_RESPONSE.to_owned()));
        assert_eq!(handle_response(200, b"nope"), Err(ERR_INVALID_RESPONSE.to_owned()));
    }

    #[test]
    fn repo_edit_query_only_contains_set_fields() {
        assert!(repo_edit_request("1", &BorgBaseRepoUpdate::default()).is_none());

        let update = BorgBaseRepoUpdate {
            name: Some("new".to_owned()),
            alert_days: Some(3),
            append_only_keys: Some(vec![]),
            ..Default::default()
        };
        let (query, vars) = repo_edit_request("1", &update).unwrap();
        assert_eq!(
            query,
            "mutation repoEdit($id: String!, $name: String, $alertDays: Int, $appendOnlyKeys: [String]) {\n  repoEdit(id: $id, name: $name, alertDays: $alertDays, appendOnlyKeys: $appendOnlyKeys) {\n    __typename\n  }\n}"
        );
        assert_eq!(
            vars,
            json!({"id": "1", "name": "new", "alertDays": 3, "appendOnlyKeys": []})
        );
    }

    #[test]
    fn ssh_key_decoding() {
        let key: BorgBaseSSHKey = serde_json::from_str(
            r#"{"id":"7","name":"mac","keyType":"ssh-ed25519","bits":256,"hashMd5":"aa:bb","addedAt":null}"#,
        )
        .unwrap();
        assert_eq!(key.key_type.as_deref(), Some("ssh-ed25519"));
        assert_eq!(key.bits, Some(256));
        assert_eq!(key.added_at, None);
    }
}
