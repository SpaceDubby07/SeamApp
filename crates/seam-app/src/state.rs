//! Tauri-managed application state: this machine's identity/config, the
//! live mDNS discovery daemon, discovered peers, and (at most one, per
//! v1's single-peer simplification) active session's command channel.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use seam_core::config::Config;
use seam_core::net::discovery::{DiscoveredPeer, Discovery};
use seam_core::net::tls::NodeIdentity;
use seam_core::session::SessionCommand;
use seam_core::topology::NodeId;
use tokio::sync::{mpsc::UnboundedSender, oneshot};

/// Fixed control port (Tier 6.5) — matches the CLI demos; not yet
/// user-configurable.
pub const CONTROL_PORT: u16 = 24800;
/// Bulk port: control port + 1 (Tier 6.5).
pub const BULK_PORT: u16 = 24801;

/// This machine's OS, for the `Handshake`/mDNS TXT record — the one place
/// `seam-app` itself needs a `#[cfg]` (everything else goes through
/// `seam_platform::current_platform()`).
#[cfg(target_os = "macos")]
pub const CURRENT_OS: seam_core::protocol::OsKind = seam_core::protocol::OsKind::MacOs;
/// This machine's OS, for the `Handshake`/mDNS TXT record.
#[cfg(windows)]
pub const CURRENT_OS: seam_core::protocol::OsKind = seam_core::protocol::OsKind::Windows;

/// Everything the Tauri command layer needs, shared across every command
/// invocation and the background accept/discovery tasks.
pub struct AppState {
    /// Where `config` is persisted — read once at startup.
    pub config_path: PathBuf,
    /// This machine's persisted settings. Held behind a `Mutex` rather
    /// than `RwLock` — every access here is a quick read/clone or a
    /// write immediately followed by `save`, never held across an
    /// `.await`.
    pub config: Mutex<Config>,
    /// This machine's TLS identity (Tier 7.6), generated once and reused.
    pub identity: NodeIdentity,
    /// The live mDNS daemon. Never read after `setup` stores it here —
    /// its only job is to keep living for the process's lifetime, since
    /// dropping it would tear down our own advertisement.
    #[allow(dead_code)]
    pub discovery: Discovery,
    /// Peers currently visible over mDNS, keyed by node id.
    pub discovered_peers: Mutex<HashMap<NodeId, DiscoveredPeer>>,
    /// The active session's command sender, if connected — v1 supports
    /// exactly one peer at a time (Tier 15 covers what a third machine
    /// would need).
    pub session_command_tx: Mutex<Option<UnboundedSender<SessionCommand>>>,
    /// The task running the active session's `run()` loop — aborting it
    /// is currently the only way to close a connection (`Session` has no
    /// graceful shutdown path yet; that's M12's reliability work).
    pub session_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Set while a `connect_to_peer`/incoming-accept pairing handshake is
    /// waiting on the user to confirm the on-screen code — `confirm_pairing`
    /// resolves it.
    pub pending_pairing: Mutex<Option<oneshot::Sender<bool>>>,
}

impl AppState {
    /// Whether a session is currently active — used to reject a second
    /// incoming connection attempt while one peer is already connected.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.session_command_tx
            .lock()
            .expect("mutex poisoned")
            .is_some()
    }
}
