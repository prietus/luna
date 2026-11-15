//! GTK4 / libadwaita user interface.
#![allow(dead_code)]

pub mod kit;
pub mod state;

pub mod app;
pub mod debug;
pub mod detail;
pub mod window;

// Sheets attached to the main window.
pub mod add_repository;
pub mod archive_browser;
pub mod create_backup;
pub mod file_history;
pub mod prune;
pub mod schedule_sheet;
pub mod treemap;
pub mod update_passphrase;

// Provider panels.
pub mod automation;
pub mod borgbase_edit;
pub mod borgbase_panel;
pub mod borgbox_alerts;
pub mod borgbox_archives;
pub mod borgbox_import;
pub mod borgbox_panel;
pub mod borgbox_passphrase;
pub mod borgbox_prune;
pub mod borgbox_schedule_editor;
pub mod stats;

// Stand-alone wizard windows.
pub mod borg_wizard;
pub mod borgbase_wizard;
pub mod borgbox_wizard;

// Tray icon + its popover-style menu window.
pub mod menu;
pub mod tray;
