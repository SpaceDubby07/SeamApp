//! Tauri shell: command handlers, event forwarding, tray, lifecycle.
//!
//! Wires the portable logic in `seam-core` to the OS implementations in
//! `seam-platform` and exposes commands/events to the `ui/` frontend over
//! Tauri IPC. See Tier 5.1 of `documentation/kvm-app-build-guide.md`.

mod commands;
mod connect;
mod state;

use std::collections::HashMap;
use std::sync::Mutex;

use directories::ProjectDirs;
use tauri::{Emitter, Manager};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use seam_core::config::Config;
use seam_core::net::discovery::{Discovery, DiscoveryEvent};
use seam_core::net::tls::NodeIdentity;

use state::{AppState, CURRENT_OS};

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

/// Starts the mDNS daemon, advertises this machine, and forwards
/// found/lost peers to the frontend as they change (Tier 8.1's
/// "discovered devices" list) — kept alive for the app's whole lifetime
/// via `AppState.discovery`.
fn start_discovery(app: &tauri::AppHandle, config: &Config) -> Discovery {
    let discovery = Discovery::new().expect("failed to start the mDNS daemon");
    discovery
        .advertise(
            config.node_id,
            &config.display_name,
            CURRENT_OS,
            state::CONTROL_PORT,
        )
        .expect("failed to advertise via mDNS");

    let local_node = config.node_id;
    let browse_discovery = discovery.clone();
    let app = app.clone();
    // `tokio::spawn` would panic here — `setup` runs before this thread
    // has entered Tauri's async runtime context; `async_runtime::spawn`
    // goes through Tauri's own runtime handle instead. `Discovery::browse`
    // itself calls `tokio::spawn` internally, so it has to run FROM
    // WITHIN this already-on-the-runtime task too, not out here.
    tauri::async_runtime::spawn(async move {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        browse_discovery
            .browse(local_node, tx)
            .expect("failed to start mDNS browse");
        while let Some(event) = rx.recv().await {
            let state = app.state::<AppState>();
            let mut peers = state.discovered_peers.lock().expect("mutex poisoned");
            match event {
                DiscoveryEvent::Found(peer) => {
                    peers.insert(peer.node_id, peer);
                }
                DiscoveryEvent::Lost(node_id) => {
                    peers.remove(&node_id);
                }
            }
            let snapshot: Vec<_> = peers.values().cloned().collect();
            drop(peers);
            let _ = app.emit("peers-changed", &snapshot);
        }
    });

    discovery
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    init_logging();
    tracing::info!("seam starting up");

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let config_path = Config::default_path().expect("could not determine config directory");
            let config = Config::load_or_create(&config_path).expect("failed to load config");
            let identity_dir =
                Config::identity_dir().expect("could not determine identity directory");
            let identity =
                NodeIdentity::load_or_create(&identity_dir).expect("failed to load TLS identity");

            let handle = app.handle();
            let discovery = start_discovery(handle, &config);

            app.manage(AppState {
                config_path,
                config: Mutex::new(config),
                identity,
                discovery,
                discovered_peers: Mutex::new(HashMap::new()),
                session_command_tx: Mutex::new(None),
                session_task: Mutex::new(None),
                pending_pairing: Mutex::new(None),
            });

            connect::spawn_accept_loop(handle.clone());

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::set_display_name,
            commands::list_discovered_peers,
            commands::get_local_screens,
            commands::has_input_permission,
            commands::request_input_permission,
            commands::connect_to_peer,
            commands::confirm_pairing,
            commands::update_layout,
            commands::send_file,
            commands::respond_to_offer,
            commands::disconnect,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
