//! Session: wires `InputCapture` → `StateMachine` → `ControlChannel` →
//! `InputSink` together (Tier 5.1's "plain Rust calls + channels" layer,
//! M4 of the build guide).
//!
//! Entirely portable — it only touches the `seam-core` trait objects, never
//! a concrete OS API, so it compiles and its logic is testable on any
//! platform even though the traits themselves are only implemented for
//! Windows so far (M1) and macOS lands in M5.
//!
//! # What's deliberately NOT here yet
//! - Clipboard sync (`ClipboardUpdate`/`ClipboardBlob`) — M7.
//! - Transfer coordination (`TransferOffer` and friends) — M10.
//! - Dynamic `ScreenConfig` handling on resolution change — not scheduled
//!   to a specific milestone yet.
//! - Reconnect on disconnect — M12. `Action::StartReconnect` currently
//!   just ends the session; the caller decides whether to retry.
//! - Missed-heartbeat dead-peer detection (Tier 7.7) — M12's "health
//!   supervisor." Pings are sent and answered, but a silent peer alone
//!   doesn't yet trigger a disconnect.

use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedReceiver;

use crate::error::PlatformError;
use crate::net::control::{ControlChannel, now_micros};
use crate::protocol::{ControlMessage, KeyCode, Modifiers, ProtocolError};
use crate::state::{Action, Input, State, StateMachine};
use crate::topology::{Point, Rect};
use crate::traits::{InputCapture, InputSink};

pub use crate::protocol::InputEvent;

/// Everything that can go wrong while a session runs.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// A network send/receive or handshake operation failed.
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    /// A platform capture/inject/suppress call failed.
    #[error(transparent)]
    Platform(#[from] PlatformError),
    /// The connection to the peer ended and reconnect isn't implemented
    /// yet (M12).
    #[error("connection lost; reconnect not implemented until M12")]
    Disconnected,
}

/// Owns one node's live connection to its peer: the handoff state machine,
/// the handshaked control channel, and the platform capture/inject
/// handles. Constructing one performs the `Disconnected` → `LocalActive`
/// transition immediately, since a `ControlChannel` only exists once its
/// own (lower-level) protocol handshake has already succeeded — see
/// [`Session::new`].
pub struct Session {
    state_machine: StateMachine,
    control: ControlChannel,
    capture: Box<dyn InputCapture>,
    sink: Box<dyn InputSink>,
    capture_rx: UnboundedReceiver<InputEvent>,
    local_bounds: Rect,
    /// What we last told the being-driven-side OS to reflect via
    /// `ModifierState` sync (Tier 7.1) — compared against each new
    /// `ModifierState` to know which keys to inject. Deliberately excludes
    /// Caps Lock; see `sync_injected_modifiers`.
    injected_modifiers: Modifiers,
    ping_seq: u64,
}

impl Session {
    /// Builds a session from an already-handshaked `control` channel and
    /// starts `capture`. Immediately drives the state machine's
    /// `Disconnected` → `LocalActive` transition, since by the time a
    /// caller has a `ControlChannel` at all, the wire-level handshake
    /// (`Handshake::Hello`/`HelloAck`) has already succeeded — that IS
    /// this state machine's `PeerHandshakeOk` trigger.
    ///
    /// # Errors
    /// Returns an error if `capture` fails to start (e.g. a missing OS
    /// permission).
    pub fn new(
        mut state_machine: StateMachine,
        control: ControlChannel,
        mut capture: Box<dyn InputCapture>,
        sink: Box<dyn InputSink>,
    ) -> Result<Self, PlatformError> {
        let (tx, capture_rx) = tokio::sync::mpsc::unbounded_channel();
        capture.start(tx)?;

        let local_bounds = state_machine.local_bounds();
        let peer = control.peer_node_id;
        for action in state_machine.handle(Input::PeerHandshakeOk(peer), Instant::now()) {
            match action {
                Action::StartHeartbeat => tracing::debug!("heartbeat starting"),
                Action::SyncClipboard => {
                    tracing::debug!("clipboard sync not implemented until M7");
                }
                other => tracing::warn!(?other, "unexpected action from PeerHandshakeOk"),
            }
        }

        Ok(Self {
            state_machine,
            control,
            capture,
            sink,
            capture_rx,
            local_bounds,
            injected_modifiers: Modifiers::default(),
            ping_seq: 0,
        })
    }

    /// The current handoff state, mostly useful for logging/diagnostics.
    #[must_use]
    pub fn state(&self) -> State {
        self.state_machine.state()
    }

    /// Runs the session until the connection ends or an unrecoverable
    /// error occurs: reads capture events, control-channel messages, and
    /// sends periodic heartbeat pings, driving the state machine and
    /// executing whatever actions it returns.
    ///
    /// # Errors
    /// Returns an error if the underlying network or platform calls fail,
    /// or once the peer disconnects (reconnect isn't implemented yet).
    pub async fn run(mut self) -> Result<(), SessionError> {
        let mut ping_interval = tokio::time::interval(Duration::from_secs(2));
        ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick fires immediately; skip it so we don't ping before
        // the peer has even seen us as connected.
        ping_interval.tick().await;

        loop {
            tokio::select! {
                event = self.capture_rx.recv() => {
                    let Some(event) = event else {
                        tracing::warn!("input capture channel closed unexpectedly");
                        return Ok(());
                    };
                    self.handle_capture_event(event).await?;
                }
                msg = self.control.recv() => {
                    match msg {
                        Ok(Some(msg)) => self.handle_control_message(msg).await?,
                        Ok(None) => {
                            tracing::info!("peer closed the connection");
                            let actions =
                                self.state_machine.handle(Input::ConnectionLost, Instant::now());
                            self.execute_actions(actions).await?;
                        }
                        Err(e) => return Err(e.into()),
                    }
                }
                _ = ping_interval.tick() => {
                    self.ping_seq += 1;
                    self.control
                        .send(&ControlMessage::Ping {
                            seq: self.ping_seq,
                            sent_at_micros: now_micros(),
                        })
                        .await?;
                }
            }
        }
    }

    /// Processes one locally-captured input event: feeds cursor motion
    /// into the state machine for edge-crossing/reclaim detection, detects
    /// the escape-hotkey combo, and — while `RemoteActive` — relays the
    /// event to the peer.
    ///
    /// # Errors
    /// Returns an error if a resulting action fails (network send or
    /// platform call).
    pub async fn handle_capture_event(&mut self, event: InputEvent) -> Result<(), SessionError> {
        self.state_machine.track_modifier(&event);

        // Escape hotkey (Tier 7.7): the low-level capture hook forwards
        // every event to our channel BEFORE checking suppression (see
        // capture.rs), so this still fires even while RemoteActive has
        // suppression enabled — satisfying "always active, even in
        // RemoteActive" without a separate OS hotkey registration.
        let is_escape_combo = matches!(
            event,
            InputEvent::KeyDown {
                code: KeyCode::Escape,
                repeat: false
            }
        ) && {
            let held = self.state_machine.held_modifiers();
            held.shift && held.ctrl && held.alt
        };

        if is_escape_combo {
            let actions = self
                .state_machine
                .handle(Input::EscapeHotkey, Instant::now());
            return self.execute_actions(actions).await;
        }

        if let InputEvent::MouseMoveAbs { x, y } = event {
            let actions = self
                .state_machine
                .handle(Input::CursorMoved(Point { x, y }), Instant::now());
            self.execute_actions(actions).await?;
        }

        if self.state_machine.state() == State::RemoteActive {
            let msg = input_event_to_control_message(&event, self.local_bounds);
            self.control.send(&msg).await?;
        }
        Ok(())
    }

    /// Processes one message received from the peer: handshake-adjacent
    /// housekeeping (ping/pong), handoff/reclaim/emergency-release (fed
    /// into the state machine), modifier-state sync, and — while
    /// `BeingDriven` — injecting relayed input locally.
    ///
    /// # Errors
    /// Returns an error if a resulting action fails (network send or
    /// platform call).
    pub async fn handle_control_message(
        &mut self,
        msg: ControlMessage,
    ) -> Result<(), SessionError> {
        let now = Instant::now();
        match msg {
            ControlMessage::Ping {
                seq,
                sent_at_micros,
            } => {
                self.control
                    .send(&ControlMessage::Pong {
                        seq,
                        sent_at_micros,
                    })
                    .await?;
            }
            ControlMessage::Pong {
                seq,
                sent_at_micros,
            } => {
                let rtt_micros = now_micros().saturating_sub(sent_at_micros);
                tracing::debug!(seq, rtt_micros, "pong received");
            }
            ControlMessage::ModifierState { mods } => {
                self.sync_injected_modifiers(mods)?;
            }
            ControlMessage::Handoff { entry } => {
                let peer = self.control.peer_node_id;
                let actions = self
                    .state_machine
                    .handle(Input::ReceivedHandoff { from: peer, entry }, now);
                tracing::info!("peer handed off control to us");
                self.execute_actions(actions).await?;
            }
            ControlMessage::Reclaim => {
                let actions = self.state_machine.handle(Input::ReceivedReclaim, now);
                tracing::info!("peer reclaimed control");
                self.execute_actions(actions).await?;
            }
            ControlMessage::EmergencyRelease => {
                let actions = self
                    .state_machine
                    .handle(Input::ReceivedEmergencyRelease, now);
                tracing::info!("peer sent emergency release");
                self.execute_actions(actions).await?;
            }
            ControlMessage::MouseMove { .. }
            | ControlMessage::MouseDelta { .. }
            | ControlMessage::MouseDown { .. }
            | ControlMessage::MouseUp { .. }
            | ControlMessage::Scroll { .. }
            | ControlMessage::KeyDown { .. }
            | ControlMessage::KeyUp { .. } => {
                self.inject_relayed_input(&msg)?;
            }
            ControlMessage::ClipboardUpdate { .. } => {
                tracing::debug!("clipboard sync not implemented until M7");
            }
            ControlMessage::TransferOffer { .. }
            | ControlMessage::TransferAccept { .. }
            | ControlMessage::TransferReject { .. }
            | ControlMessage::TransferCancel { .. }
            | ControlMessage::TransferComplete { .. } => {
                tracing::debug!("file transfer not implemented until M10");
            }
            ControlMessage::ScreenConfig { .. } => {
                tracing::debug!("dynamic screen config updates not implemented yet");
            }
            ControlMessage::Goodbye { reason } => {
                tracing::info!(reason, "peer sent goodbye");
            }
        }
        Ok(())
    }

    /// Injects a relayed input message while `BeingDriven`. Logs and
    /// ignores it otherwise — a well-behaved peer only sends these while
    /// we've told it we're being driven, so seeing one outside that state
    /// means either a stale/reordered message or a protocol violation,
    /// neither of which should crash the session.
    fn inject_relayed_input(&mut self, msg: &ControlMessage) -> Result<(), SessionError> {
        if self.state_machine.state() != State::BeingDriven {
            tracing::warn!(?msg, "ignoring relayed input while not BeingDriven");
            return Ok(());
        }
        if let Some(event) = control_message_to_input_event(msg, self.local_bounds) {
            self.sink.inject(&event)?;
        }
        Ok(())
    }

    /// Diffs `mods` against what we last synced and injects `KeyDown`/
    /// `KeyUp` for whichever of Shift/Ctrl/Alt/Meta changed, so a modifier
    /// that was already held at the moment of handoff (and so generates no
    /// new key event of its own) still ends up reflected on the machine
    /// being driven. This is Tier 7.1's fix for the stuck-modifier bug
    /// class, applied on receipt rather than on our own transitions.
    ///
    /// Caps Lock is deliberately excluded: it's a toggle, and correctly
    /// syncing a toggle would require knowing the driven machine's actual
    /// OS-level Caps Lock state, which nothing here currently queries.
    /// Left as a known gap rather than guessing.
    ///
    /// The `Left*` variant is used for every synthesized key, since
    /// `Modifiers` doesn't preserve which physical side was held — a
    /// simplification inherent to the wire format itself (Tier 6.3), not
    /// something this function chooses.
    fn sync_injected_modifiers(&mut self, mods: Modifiers) -> Result<(), SessionError> {
        let diffs = [
            (
                self.injected_modifiers.shift,
                mods.shift,
                KeyCode::LeftShift,
            ),
            (self.injected_modifiers.ctrl, mods.ctrl, KeyCode::LeftCtrl),
            (self.injected_modifiers.alt, mods.alt, KeyCode::LeftAlt),
            (self.injected_modifiers.meta, mods.meta, KeyCode::LeftMeta),
        ];
        for (was_down, now_down, code) in diffs {
            if was_down != now_down {
                let event = if now_down {
                    InputEvent::KeyDown {
                        code,
                        repeat: false,
                    }
                } else {
                    InputEvent::KeyUp { code }
                };
                self.sink.inject(&event)?;
            }
        }
        self.injected_modifiers = mods;
        Ok(())
    }

    async fn execute_actions(&mut self, actions: Vec<Action>) -> Result<(), SessionError> {
        for action in actions {
            self.execute_action(action).await?;
        }
        Ok(())
    }

    async fn execute_action(&mut self, action: Action) -> Result<(), SessionError> {
        match action {
            Action::SendModifierState(mods) => {
                self.control
                    .send(&ControlMessage::ModifierState { mods })
                    .await?;
            }
            Action::SendHandoff(entry) => {
                tracing::info!("handing off control to peer");
                self.control
                    .send(&ControlMessage::Handoff { entry })
                    .await?;
            }
            Action::SendReclaim => {
                tracing::info!("reclaiming control from peer");
                self.control.send(&ControlMessage::Reclaim).await?;
            }
            Action::SendEmergencyRelease => {
                tracing::info!("sending emergency release");
                self.control.send(&ControlMessage::EmergencyRelease).await?;
            }
            Action::SetSuppression(suppress) => {
                self.capture.set_suppression(suppress)?;
            }
            Action::ReleaseAllModifiers => {
                self.sink.release_all_modifiers()?;
                self.injected_modifiers = Modifiers::default();
            }
            Action::WarpCursor { x, y } => {
                self.sink.warp_cursor(x, y)?;
            }
            Action::StartHeartbeat => tracing::debug!("heartbeat already running"),
            Action::StartReconnect => {
                tracing::warn!("connection lost; reconnect not implemented until M12");
                return Err(SessionError::Disconnected);
            }
            Action::SyncClipboard => tracing::debug!("clipboard sync not implemented until M7"),
        }
        Ok(())
    }
}

/// Translates a locally-captured event into the wire message that relays
/// it, normalizing absolute cursor positions against `local_bounds`.
/// Returns `None` for nothing — every `InputEvent` variant maps to some
/// `ControlMessage` today, but the signature stays `Option` since that's
/// not a permanent guarantee.
#[allow(clippy::cast_precision_loss)]
fn input_event_to_control_message(event: &InputEvent, local_bounds: Rect) -> ControlMessage {
    match *event {
        InputEvent::MouseMoveAbs { x, y } => ControlMessage::MouseMove {
            x: (x - local_bounds.x) as f32 / local_bounds.width as f32,
            y: (y - local_bounds.y) as f32 / local_bounds.height as f32,
        },
        InputEvent::MouseDelta { dx, dy } => ControlMessage::MouseDelta {
            dx: clamp_i16(dx),
            dy: clamp_i16(dy),
        },
        InputEvent::MouseDown { button } => ControlMessage::MouseDown { button },
        InputEvent::MouseUp { button } => ControlMessage::MouseUp { button },
        InputEvent::Scroll { dx, dy } => ControlMessage::Scroll {
            dx: clamp_i16(dx),
            dy: clamp_i16(dy),
            precise: false,
        },
        InputEvent::KeyDown { code, repeat } => ControlMessage::KeyDown { code, repeat },
        InputEvent::KeyUp { code } => ControlMessage::KeyUp { code },
    }
}

/// The inverse of `input_event_to_control_message`, for injecting a
/// relayed message while `BeingDriven`. Returns `None` for message
/// variants that aren't input relay (handoff, clipboard, etc.) — those are
/// handled elsewhere in `handle_control_message`.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn control_message_to_input_event(msg: &ControlMessage, local_bounds: Rect) -> Option<InputEvent> {
    match *msg {
        ControlMessage::MouseMove { x, y } => Some(InputEvent::MouseMoveAbs {
            x: local_bounds.x + (x * local_bounds.width as f32) as i32,
            y: local_bounds.y + (y * local_bounds.height as f32) as i32,
        }),
        ControlMessage::MouseDelta { dx, dy } => Some(InputEvent::MouseDelta {
            dx: i32::from(dx),
            dy: i32::from(dy),
        }),
        ControlMessage::MouseDown { button } => Some(InputEvent::MouseDown { button }),
        ControlMessage::MouseUp { button } => Some(InputEvent::MouseUp { button }),
        ControlMessage::Scroll { dx, dy, .. } => Some(InputEvent::Scroll {
            dx: i32::from(dx),
            dy: i32::from(dy),
        }),
        ControlMessage::KeyDown { code, repeat } => Some(InputEvent::KeyDown { code, repeat }),
        ControlMessage::KeyUp { code } => Some(InputEvent::KeyUp { code }),
        _ => None,
    }
}

fn clamp_i16(v: i32) -> i16 {
    i16::try_from(v.clamp(i32::from(i16::MIN), i32::from(i16::MAX))).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{InputEvent, Session};
    use crate::error::PlatformError;
    use crate::net::control::ControlChannel;
    use crate::protocol::{ControlMessage, KeyCode, Modifiers, MouseButton, OsKind};
    use crate::state::{State, StateMachine};
    use crate::topology::{Layout, NodeId, Rect};
    use crate::traits::{ClipboardProvider, InputCapture, InputSink, ScreenInfo};
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;
    use tokio::sync::mpsc::UnboundedSender;

    fn bounds() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        }
    }

    /// A no-op capture: `Session` reads events through `handle_capture_event`
    /// directly in these tests rather than via the channel `start` would
    /// feed, so nothing needs to actually be sent here.
    struct NoopCapture {
        suppressed: Arc<Mutex<Vec<bool>>>,
    }

    impl InputCapture for NoopCapture {
        fn start(&mut self, _sink: UnboundedSender<InputEvent>) -> Result<(), PlatformError> {
            Ok(())
        }
        fn stop(&mut self) -> Result<(), PlatformError> {
            Ok(())
        }
        fn set_suppression(&mut self, suppress: bool) -> Result<(), PlatformError> {
            self.suppressed
                .lock()
                .expect("mutex poisoned")
                .push(suppress);
            Ok(())
        }
        fn is_healthy(&self) -> bool {
            true
        }
    }

    #[derive(Clone, Default)]
    struct RecordingSink {
        injected: Arc<Mutex<Vec<InputEvent>>>,
        warps: Arc<Mutex<Vec<(i32, i32)>>>,
        releases: Arc<Mutex<u32>>,
    }

    impl InputSink for RecordingSink {
        fn inject(&mut self, event: &InputEvent) -> Result<(), PlatformError> {
            self.injected.lock().expect("mutex poisoned").push(*event);
            Ok(())
        }
        fn warp_cursor(&mut self, x: i32, y: i32) -> Result<(), PlatformError> {
            self.warps.lock().expect("mutex poisoned").push((x, y));
            Ok(())
        }
        fn release_all_modifiers(&mut self) -> Result<(), PlatformError> {
            *self.releases.lock().expect("mutex poisoned") += 1;
            Ok(())
        }
    }

    /// Builds a handshaked `ControlChannel` pair over real loopback TCP —
    /// matching Tier 12.3's "two in-process nodes over loopback" pattern.
    /// Also returns each side's own `NodeId`, since `Layout` needs both to
    /// place the nodes as adjacent.
    async fn loopback_pair() -> (ControlChannel, NodeId, ControlChannel, NodeId) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let a_node = NodeId::new();
        let b_node = NodeId::new();

        let b_task = tokio::spawn(async move {
            ControlChannel::accept(&listener, b_node, "b", OsKind::Windows)
                .await
                .expect("b handshake")
        });
        let a = ControlChannel::connect(addr, a_node, "a", OsKind::Windows)
            .await
            .expect("a handshake");
        let b = b_task.await.expect("b task");
        (a, a_node, b, b_node)
    }

    fn adjacent_layout(local: NodeId, peer: NodeId, peer_on_right: bool) -> Layout {
        let mut layout = Layout::new();
        layout.set_placement(local, bounds());
        let peer_bounds = if peer_on_right {
            Rect {
                x: 1920,
                ..bounds()
            }
        } else {
            Rect {
                x: -1920,
                ..bounds()
            }
        };
        layout.set_placement(peer, peer_bounds);
        layout
    }

    fn session_with(
        control: ControlChannel,
        local: NodeId,
        layout: Layout,
    ) -> (Session, RecordingSink, Arc<Mutex<Vec<bool>>>) {
        let sm = StateMachine::new(local, bounds(), layout);
        let suppressed = Arc::new(Mutex::new(Vec::new()));
        let capture = NoopCapture {
            suppressed: suppressed.clone(),
        };
        let sink = RecordingSink::default();
        let session = Session::new(sm, control, Box::new(capture), Box::new(sink.clone()))
            .expect("session construction");
        (session, sink, suppressed)
    }

    #[tokio::test]
    async fn constructing_a_session_reaches_local_active() {
        let (a, a_node, b, b_node) = loopback_pair().await;
        let layout = adjacent_layout(a_node, b_node, true);
        let (session, ..) = session_with(a, a_node, layout);
        assert_eq!(session.state(), State::LocalActive);
        // b's side of the channel is unused in this test but stays bound
        // (not dropped) so the connection doesn't look closed to a.
        let _b = b;
    }

    /// The M4 demo, end to end at the protocol/session level (Tier 13):
    /// wire capture → state machine → network → injection, with
    /// suppression toggling and the peer correctly entering `BeingDriven`
    /// and warping its cursor to the negotiated entry point.
    #[tokio::test]
    async fn full_handoff_over_loopback() {
        let (a_control, a_node, b_control, b_node) = loopback_pair().await;
        let a_layout = adjacent_layout(a_node, b_node, true);
        let b_layout = adjacent_layout(b_node, a_node, false);

        let (mut a_session, _a_sink, a_suppressed) = session_with(a_control, a_node, a_layout);
        let (mut b_session, b_sink, _b_suppressed) = session_with(b_control, b_node, b_layout);

        // A slides its cursor to the right edge -> should hand off to B.
        a_session
            .handle_capture_event(InputEvent::MouseMoveAbs { x: 960, y: 540 })
            .await
            .expect("first move");
        a_session
            .handle_capture_event(InputEvent::MouseMoveAbs { x: 1919, y: 540 })
            .await
            .expect("edge move");

        assert_eq!(a_session.state(), State::RemoteActive);
        assert_eq!(*a_suppressed.lock().expect("mutex poisoned"), vec![true]);

        // B receives whatever A's Session sent (ModifierState, then
        // Handoff) and reacts to each in turn.
        for _ in 0..2 {
            let msg = b_session
                .control
                .recv()
                .await
                .expect("recv")
                .expect("not closed");
            b_session.handle_control_message(msg).await.expect("handle");
        }

        assert_eq!(b_session.state(), State::BeingDriven);
        let warps = b_sink.warps.lock().expect("mutex poisoned");
        assert_eq!(warps.len(), 1);
        // Entry point should be on B's LEFT edge (mirrors A's Right exit),
        // vertically centered since A crossed at y=540 of a 1080-tall
        // screen.
        assert_eq!(warps[0].0, 0);
        assert!((warps[0].1 - 540).abs() <= 1);
    }

    #[tokio::test]
    async fn escape_combo_is_detected_even_though_forwarding_would_be_suppressed() {
        let (a_control, a_node, b_control, b_node) = loopback_pair().await;
        let layout = adjacent_layout(a_node, b_node, true);
        let (mut session, _sink, suppressed) = session_with(a_control, a_node, layout);
        // Kept alive (never read from) so every `control.send()` below has
        // somewhere to land — a dropped peer socket makes send() failure
        // timing-dependent, and this test cares about the state machine's
        // reaction, not send delivery.
        let _b_control = b_control;

        for code in [KeyCode::LeftShift, KeyCode::LeftCtrl, KeyCode::LeftAlt] {
            session
                .handle_capture_event(InputEvent::KeyDown {
                    code,
                    repeat: false,
                })
                .await
                .expect("modifier keydown");
        }
        // Force RemoteActive so suppression is already on, proving the
        // combo still gets through to the state machine afterward.
        session
            .handle_capture_event(InputEvent::MouseMoveAbs { x: 960, y: 540 })
            .await
            .expect("center move");
        session
            .handle_capture_event(InputEvent::MouseMoveAbs { x: 1919, y: 540 })
            .await
            .expect("edge move");
        assert_eq!(session.state(), State::RemoteActive);
        assert_eq!(*suppressed.lock().expect("mutex poisoned"), vec![true]);

        session
            .handle_capture_event(InputEvent::KeyDown {
                code: KeyCode::Escape,
                repeat: false,
            })
            .await
            .expect("escape combo");
        assert_eq!(session.state(), State::LocalActive);
    }

    #[tokio::test]
    async fn modifier_state_sync_injects_only_the_changed_keys() {
        let (a_control, a_node, b_control, b_node) = loopback_pair().await;
        let layout = adjacent_layout(a_node, b_node, true);
        let (mut session, sink, _suppressed) = session_with(a_control, a_node, layout);
        let _b_control = b_control;

        session
            .handle_control_message(ControlMessage::ModifierState {
                mods: Modifiers {
                    ctrl: true,
                    ..Modifiers::default()
                },
            })
            .await
            .expect("first sync");
        session
            .handle_control_message(ControlMessage::ModifierState {
                mods: Modifiers {
                    ctrl: true,
                    shift: true,
                    ..Modifiers::default()
                },
            })
            .await
            .expect("second sync");

        let injected = sink.injected.lock().expect("mutex poisoned");
        assert_eq!(
            *injected,
            vec![
                InputEvent::KeyDown {
                    code: KeyCode::LeftCtrl,
                    repeat: false
                },
                InputEvent::KeyDown {
                    code: KeyCode::LeftShift,
                    repeat: false
                },
            ]
        );
    }

    #[tokio::test]
    async fn relayed_input_is_ignored_outside_being_driven() {
        let (a_control, a_node, b_control, b_node) = loopback_pair().await;
        let layout = adjacent_layout(a_node, b_node, true);
        let (mut session, sink, _suppressed) = session_with(a_control, a_node, layout);
        let _b_control = b_control;
        assert_eq!(session.state(), State::LocalActive);

        session
            .handle_control_message(ControlMessage::MouseDown {
                button: MouseButton::Left,
            })
            .await
            .expect("handle");

        assert!(sink.injected.lock().expect("mutex poisoned").is_empty());
    }

    // Compile-time proof that ClipboardProvider/ScreenInfo mocks still
    // satisfy the trait boundary after M3/M4's additions — mirrors
    // traits.rs's own tests, kept here too since session.rs is the module
    // most likely to break that boundary by accident.
    #[allow(dead_code)]
    struct UnusedClipboard;
    impl ClipboardProvider for UnusedClipboard {
        fn watch(
            &mut self,
            _sink: UnboundedSender<crate::protocol::ClipboardEvent>,
        ) -> Result<(), PlatformError> {
            Ok(())
        }
        fn set_text(&mut self, _text: &str) -> Result<(), PlatformError> {
            Ok(())
        }
        fn set_image(&mut self, _png_bytes: &[u8]) -> Result<(), PlatformError> {
            Ok(())
        }
    }
    #[allow(dead_code)]
    struct UnusedScreens;
    impl ScreenInfo for UnusedScreens {
        fn displays(&self) -> Vec<crate::topology::Display> {
            Vec::new()
        }
        fn virtual_bounds(&self) -> Rect {
            bounds()
        }
        fn scale_factor(&self, _display_id: crate::topology::DisplayId) -> f64 {
            1.0
        }
    }
}
