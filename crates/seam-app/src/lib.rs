//! Tauri shell: command handlers, event forwarding, tray, lifecycle.
//!
//! Wires the portable logic in `seam-core` to the OS implementations in
//! `seam-platform` and exposes commands/events to the `ui/` frontend over
//! Tauri IPC. See Tier 5.1 of `documentation/kvm-app-build-guide.md`.

use directories::ProjectDirs;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

/// Directory logs are written to: `<app data dir>/logs`, falling back to the
/// working directory if the OS won't tell us where app data belongs.
fn log_dir() -> std::path::PathBuf {
    ProjectDirs::from("com", "zach", "seam")
        .map(|dirs| dirs.data_dir().join("logs"))
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// Sets up structured logging to both a rolling daily file and stdout.
///
/// `RUST_LOG` overrides the default filter, which shows our own crates at
/// `debug` and everything else (tokio internals, etc.) at `warn` so they
/// don't drown out application logs.
fn init_logging() {
    let file_appender = tracing_appender::rolling::daily(log_dir(), "seam.log");

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            EnvFilter::new("warn,seam_core=debug,seam_platform=debug,seam_app_lib=debug")
        }))
        .with(fmt::layer().with_writer(file_appender).json())
        .with(fmt::layer().with_writer(std::io::stdout).pretty())
        .init();
}

// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    init_logging();
    tracing::info!("seam starting up");

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![greet])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
