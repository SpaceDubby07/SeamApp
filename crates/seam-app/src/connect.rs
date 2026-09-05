//! Shared connection bootstrap: pairing, bulk channel setup, and `Session`
//! construction — used by both the outbound `connect_to_peer` command and
//! the inbound accept loop, since everything past the initial control
//! handshake is identical either way.

use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tokio::net::TcpListener;

use seam_core::net::bulk::BulkChannel;
use seam_core::net::control::ControlChannel;
use seam_core::net::pairing::pairing_code;
use seam_core::net::tls::Trust;
use seam_core::session::{Session, SessionEvent, SessionHandle};
use seam_core::state::StateMachine;
use seam_core::topology::{Layout, Rect};

use crate::state::{AppState, BULK_PORT, CONTROL_PORT, CURRENT_OS};

/// Which side of the control handshake we were — determines how the bulk
/// channel gets set up (Tier 6.1: it never re-runs `OnFirstUse`, only
/// `Pinned` to whatever the control channel just authenticated).
pub enum Role {
    /// We accepted an incoming connection.
    Listener {
        /// Bound once at app startup and reused for every accepted
        /// connection.
        bulk_listener: &'static TcpListener,
    },
    /// We initiated the connection.
    Connector {
        /// The peer's host, reused for the bulk channel (same fixed
        /// port-plus-one convention as the control channel).
        host: String,
    },
}

/// Emitted once a session is up, so the UI can leave the connecting/
/// pairing screen.
#[derive(Serialize, Clone)]
struct ConnectedInfo {
    peer_display_name: String,
}

/// Runs the pairing flow (if needed), sets up the bulk channel, builds and
/// starts a `Session`, and stores its command sender in `AppState` —
/// everything after the control handshake succeeds, for either role.
///
/// # Errors
/// Returns a human-readable error if pairing is declined or any step
/// (bulk connect/accept, `Session::new`, sending our screen config) fails.
pub async fn finish_connection(
    control: ControlChannel,
    role: Role,
    app: AppHandle,
) -> Result<(), String> {
    let state = app.state::<AppState>();

    if state.is_connected() {
        // v1 single-peer simplification (Tier 15): a second connection
        // attempt while already paired up is simply dropped.
        return Err("already connected to a peer".to_string());
    }

    let trust = { state.config.lock().expect("mutex poisoned").trust_mode() };
    if matches!(trust, Trust::OnFirstUse) {
        let code = pairing_code(state.identity.fingerprint, control.peer_fingerprint);
        let (tx, rx) = tokio::sync::oneshot::channel();
        *state.pending_pairing.lock().expect("mutex poisoned") = Some(tx);
        app.emit("pairing-requested", &code)
            .map_err(|e| e.to_string())?;

        let accept = rx.await.unwrap_or(false);
        if !accept {
            return Err("pairing was declined".to_string());
        }

        let mut config = state.config.lock().expect("mutex poisoned");
        config.pin_peer(control.peer_node_id, control.peer_fingerprint);
        config
            .save(&state.config_path)
            .map_err(|e| format!("failed to save paired peer: {e}"))?;
    }

    let peer_display_name = control.peer_display_name.clone();
    let peer_node = control.peer_node_id;
    let peer_fingerprint = control.peer_fingerprint;

    let bulk = match &role {
        Role::Listener { bulk_listener } => {
            BulkChannel::accept(bulk_listener, &state.identity, peer_fingerprint)
                .await
                .map_err(|e| format!("bulk channel accept failed: {e}"))?
        }
        Role::Connector { host } => {
            let bulk_target = format!("{host}:{BULK_PORT}");
            BulkChannel::connect(bulk_target, &state.identity, peer_fingerprint)
                .await
                .map_err(|e| format!("bulk channel connect failed: {e}"))?
        }
    };

    let platform = seam_platform::current_platform();
    let local_bounds = platform.screens.virtual_bounds();
    let displays = platform.screens.displays();
    let (local_node, config_snapshot) = {
        let config = state.config.lock().expect("mutex poisoned");
        (config.node_id, config.clone())
    };

    // Naive initial placement — immediately to the right, non-overlapping
    // — good enough to start a session; the user drags the layout canvas
    // (Tier 8.1) into whatever's actually true, and `SessionCommand::
    // UpdateLayout` takes it from there.
    let initial_peer_bounds = Rect {
        x: local_bounds.x + local_bounds.width.cast_signed(),
        ..local_bounds
    };
    let mut layout = Layout::new();
    layout.set_placement(local_node, local_bounds);
    layout.set_placement(peer_node, initial_peer_bounds);
    let state_machine = StateMachine::new(local_node, local_bounds, layout);

    let (mut session, handle) = Session::new(
        state_machine,
        control,
        bulk,
        platform.capture,
        platform.sink,
        platform.clipboard,
        &config_snapshot,
    )
    .map_err(|e| format!("failed to start session: {e}"))?;

    session
        .send_screen_config(displays, local_bounds)
        .await
        .map_err(|e| format!("failed to send screen config: {e}"))?;

    let SessionHandle {
        command_tx,
        mut event_rx,
    } = handle;
    *state.session_command_tx.lock().expect("mutex poisoned") = Some(command_tx);

    app.emit("connected", &ConnectedInfo { peer_display_name })
        .map_err(|e| e.to_string())?;
    // The naive initial placement, so the layout canvas has a starting
    // position before `PeerScreenConfig` corrects its size (below).
    app.emit(
        "session-event",
        &SessionEvent::LayoutChanged {
            peer_bounds: initial_peer_bounds,
        },
    )
    .map_err(|e| e.to_string())?;

    let events_app = app.clone();
    tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            let _ = events_app.emit("session-event", &event);
        }
    });

    let ended_app = app.clone();
    let session_task = tokio::spawn(async move {
        if let Err(e) = session.run().await {
            tracing::warn!(error = %e, "session ended");
        }
        let ended_state = ended_app.state::<AppState>();
        *ended_state
            .session_command_tx
            .lock()
            .expect("mutex poisoned") = None;
        *ended_state.session_task.lock().expect("mutex poisoned") = None;
        let _ = ended_app.emit("disconnected", ());
    });
    *state.session_task.lock().expect("mutex poisoned") = Some(session_task);

    Ok(())
}

/// Binds the control and bulk ports once and accepts connections
/// indefinitely — Tier 8.1's "peer-to-peer, no server/client toggle"
/// means this machine is always dialable, alongside whatever outbound
/// `connect_to_peer` the user initiates from the Connection panel.
pub fn spawn_accept_loop(app: AppHandle) {
    // `tokio::spawn` would panic here — called synchronously from
    // `setup`, before this thread has entered Tauri's async runtime
    // context; `async_runtime::spawn` goes through Tauri's own runtime
    // handle instead. Everything spawned FROM WITHIN this task (in
    // `finish_connection`) is already running on that runtime by then,
    // so plain `tokio::spawn` is fine there.
    tauri::async_runtime::spawn(async move {
        let control_listener = match TcpListener::bind(("0.0.0.0", CONTROL_PORT)).await {
            Ok(listener) => listener,
            Err(e) => {
                tracing::error!(error = %e, "failed to bind the control port");
                return;
            }
        };
        let bulk_listener = match TcpListener::bind(("0.0.0.0", BULK_PORT)).await {
            Ok(listener) => listener,
            Err(e) => {
                tracing::error!(error = %e, "failed to bind the bulk port");
                return;
            }
        };
        // Leaked once, for the process's lifetime, so `finish_connection`
        // can borrow it with a `'static` lifetime across every accepted
        // connection without needing an `Arc` threaded through `Role`.
        let bulk_listener: &'static TcpListener = Box::leak(Box::new(bulk_listener));

        loop {
            let state = app.state::<AppState>();
            let (node_id, display_name, trust) = {
                let config = state.config.lock().expect("mutex poisoned");
                (
                    config.node_id,
                    config.display_name.clone(),
                    config.trust_mode(),
                )
            };
            match ControlChannel::accept(
                &control_listener,
                node_id,
                &display_name,
                CURRENT_OS,
                &state.identity,
                trust,
            )
            .await
            {
                Ok(control) => {
                    if let Err(e) =
                        finish_connection(control, Role::Listener { bulk_listener }, app.clone())
                            .await
                    {
                        tracing::warn!(error = %e, "incoming connection did not complete");
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "incoming control handshake failed");
                    // Avoid a tight error loop if something's persistently
                    // wrong (e.g. a port scanner hammering the listener).
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
        }
    });
}
