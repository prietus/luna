//! Discovery of local SSH keys and generation of dedicated per-repo keys.
use super::format::slug;
use super::paths;
use std::process::{Command, Stdio};

/// A discovered SSH private key on the local machine.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LocalSSHKey {
    pub private_path: String,
    pub public_path: String,
    /// ed25519, rsa, ecdsa, …
    pub kind: String,
    pub comment: String,
}

impl LocalSSHKey {
    pub fn display_name(&self) -> String {
        let filename = self.private_path.rsplit('/').next().unwrap_or(&self.private_path);
        if self.comment.is_empty() {
            format!("{filename} ({})", self.kind)
        } else {
            format!("{filename} ({}) — {}", self.kind, self.comment)
        }
    }

    /// Full contents of the `.pub` file (single line, ready to upload).
    pub fn public_key(&self) -> Option<String> {
        std::fs::read_to_string(&self.public_path).ok().map(|s| s.trim().to_string())
    }
}

const BLACKLISTED_NAMES: [&str; 7] =
    ["config", "known_hosts", "known_hosts.old", "authorized_keys", "authorized_keys2", "environment", "rc"];
const BLACKLISTED_EXTENSIONS: [&str; 4] = ["pub", "old", "bak", "crt"];

/// Lists private SSH keys under `~/.ssh` by pairing each non-`.pub` file with
/// a matching `.pub` and parsing its first line for the algorithm + comment.
pub fn list() -> Vec<LocalSSHKey> {
    let ssh_dir = paths::home().join(".ssh");
    let Ok(entries) = std::fs::read_dir(&ssh_dir) else { return vec![] };
    let mut result = vec![];
    for entry in entries.flatten() {
        let file = entry.file_name().to_string_lossy().into_owned();
        if file.starts_with('.') || BLACKLISTED_NAMES.contains(&file.as_str()) {
            continue;
        }
        let ext = file.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
        if BLACKLISTED_EXTENSIONS.contains(&ext) {
            continue;
        }
        let private_path = ssh_dir.join(&file).to_string_lossy().into_owned();
        let public_path = format!("{private_path}.pub");
        let Ok(raw) = std::fs::read_to_string(&public_path) else { continue };
        let line = raw.lines().find(|l| !l.is_empty()).unwrap_or("").trim();
        let parts: Vec<&str> = line.splitn(3, ' ').filter(|p| !p.is_empty()).collect();
        if parts.len() < 2 {
            continue;
        }
        result.push(LocalSSHKey {
            private_path,
            public_path,
            kind: parts[0].replace("ssh-", ""),
            comment: parts.get(2).map(|c| c.trim().to_string()).unwrap_or_default(),
        });
    }
    result.sort_by(|a, b| a.private_path.cmp(&b.private_path));
    result
}

/// Generates a new ed25519 key pair under `~/.ssh/luna_<slug>` with no
/// passphrase. Returns `(private key path, public key line)`. An existing
/// key at the same path is reused, never overwritten.
pub fn generate(slug_source: &str, comment: &str) -> Result<(String, String), String> {
    use std::os::unix::fs::DirBuilderExt;
    let ssh_dir = paths::home().join(".ssh");
    let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&ssh_dir);
    let private_path = ssh_dir.join(format!("luna_{}", slug(slug_source, '_')));
    let public_path = format!("{}.pub", private_path.to_string_lossy());
    if !private_path.exists() {
        let out = Command::new("ssh-keygen")
            .args(["-t", "ed25519", "-f"])
            .arg(&private_path)
            .args(["-N", "", "-C", comment, "-q"])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("ssh-keygen failed: {e}"))?;
        if !out.status.success() {
            return Err(format!("ssh-keygen failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }
    }
    let public = std::fs::read_to_string(&public_path)
        .map_err(|_| "Could not read the generated public key".to_string())?;
    Ok((private_path.to_string_lossy().into_owned(), public.trim().to_string()))
}
