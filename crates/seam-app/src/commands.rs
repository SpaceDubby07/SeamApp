//! `#[tauri::command]` handlers the `ui/` frontend calls via `invoke`.

use tauri::{AppHandle, Emitter, Manager, State};

use seam_core::config::Config;
use seam_core::net::control::ControlChannel;
use seam_core::net::discovery::DiscoveredPeer;
use seam_core::session::SessionCommand;

use crate::connect::{Role, finish_connection};
use crate::logbuf::{self, LogLine};
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

/// Drops the pinned pairing (Tier 8.1's "Forget" option). The next
/// connection to any peer runs the full trust-on-first-use pairing flow
/// again. Doesn't disconnect an in-progress session — it only affects the
/// *next* handshake.
///
/// # Errors
/// Returns an error only if the config file can't be written.
#[tauri::command]
pub fn forget_peer(state: State<'_, AppState>) -> Result<(), String> {
    let mut config = state.config.lock().expect("mutex poisoned");
    config.paired_peer = None;
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

/// Sends a command into the active session. Every transfer command below
/// is a thin wrapper over this — there's only ever one active session
/// (v1's single-peer simplification).
fn send_session_command(state: &State<'_, AppState>, cmd: SessionCommand) -> Result<(), String> {
    let sender = state.session_command_tx.lock().expect("mutex poisoned");
    match sender.as_ref() {
        Some(tx) => tx.send(cmd).map_err(|_| "session ended".to_string()),
        None => Err("not connected".to_string()),
    }
}

/// Offers `path` to the connected peer.
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

/// Cancels an in-flight transfer (Tier 8.1 panel 4's per-row cancel
/// button), sent or received — the peer is told so its side stops too.
///
/// # Errors
/// Returns an error if there's no active session.
#[tauri::command]
pub fn cancel_transfer(
    transfer_id: seam_core::protocol::TransferId,
    state: State<'_, AppState>,
) -> Result<(), String> {
    send_session_command(&state, SessionCommand::CancelTransfer(transfer_id))
}

/// Pauses an in-flight transfer, sent or received — the peer is told so
/// its side reflects it too, and (if we're the sender) actually stops
/// sending chunks.
///
/// # Errors
/// Returns an error if there's no active session.
#[tauri::command]
pub fn pause_transfer(
    transfer_id: seam_core::protocol::TransferId,
    state: State<'_, AppState>,
) -> Result<(), String> {
    send_session_command(&state, SessionCommand::PauseTransfer(transfer_id))
}

/// Reverses a `pause_transfer`.
///
/// # Errors
/// Returns an error if there's no active session.
#[tauri::command]
pub fn resume_transfer(
    transfer_id: seam_core::protocol::TransferId,
    state: State<'_, AppState>,
) -> Result<(), String> {
    send_session_command(&state, SessionCommand::ResumeTransfer(transfer_id))
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

/// Ends the active session cleanly: sends [`SessionCommand::Shutdown`],
/// which makes `run` send the peer a `Goodbye` and return `Ok(())` — then
/// `finish_connection`'s wrapper task clears state and emits
/// `disconnected`. A no-op if nothing's connected.
///
/// If the command can't be delivered (no session, or its channel is
/// already gone), this falls back to aborting the task and emitting
/// `disconnected` here, so the UI never gets stuck in the connected
/// state.
#[tauri::command]
pub fn disconnect(state: State<'_, AppState>, app: AppHandle) {
    let requested = state
        .session_command_tx
        .lock()
        .expect("mutex poisoned")
        .as_ref()
        .is_some_and(|tx| tx.send(SessionCommand::Shutdown).is_ok());

    if !requested {
        if let Some(task) = state.session_task.lock().expect("mutex poisoned").take() {
            task.abort();
        }
        *state.session_command_tx.lock().expect("mutex poisoned") = None;
        let _ = app.emit("disconnected", ());
    }
}

/// Recent log lines for the frontend's Log panel. Pass the highest `seq`
/// already displayed as `after_seq` to fetch only what's new; omit it for
/// the whole buffer.
#[tauri::command]
pub fn get_logs(after_seq: Option<u64>) -> Vec<LogLine> {
    logbuf::snapshot(after_seq)
}

/// Empties the in-memory log buffer (does not touch the on-disk log file).
#[tauri::command]
pub fn clear_logs() {
    logbuf::clear();
}

/// Writes the current log buffer to `<log dir>/seam-logs-<unix>.txt` and
/// returns the full path, for the "Export" button.
///
/// # Errors
/// Returns an error string if the file can't be created or written.
#[tauri::command]
pub fn export_logs() -> Result<String, String> {
    let dir = crate::log_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let path = dir.join(format!("seam-logs-{stamp}.txt"));
    std::fs::write(&path, logbuf::render_text())
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    Ok(path.to_string_lossy().into_owned())
}
