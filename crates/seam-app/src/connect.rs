//! Shared connection bootstrap: pairing, bulk channel setup, `Session`
//! construction, and — for outbound connections — the reconnect
//! supervisor (M12). Used by both the `connect_to_peer` command and the
//! inbound accept loop, since everything past the initial control
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

/// First backoff wait after a dropped connection.
const RECONNECT_INITIAL_DELAY: Duration = Duration::from_millis(500);
/// Ceiling the exponential backoff is clamped to.
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(20);

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
        /// port-plus-one convention as the control channel) and, on a
        /// drop, for the reconnect supervisor.
        host: String,
    },
}

/// Emitted once a session is up, so the UI can leave the connecting/
/// pairing/reconnecting screen.
#[derive(Serialize, Clone)]
struct ConnectedInfo {
    peer_display_name: String,
}

/// Runs the pairing flow (if needed), sets up the bulk channel, builds a
/// `Session`, wires it into `AppState`, and spawns the supervisor that
/// runs it — reconnecting with backoff if this was an outbound connection
/// and it drops unexpectedly.
///
/// # Errors
/// Returns a human-readable error if pairing is declined or any step
/// (bulk connect/accept, `Session::new`, sending our screen config) fails.
/// Once the supervisor is spawned this returns `Ok(())` immediately.
pub async fn finish_connection(
    control: ControlChannel,
    role: Role,
    app: AppHandle,
) -> Result<(), String> {
    // Only an outbound connection reconnects on its own — an accepted one
    // is re-established by the peer dialling back in through the accept
    // loop, which is always listening.
    let reconnect_host = match &role {
        Role::Connector { host } => Some(host.clone()),
        Role::Listener { .. } => None,
    };

    let session = bootstrap_session(control, role, &app).await?;

    let supervisor = tokio::spawn(supervise(session, reconnect_host, app.clone()));
    *app.state::<AppState>()
        .session_task
        .lock()
        .expect("mutex poisoned") = Some(supervisor);

    Ok(())
}

/// Everything after the control handshake, up to a ready-to-run `Session`:
/// the pairing prompt (skipped once a peer is pinned), the bulk channel,
/// `Session::new`, the first screen-config exchange, and wiring the
/// command channel + event forwarding + `connected` event into the app.
/// Reused verbatim by [`reconnect_with_backoff`] — by then trust is
/// `Pinned`, so the pairing block is inert.
async fn bootstrap_session(
    control: ControlChannel,
    role: Role,
    app: &AppHandle,
) -> Result<Session, String> {
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
    // UpdateLayout` takes it from there. On a reconnect this momentarily
    // resets the canvas until the peer re-sends its `ScreenConfig`.
    let initial_peer_bounds = Rect {
        x: local_bounds.x + local_bounds.width.cast_signed(),
        ..local_bounds
    };
    let mut layout = Layout::new();
    layout.set_placement(local_node, local_bounds);
    layout.set_placement(peer_node, initial_peer_bounds);
    let mut state_machine = StateMachine::new(local_node, local_bounds, layout);
    state_machine.set_edge_settings(
        config_snapshot.edge_settings.corner_dead_zone_px,
        config_snapshot.edge_settings.handoff_cooldown_ms,
    );

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

    Ok(session)
}

/// Owns a session for its whole life: runs it, and on an unexpected drop
/// (an `Err` from `run`, as opposed to the `Ok(())` a user Disconnect
/// produces) reconnects with exponential backoff — but only for an
/// outbound connection (`reconnect_host` is `Some`). Emits `disconnected`
/// once, when the session ends for good.
async fn supervise(mut session: Session, reconnect_host: Option<String>, app: AppHandle) {
    loop {
        let outcome = session.run().await;

        // This instance is finished. Clear its command channel right away
        // so `is_connected()` is false during any backoff (and a stray
        // `Shutdown` can't be posted into a dead channel).
        *app.state::<AppState>()
            .session_command_tx
            .lock()
            .expect("mutex poisoned") = None;

        match (&outcome, &reconnect_host) {
            (Ok(()), _) => {
                tracing::info!("session ended gracefully");
                break;
            }
            (Err(e), None) => {
                tracing::warn!(error = %e, "accepted session ended; not reconnecting");
                break;
            }
            (Err(e), Some(host)) => {
                tracing::warn!(error = %e, "connection lost; reconnecting with backoff");
                let _ = app.emit("reconnecting", ());
                session = reconnect_with_backoff(host, &app).await;
                // loop back around and run the fresh session.
            }
        }
    }

    let state = app.state::<AppState>();
    *state.session_command_tx.lock().expect("mutex poisoned") = None;
    *state.session_task.lock().expect("mutex poisoned") = None;
    let _ = app.emit("disconnected", ());
}

/// Retries [`reconnect_once`] with exponential backoff (0.5s → 20s cap)
/// until it succeeds. Runs forever by design — M12's "leave it running
/// for a week" — and is cancelled only by the supervisor task being
/// aborted (which `disconnect` does when there's no live session to send
/// `Shutdown` to).
async fn reconnect_with_backoff(host: &str, app: &AppHandle) -> Session {
    let mut delay = RECONNECT_INITIAL_DELAY;
    let mut attempt = 1u32;
    loop {
        tokio::time::sleep(delay).await;
        match reconnect_once(host, app).await {
            Ok(session) => {
                tracing::info!(attempt, "reconnected");
                return session;
            }
            Err(e) => {
                tracing::warn!(
                    attempt,
                    delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                    error = %e,
                    "reconnect attempt failed"
                );
                attempt += 1;
                delay = (delay * 2).min(RECONNECT_MAX_DELAY);
            }
        }
    }
}

/// One reconnect attempt: redo the control handshake (trust is `Pinned` by
/// now, so no pairing prompt) and rebuild the session.
async fn reconnect_once(host: &str, app: &AppHandle) -> Result<Session, String> {
    let (node_id, display_name, trust) = {
        let state = app.state::<AppState>();
        let config = state.config.lock().expect("mutex poisoned");
        (
            config.node_id,
            config.display_name.clone(),
            config.trust_mode(),
        )
    };
    let control = ControlChannel::connect(
        format!("{host}:{CONTROL_PORT}"),
        node_id,
        &display_name,
        CURRENT_OS,
        &app.state::<AppState>().identity,
        trust,
    )
    .await
    .map_err(|e| format!("control reconnect failed: {e}"))?;

    bootstrap_session(
        control,
        Role::Connector {
            host: host.to_string(),
        },
        app,
    )
    .await
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
