//! On-disk persistence for the repository list and the BorgBox server list.
//! The GUI wraps these in observable stores; the headless runner reads the
//! files directly.
use super::models::{BorgBoxServer, Repository};
use super::paths;
use uuid::Uuid;

fn write_atomic(path: &std::path::Path, data: &[u8]) {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&tmp, data).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

pub fn load_repositories() -> Vec<Repository> {
    std::fs::read(paths::repositories_file())
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default()
}

pub fn save_repositories(repos: &[Repository]) {
    if let Ok(data) = serde_json::to_vec_pretty(repos) {
        write_atomic(&paths::repositories_file(), &data);
    }
}

pub fn load_repository(id: &Uuid) -> Option<Repository> {
    load_repositories().into_iter().find(|r| &r.id == id)
}

pub fn load_borgbox_servers() -> Vec<BorgBoxServer> {
    std::fs::read(paths::borgbox_servers_file())
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default()
}

pub fn save_borgbox_servers(servers: &[BorgBoxServer]) {
    if let Ok(data) = serde_json::to_vec_pretty(servers) {
        write_atomic(&paths::borgbox_servers_file(), &data);
    }
}
