//! Platform-independent core: models, borg wrapper, keyring, scheduling,
//! headless runner and API clients. Everything here is blocking and free of
//! GTK types so it can run on worker threads and in the headless runner.
#![allow(dead_code)]

pub mod archive_tree;
pub mod borg;
pub mod borgbase;
pub mod borgbox;
pub mod format;
pub mod keyring;
pub mod models;
pub mod notify;
pub mod paths;
pub mod runner;
pub mod schedule;
pub mod snapshot;
pub mod sshkeys;
pub mod status;
pub mod store;
