//! `#[tauri::command]` handlers the `ui/` frontend calls via `invoke`.

use tauri::{AppHandle, Emitter, Manager, State};

use seam_core::config::Config;
use seam_core::net::control::ControlChannel;
use seam_core::net::discovery::DiscoveredPeer;
use seam_core::session::SessionCommand;
use seam_core::topology::{Display, Rect};

use crate::connect::{Role, finish_connection};
use crate::state::{AppState, CURRENT_OS};

/// Returns this machine's current settings.
#[tauri::command]
pub fn get_config(state: State<'_, AppState>) -> Config {
    state.config.lock().expect("mutex poisoned").clone()
}

/// Renames this machine (the Connection panel's device-name field) and
/// persists it immediately.
///
/// # Errors
/// Returns an error if the config can't be saved.
#[tauri::command]
pub fn set_display_name(name: String, state: State<'_, AppState>) -> Result<(), String> {
    let mut config = state.config.lock().expect("mutex poisoned");
    config.display_name = name;
    config.save(&state.config_path).map_err(|e| e.to_string())
}

/// Peers currently visible over mDNS (Tier 8.1's "discovered devices"
/// list) — a snapshot; the frontend also listens for the `peers-changed`
/// event for live updates.
#[tauri::command]
pub fn list_discovered_peers(state: State<'_, AppState>) -> Vec<DiscoveredPeer> {
    state
        .discovered_peers
        .lock()
        .expect("mutex poisoned")
        .values()
        .cloned()
        .collect()
}

/// This machine's own displays and virtual desktop bounds, for the layout
/// canvas's local tile.
#[tauri::command]
pub fn get_local_screens() -> (Vec<Display>, Rect) {
    let platform = seam_platform::current_platform();
    (
        platform.screens.displays(),
        platform.screens.virtual_bounds(),
    )
}

/// Whether this OS's input-capture permission (Accessibility, on macOS)
/// has been granted.
#[tauri::command]
pub fn has_input_permission() -> bool {
    seam_platform::has_input_permission()
}

/// Opens the OS's permission-request UI (System Settings > Accessibility
/// on macOS; a no-op on Windows).
///
/// # Errors
/// Returns an error if the OS-level request call itself fails.
#[tauri::command]
pub fn request_input_permission() -> Result<(), String> {
    seam_platform::request_input_permission().map_err(|e| e.to_string())
}

/// Connects to `addr` (a bare host or IP — the control port is always
/// [`crate::state::CONTROL_PORT`]), running the pairing flow if this is
/// the first time these two machines have connected.
///
/// # Errors
/// Returns a human-readable error if the handshake, pairing, or session
/// startup fails.
#[tauri::command]
pub async fn connect_to_peer(addr: String, app: AppHandle) -> Result<(), String> {
    let host = addr.split(':').next().unwrap_or(&addr).to_string();
    let (node_id, display_name, trust, identity_fingerprint) = {
        let state = app.state::<AppState>();
        let config = state.config.lock().expect("mutex poisoned");
        (
            config.node_id,
            config.display_name.clone(),
            config.trust_mode(),
            state.identity.fingerprint,
        )
    };
    let _ = identity_fingerprint; // reserved for a future "my fingerprint" UI display

    let control_target = format!("{host}:{}", crate::state::CONTROL_PORT);
    let identity = &app.state::<AppState>().identity;
    let control = ControlChannel::connect(
        control_target,
        node_id,
        &display_name,
        CURRENT_OS,
        identity,
        trust,
    )
    .await
    .map_err(|e| format!("connection failed: {e}"))?;

    finish_connection(control, Role::Connector { host }, app).await
}

/// Answers a `pairing-requested` event: `accept` must match whether the
/// on-screen code matched the other machine's.
#[tauri::command]
pub fn confirm_pairing(accept: bool, state: State<'_, AppState>) -> Result<(), String> {
    let sender = state.pending_pairing.lock().expect("mutex poisoned").take();
    match sender {
        Some(tx) => {
            let _ = tx.send(accept);
            Ok(())
        }
        None => Err("no pairing confirmation is pending".to_string()),
    }
}

/// Sends a command into the active session. Every layout/transfer command
/// below is a thin wrapper over this — there's only ever one active
/// session (v1's single-peer simplification).
fn send_session_command(state: &State<'_, AppState>, cmd: SessionCommand) -> Result<(), String> {
    let sender = state.session_command_tx.lock().expect("mutex poisoned");
    match sender.as_ref() {
        Some(tx) => tx.send(cmd).map_err(|_| "session ended".to_string()),
        None => Err("not connected".to_string()),
    }
}

/// Rearranges the layout canvas (Tier 8.1): places the peer at
/// `peer_bounds`, in this machine's own coordinate space, and tells the
/// peer about it.
///
/// # Errors
/// Returns an error if there's no active session.
#[tauri::command]
pub fn update_layout(peer_bounds: Rect, state: State<'_, AppState>) -> Result<(), String> {
    send_session_command(&state, SessionCommand::UpdateLayout { peer_bounds })
}

/// Offers `path` to the connected peer (Tier 7.5).
///
/// # Errors
/// Returns an error if there's no active session.
#[tauri::command]
pub fn send_file(path: String, state: State<'_, AppState>) -> Result<(), String> {
    send_session_command(
        &state,
        SessionCommand::SendFile(std::path::PathBuf::from(path)),
    )
}

/// Answers a `session-event` of type `OfferReceived`.
///
/// # Errors
/// Returns an error if there's no active session.
#[tauri::command]
pub fn respond_to_offer(
    transfer_id: seam_core::protocol::TransferId,
    accept: bool,
    state: State<'_, AppState>,
) -> Result<(), String> {
    send_session_command(
        &state,
        SessionCommand::RespondToOffer {
            transfer_id,
            accept,
        },
    )
}

/// Ends the active session by aborting its `run()` task outright —
/// `Session` has no graceful shutdown path yet (M12's reliability work
/// covers a real `Goodbye`-then-close), so this drops the control/bulk
/// sockets without telling the peer; it'll notice via its own
/// connection-closed handling. A no-op if nothing's connected.
///
/// Aborting is safe: `Session`'s `Drop` still runs when the task's future
/// is dropped, so suppression is lifted, modifiers are released, and the
/// capture hook is torn down. But abort skips `run`'s own post-exit
/// bookkeeping in `finish_connection` (which is what normally emits
/// `disconnected`), so we emit it here — otherwise the UI never leaves
/// the connected state.
#[tauri::command]
pub fn disconnect(state: State<'_, AppState>, app: AppHandle) {
    if let Some(task) = state.session_task.lock().expect("mutex poisoned").take() {
        task.abort();
    }
    *state.session_command_tx.lock().expect("mutex poisoned") = None;
    let _ = app.emit("disconnected", ());
}
