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

use crate::config::Config;
use crate::error::PlatformError;
use crate::net::bulk::BulkChannel;
use crate::net::control::{ControlChannel, now_micros};
use crate::protocol::{
    BulkMessage, ClipboardContent, ClipboardEvent, ControlMessage, KeyCode, Modifiers,
    ProtocolError,
};
use crate::remap::RemapTable;
use crate::state::{Action, Input, State, StateMachine};
use crate::topology::{Point, Rect};
use crate::traits::{ClipboardProvider, InputCapture, InputSink};

pub use crate::protocol::InputEvent;

/// Tier 7.4: plain text travels inline on the control channel only up to
/// this size. There's no bulk-relay path for text in the wire protocol
/// (only images have an offer/blob split) — oversized text is skipped
/// entirely rather than partially synced, same as an oversized image.
const CLIPBOARD_TEXT_INLINE_MAX_BYTES: usize = 256 * 1024;

/// An accepted `ClipboardContent::ImageOffer` awaiting its matching
/// `BulkMessage::ClipboardBlob`. Only one can be outstanding at a time — a
/// newer offer simply replaces it, matching the "ignore anything not the
/// latest" spirit of the `seq` ordering rule (Tier 6.3).
struct PendingClipboardImage {
    seq: u64,
    mime: String,
}

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
    /// The bulk channel (M7, Tier 6.1): clipboard images travel here, never
    /// on `control` — a multi-MB image on the control channel would stall
    /// the input-latency-critical path.
    bulk: BulkChannel,
    capture: Box<dyn InputCapture>,
    sink: Box<dyn InputSink>,
    clipboard: Box<dyn ClipboardProvider>,
    capture_rx: UnboundedReceiver<InputEvent>,
    clipboard_rx: UnboundedReceiver<ClipboardEvent>,
    local_bounds: Rect,
    /// What we last told the being-driven-side OS to reflect via
    /// `ModifierState` sync (Tier 7.1) — compared against each new
    /// `ModifierState` to know which keys to inject. Deliberately excludes
    /// Caps Lock; see `sync_injected_modifiers`.
    injected_modifiers: Modifiers,
    /// This machine's own remap table (Tier 7.3): applied to everything we
    /// inject on the peer's behalf, never to what we send. Each machine
    /// owns its own rules, so this is never synced over the wire.
    remap: RemapTable,
    /// Hard cap on outgoing clipboard content (Tier 7.4); see
    /// [`Config::clipboard_max_bytes`].
    clipboard_max_bytes: u64,
    /// Monotonic `seq` for our own outgoing `ClipboardUpdate`s.
    next_clipboard_seq: u64,
    /// Highest peer `ClipboardUpdate` `seq` we've accepted (Text applied
    /// immediately; an image offer counts once accepted, not once its blob
    /// arrives) — anything at or below this is a stale/duplicate/
    /// out-of-order update and is ignored (Tier 6.3's `seq` rule).
    last_seen_peer_clipboard_seq: u64,
    /// An accepted image offer waiting on its bulk-channel blob.
    pending_image: Option<PendingClipboardImage>,
    /// Content we just wrote to the local clipboard because the PEER sent
    /// it. Compared against the next local `ClipboardEvent` our own watcher
    /// reports — if it matches, that event is our own write echoing back
    /// rather than a genuine new local change, and must NOT be broadcast
    /// again (the clipboard-sync equivalent of the stuck-modifier echo
    /// problem: without this, two synced machines would ping-pong the same
    /// update back and forth forever).
    last_applied_from_peer: Option<ClipboardEvent>,
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
    /// `bulk` is this session's already-connected bulk channel (M7) —
    /// clipboard images travel there. `clipboard` is this machine's
    /// clipboard watcher/setter; its `watch` call immediately seeds an
    /// on-connect sync if the local clipboard already holds something (see
    /// [`ClipboardProvider::watch`]'s contract). `config` supplies this
    /// machine's own remap table (Tier 7.3) and clipboard size cap (Tier
    /// 7.4) — only those two fields are read; `config.node_id`/
    /// `display_name` already went into `control`'s handshake before this
    /// call.
    ///
    /// # Errors
    /// Returns an error if `capture` or `clipboard` fail to start (e.g. a
    /// missing OS permission).
    pub fn new(
        mut state_machine: StateMachine,
        control: ControlChannel,
        bulk: BulkChannel,
        mut capture: Box<dyn InputCapture>,
        sink: Box<dyn InputSink>,
        mut clipboard: Box<dyn ClipboardProvider>,
        config: &Config,
    ) -> Result<Self, PlatformError> {
        let (tx, capture_rx) = tokio::sync::mpsc::unbounded_channel();
        capture.start(tx)?;

        let (clipboard_tx, clipboard_rx) = tokio::sync::mpsc::unbounded_channel();
        clipboard.watch(clipboard_tx)?;

        let local_bounds = state_machine.local_bounds();
        let peer = control.peer_node_id;
        for action in state_machine.handle(Input::PeerHandshakeOk(peer), Instant::now()) {
            match action {
                Action::StartHeartbeat => tracing::debug!("heartbeat starting"),
                Action::SyncClipboard => {
                    tracing::debug!(
                        "clipboard sync driven by ClipboardProvider::watch's initial event, \
                         nothing further to do here"
                    );
                }
                other => tracing::warn!(?other, "unexpected action from PeerHandshakeOk"),
            }
        }

        Ok(Self {
            state_machine,
            control,
            bulk,
            capture,
            sink,
            clipboard,
            capture_rx,
            clipboard_rx,
            local_bounds,
            injected_modifiers: Modifiers::default(),
            remap: config.remap.clone(),
            clipboard_max_bytes: config.clipboard_max_bytes,
            next_clipboard_seq: 0,
            last_seen_peer_clipboard_seq: 0,
            pending_image: None,
            last_applied_from_peer: None,
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

        // Once the bulk channel closes, `recv()` would return `None`
        // immediately forever — this guard stops polling it rather than
        // busy-looping. Losing bulk sync degrades clipboard images only;
        // the control channel stays authoritative for whether the session
        // as a whole is still alive.
        let mut bulk_open = true;

        loop {
            tokio::select! {
                event = self.capture_rx.recv() => {
                    let Some(event) = event else {
                        tracing::warn!("input capture channel closed unexpectedly");
                        return Ok(());
                    };
                    self.handle_capture_event(event).await?;
                }
                event = self.clipboard_rx.recv() => {
                    let Some(event) = event else {
                        tracing::warn!("clipboard watcher channel closed unexpectedly");
                        return Ok(());
                    };
                    self.handle_local_clipboard_event(event).await?;
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
                msg = self.bulk.recv(), if bulk_open => {
                    match msg {
                        Ok(Some(msg)) => self.handle_bulk_message(msg)?,
                        Ok(None) => {
                            tracing::info!("peer closed the bulk channel; clipboard images will no longer sync");
                            bulk_open = false;
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
            ControlMessage::ClipboardUpdate { seq, content } => {
                self.handle_remote_clipboard_update(seq, content)?;
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
    ///
    /// Our own `remap` table (Tier 7.3) is applied here, right before
    /// injection — the one place this machine's key-swap/scroll-inversion
    /// rules take effect, since the peer sent us unmodified physical codes.
    fn inject_relayed_input(&mut self, msg: &ControlMessage) -> Result<(), SessionError> {
        if self.state_machine.state() != State::BeingDriven {
            tracing::warn!(?msg, "ignoring relayed input while not BeingDriven");
            return Ok(());
        }
        if let Some(event) = control_message_to_input_event(msg, self.local_bounds) {
            let event = self.remap.apply(event);
            self.sink.inject(&event)?;
        }
        Ok(())
    }

    /// Handles one clipboard change our own `ClipboardProvider` reported —
    /// either a genuine local edit, or the initial "here's what's already
    /// on the clipboard" event `watch` fires on startup (Tier 7.4).
    ///
    /// # Errors
    /// Returns an error if sending the resulting control/bulk message
    /// fails.
    async fn handle_local_clipboard_event(
        &mut self,
        event: ClipboardEvent,
    ) -> Result<(), SessionError> {
        if self.is_echo_of_what_we_just_applied(&event) {
            tracing::debug!("skipping clipboard sync: this is our own write echoing back");
            return Ok(());
        }

        match event {
            ClipboardEvent::Text(text) => {
                if text.len() > CLIPBOARD_TEXT_INLINE_MAX_BYTES {
                    tracing::warn!(
                        len = text.len(),
                        max = CLIPBOARD_TEXT_INLINE_MAX_BYTES,
                        "clipboard text exceeds the inline size limit; not syncing"
                    );
                    return Ok(());
                }
                let seq = self.alloc_clipboard_seq();
                self.control
                    .send(&ControlMessage::ClipboardUpdate {
                        seq,
                        content: ClipboardContent::Text(text),
                    })
                    .await?;
            }
            ClipboardEvent::Image { mime, data } => {
                let size = data.len() as u64;
                if size > self.clipboard_max_bytes {
                    tracing::warn!(
                        size,
                        max = self.clipboard_max_bytes,
                        "clipboard image exceeds the size cap; not syncing"
                    );
                    return Ok(());
                }
                let seq = self.alloc_clipboard_seq();
                self.control
                    .send(&ControlMessage::ClipboardUpdate {
                        seq,
                        content: ClipboardContent::ImageOffer {
                            mime: mime.clone(),
                            size,
                        },
                    })
                    .await?;
                self.bulk
                    .send(&BulkMessage::ClipboardBlob { seq, mime, data })
                    .await?;
            }
        }
        Ok(())
    }

    /// Allocates the next outgoing clipboard `seq`, starting at 1 (0 is
    /// reserved to mean "nothing sent yet" for `last_seen_peer_clipboard_seq`
    /// on the receiving end).
    fn alloc_clipboard_seq(&mut self) -> u64 {
        self.next_clipboard_seq += 1;
        self.next_clipboard_seq
    }

    /// True if `event` is exactly the content we last wrote to the local
    /// clipboard on the peer's behalf — see `last_applied_from_peer`'s
    /// docs. Consumes the marker either way, so only the ONE local event
    /// immediately following an applied peer update can match; anything
    /// after that is a genuine new local change even if it happens to have
    /// identical content.
    fn is_echo_of_what_we_just_applied(&mut self, event: &ClipboardEvent) -> bool {
        self.last_applied_from_peer.take().as_ref() == Some(event)
    }

    /// Handles a `ClipboardUpdate` from the peer: applies `Text` content
    /// immediately, or — for `ImageOffer` — records it as pending until the
    /// matching `BulkMessage::ClipboardBlob` arrives (`handle_bulk_message`).
    ///
    /// # Errors
    /// Returns an error if writing to the local clipboard fails.
    fn handle_remote_clipboard_update(
        &mut self,
        seq: u64,
        content: ClipboardContent,
    ) -> Result<(), SessionError> {
        if seq <= self.last_seen_peer_clipboard_seq {
            tracing::debug!(
                seq,
                last_seen = self.last_seen_peer_clipboard_seq,
                "ignoring stale/out-of-order clipboard update"
            );
            return Ok(());
        }
        self.last_seen_peer_clipboard_seq = seq;

        match content {
            ClipboardContent::Text(text) => {
                self.clipboard.set_text(&text)?;
                self.last_applied_from_peer = Some(ClipboardEvent::Text(text));
                self.pending_image = None;
            }
            ClipboardContent::ImageOffer { mime, size } => {
                if size > self.clipboard_max_bytes {
                    tracing::warn!(
                        size,
                        max = self.clipboard_max_bytes,
                        "peer's clipboard image offer exceeds our size cap; ignoring"
                    );
                    return Ok(());
                }
                self.pending_image = Some(PendingClipboardImage { seq, mime });
            }
        }
        Ok(())
    }

    /// Handles a message on the bulk channel: applies a `ClipboardBlob`
    /// that matches the pending image offer, and ignores anything else
    /// (stale blob, or a `Chunk` — file transfer isn't implemented until
    /// M10).
    ///
    /// # Errors
    /// Returns an error if writing the image to the local clipboard fails.
    fn handle_bulk_message(&mut self, msg: BulkMessage) -> Result<(), SessionError> {
        match msg {
            BulkMessage::ClipboardBlob { seq, data, .. } => {
                // The offer's `mime` is what we already validated against
                // our size cap, so it — not the blob's own copy — is what
                // gets used from here on.
                let Some(pending) = self.pending_image.take_if(|pending| pending.seq == seq) else {
                    tracing::debug!(
                        seq,
                        "ignoring clipboard blob with no matching pending offer"
                    );
                    return Ok(());
                };
                self.clipboard.set_image(&data)?;
                self.last_applied_from_peer = Some(ClipboardEvent::Image {
                    mime: pending.mime,
                    data,
                });
            }
            BulkMessage::Chunk { .. } => {
                tracing::debug!("file transfer not implemented until M10");
            }
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
    /// something this function chooses. Each base code is passed through
    /// our own `remap` table before injecting (Tier 7.3) — this is what
    /// makes a modifier already held at the moment of handoff come out
    /// correctly swapped too, not just fresh `KeyDown`/`KeyUp` events.
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
                let code = self.remap.remap_key(code);
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
            // Only ever produced by `on_handshake_ok`, which `Session::new`
            // consumes directly rather than through `execute_actions` — kept
            // here only so this match stays exhaustive against `Action`.
            Action::SyncClipboard => {}
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
    use crate::config::Config;
    use crate::error::PlatformError;
    use crate::net::bulk::BulkChannel;
    use crate::net::control::ControlChannel;
    use crate::protocol::{
        BulkMessage, ClipboardContent, ClipboardEvent, ControlMessage, KeyCode, Modifiers,
        MouseButton, OsKind,
    };
    use crate::remap::RemapTable;
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

    /// A `ClipboardProvider` mock: `watch` fires `initial` immediately (if
    /// any), mirroring the real on-connect-sync contract, and every
    /// `set_text`/`set_image` call is recorded for assertions.
    #[derive(Clone, Default)]
    struct RecordingClipboard {
        initial: Option<ClipboardEvent>,
        set_texts: Arc<Mutex<Vec<String>>>,
        set_images: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl ClipboardProvider for RecordingClipboard {
        fn watch(&mut self, sink: UnboundedSender<ClipboardEvent>) -> Result<(), PlatformError> {
            if let Some(event) = self.initial.clone() {
                sink.send(event).expect("receiver still open");
            }
            Ok(())
        }
        fn set_text(&mut self, text: &str) -> Result<(), PlatformError> {
            self.set_texts
                .lock()
                .expect("mutex poisoned")
                .push(text.to_string());
            Ok(())
        }
        fn set_image(&mut self, png_bytes: &[u8]) -> Result<(), PlatformError> {
            self.set_images
                .lock()
                .expect("mutex poisoned")
                .push(png_bytes.to_vec());
            Ok(())
        }
    }

    /// Builds a connected `BulkChannel` pair over real loopback TCP, same
    /// pattern as `loopback_pair` for the control channel.
    async fn bulk_loopback_pair() -> (BulkChannel, BulkChannel) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let server =
            tokio::spawn(async move { BulkChannel::accept(&listener).await.expect("accept") });
        let client = BulkChannel::connect(addr).await.expect("connect");
        let server = server.await.expect("server task");
        (client, server)
    }

    async fn session_with(
        control: ControlChannel,
        local: NodeId,
        layout: Layout,
    ) -> (Session, RecordingSink, Arc<Mutex<Vec<bool>>>) {
        let (session, sink, _clipboard, suppressed) = session_with_full(
            control,
            local,
            layout,
            Config::new_default(),
            RecordingClipboard::default(),
        )
        .await;
        (session, sink, suppressed)
    }

    async fn session_with_remap(
        control: ControlChannel,
        local: NodeId,
        layout: Layout,
        remap: RemapTable,
    ) -> (Session, RecordingSink, Arc<Mutex<Vec<bool>>>) {
        let mut config = Config::new_default();
        config.remap = remap;
        let (session, sink, _clipboard, suppressed) = session_with_full(
            control,
            local,
            layout,
            config,
            RecordingClipboard::default(),
        )
        .await;
        (session, sink, suppressed)
    }

    async fn session_with_clipboard(
        control: ControlChannel,
        local: NodeId,
        layout: Layout,
        clipboard: RecordingClipboard,
    ) -> (
        Session,
        RecordingSink,
        RecordingClipboard,
        Arc<Mutex<Vec<bool>>>,
    ) {
        session_with_full(control, local, layout, Config::new_default(), clipboard).await
    }

    async fn session_with_full(
        control: ControlChannel,
        local: NodeId,
        layout: Layout,
        config: Config,
        clipboard: RecordingClipboard,
    ) -> (
        Session,
        RecordingSink,
        RecordingClipboard,
        Arc<Mutex<Vec<bool>>>,
    ) {
        let sm = StateMachine::new(local, bounds(), layout);
        let suppressed = Arc::new(Mutex::new(Vec::new()));
        let capture = NoopCapture {
            suppressed: suppressed.clone(),
        };
        let sink = RecordingSink::default();
        // Only one end is ever driven directly in these tests (via
        // `handle_local_clipboard_event`/`handle_bulk_message`, not the
        // real `run()` select loop), so the peer end just needs to stay
        // alive — kept in the returned tuple's drop scope by virtue of
        // `bulk_loopback_pair`'s server task, not read from here.
        let (bulk, _peer_bulk) = bulk_loopback_pair().await;
        let session = Session::new(
            sm,
            control,
            bulk,
            Box::new(capture),
            Box::new(sink.clone()),
            Box::new(clipboard.clone()),
            &config,
        )
        .expect("session construction");
        (session, sink, clipboard, suppressed)
    }

    #[tokio::test]
    async fn constructing_a_session_reaches_local_active() {
        let (a, a_node, b, b_node) = loopback_pair().await;
        let layout = adjacent_layout(a_node, b_node, true);
        let (session, ..) = session_with(a, a_node, layout).await;
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

        let (mut a_session, _a_sink, a_suppressed) =
            session_with(a_control, a_node, a_layout).await;
        let (mut b_session, b_sink, _b_suppressed) =
            session_with(b_control, b_node, b_layout).await;

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
        let (mut session, _sink, suppressed) = session_with(a_control, a_node, layout).await;
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
        let (mut session, sink, _suppressed) = session_with(a_control, a_node, layout).await;
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
        let (mut session, sink, _suppressed) = session_with(a_control, a_node, layout).await;
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

    /// The M6 demo (Tier 13): a relayed Ctrl keystroke, injected by a
    /// session configured with the `windows_keyboard_on_mac` preset, must
    /// come out as Cmd — never the raw physical code the peer sent.
    #[tokio::test]
    async fn relayed_keys_are_remapped_before_injection() {
        let (a_control, a_node, b_control, b_node) = loopback_pair().await;
        let layout = adjacent_layout(a_node, b_node, true);
        let (mut session, sink, _suppressed) = session_with_remap(
            a_control,
            a_node,
            layout,
            RemapTable::windows_keyboard_on_mac(),
        )
        .await;
        let _b_control = b_control;

        // Force BeingDriven directly (bypassing a real handoff) — this test
        // is only about what the remap table does to an already-relayed
        // message, not about the handoff transition itself.
        session
            .handle_control_message(ControlMessage::Handoff {
                entry: crate::topology::EdgePoint {
                    edge: crate::topology::Edge::Left,
                    pos: 0.5,
                },
            })
            .await
            .expect("handoff");
        assert_eq!(session.state(), State::BeingDriven);

        session
            .handle_control_message(ControlMessage::KeyDown {
                code: KeyCode::LeftCtrl,
                repeat: false,
            })
            .await
            .expect("relayed keydown");

        assert_eq!(
            *sink.injected.lock().expect("mutex poisoned"),
            vec![InputEvent::KeyDown {
                code: KeyCode::LeftMeta,
                repeat: false,
            }]
        );
    }

    /// A modifier already held at the moment of handoff generates no fresh
    /// `KeyDown` of its own — it only shows up in the `ModifierState`
    /// snapshot's diff against "nothing held yet". That diff-driven
    /// synthetic key must be remapped too, or a Windows keyboard's Ctrl
    /// held across a handoff would stick as Ctrl on the Mac instead of Cmd.
    #[tokio::test]
    async fn modifier_state_sync_remaps_the_synthesized_key() {
        let (a_control, a_node, b_control, b_node) = loopback_pair().await;
        let layout = adjacent_layout(a_node, b_node, true);
        let (mut session, sink, _suppressed) = session_with_remap(
            a_control,
            a_node,
            layout,
            RemapTable::windows_keyboard_on_mac(),
        )
        .await;
        let _b_control = b_control;

        session
            .handle_control_message(ControlMessage::ModifierState {
                mods: Modifiers {
                    ctrl: true,
                    ..Modifiers::default()
                },
            })
            .await
            .expect("modifier sync");

        assert_eq!(
            *sink.injected.lock().expect("mutex poisoned"),
            vec![InputEvent::KeyDown {
                code: KeyCode::LeftMeta,
                repeat: false,
            }]
        );
    }

    /// Scroll inversion (Tier 7.3) applies at the same injection point as
    /// key remapping.
    #[tokio::test]
    async fn relayed_scroll_is_inverted_per_the_remap_table() {
        let (a_control, a_node, b_control, b_node) = loopback_pair().await;
        let layout = adjacent_layout(a_node, b_node, true);
        let table = RemapTable {
            invert_scroll_y: true,
            ..RemapTable::default()
        };
        let (mut session, sink, _suppressed) =
            session_with_remap(a_control, a_node, layout, table).await;
        let _b_control = b_control;

        session
            .handle_control_message(ControlMessage::Handoff {
                entry: crate::topology::EdgePoint {
                    edge: crate::topology::Edge::Left,
                    pos: 0.5,
                },
            })
            .await
            .expect("handoff");

        session
            .handle_control_message(ControlMessage::Scroll {
                dx: 2,
                dy: 5,
                precise: false,
            })
            .await
            .expect("relayed scroll");

        assert_eq!(
            *sink.injected.lock().expect("mutex poisoned"),
            vec![InputEvent::Scroll { dx: 2, dy: -5 }]
        );
    }

    /// The M7 demo (Tier 13), text half: copying on one machine syncs to
    /// the other over the control channel, inline.
    #[tokio::test]
    async fn local_text_change_is_synced_to_the_peer() {
        let (a_control, a_node, mut b_control, b_node) = loopback_pair().await;
        let layout = adjacent_layout(a_node, b_node, true);
        let (mut session, ..) = session_with(a_control, a_node, layout).await;

        session
            .handle_local_clipboard_event(ClipboardEvent::Text("hello from A".to_string()))
            .await
            .expect("sync text");

        let msg = b_control.recv().await.expect("recv").expect("not closed");
        assert_eq!(
            msg,
            ControlMessage::ClipboardUpdate {
                seq: 1,
                content: ClipboardContent::Text("hello from A".to_string()),
            }
        );
    }

    /// The M7 demo, image half: an image never touches the control
    /// channel — only an offer does, with the bytes themselves following on
    /// the bulk channel (Tier 7.4).
    #[tokio::test]
    async fn local_image_change_is_offered_on_control_then_sent_on_bulk() {
        let (a_control, a_node, mut b_control, b_node) = loopback_pair().await;
        let layout = adjacent_layout(a_node, b_node, true);
        let sm = StateMachine::new(a_node, bounds(), layout);
        let capture = NoopCapture {
            suppressed: Arc::new(Mutex::new(Vec::new())),
        };
        let (a_bulk, mut b_bulk) = bulk_loopback_pair().await;
        let mut session = Session::new(
            sm,
            a_control,
            a_bulk,
            Box::new(capture),
            Box::new(RecordingSink::default()),
            Box::new(RecordingClipboard::default()),
            &Config::new_default(),
        )
        .expect("session construction");

        let png_bytes = vec![1u8, 2, 3, 4];
        session
            .handle_local_clipboard_event(ClipboardEvent::Image {
                mime: "image/png".to_string(),
                data: png_bytes.clone(),
            })
            .await
            .expect("sync image");

        let control_msg = b_control.recv().await.expect("recv").expect("not closed");
        assert_eq!(
            control_msg,
            ControlMessage::ClipboardUpdate {
                seq: 1,
                content: ClipboardContent::ImageOffer {
                    mime: "image/png".to_string(),
                    size: png_bytes.len() as u64,
                },
            }
        );

        let bulk_msg = b_bulk.recv().await.expect("recv").expect("not closed");
        assert_eq!(
            bulk_msg,
            BulkMessage::ClipboardBlob {
                seq: 1,
                mime: "image/png".to_string(),
                data: png_bytes,
            }
        );
    }

    /// The clipboard-sync equivalent of the stuck-modifier test: applying a
    /// peer update must not bounce right back to them as if it were a fresh
    /// local edit, but a genuinely new local change afterward still syncs.
    #[tokio::test]
    async fn applying_a_peer_update_does_not_echo_back_to_the_peer() {
        let (a_control, a_node, mut b_control, b_node) = loopback_pair().await;
        let layout = adjacent_layout(a_node, b_node, true);
        let (mut session, _sink, clipboard, _suppressed) =
            session_with_clipboard(a_control, a_node, layout, RecordingClipboard::default()).await;

        session
            .handle_control_message(ControlMessage::ClipboardUpdate {
                seq: 1,
                content: ClipboardContent::Text("from peer".to_string()),
            })
            .await
            .expect("apply peer update");
        assert_eq!(
            *clipboard.set_texts.lock().expect("mutex poisoned"),
            vec!["from peer".to_string()]
        );

        // Our own watcher, having just observed that exact write, reports
        // it back through the local-change path — must be swallowed.
        session
            .handle_local_clipboard_event(ClipboardEvent::Text("from peer".to_string()))
            .await
            .expect("handle echo");
        // A genuinely new local change afterward must still sync normally.
        session
            .handle_local_clipboard_event(ClipboardEvent::Text("actually new".to_string()))
            .await
            .expect("handle new change");

        let msg = b_control.recv().await.expect("recv").expect("not closed");
        assert_eq!(
            msg,
            ControlMessage::ClipboardUpdate {
                seq: 1,
                content: ClipboardContent::Text("actually new".to_string()),
            }
        );
    }

    /// Tier 7.4: content over the size limit is skipped entirely — not
    /// partially sent — and doesn't consume a `seq`.
    #[tokio::test]
    async fn oversized_text_is_not_synced() {
        let (a_control, a_node, mut b_control, b_node) = loopback_pair().await;
        let layout = adjacent_layout(a_node, b_node, true);
        let (mut session, ..) = session_with(a_control, a_node, layout).await;

        let huge = "x".repeat(300 * 1024);
        session
            .handle_local_clipboard_event(ClipboardEvent::Text(huge))
            .await
            .expect("handle oversized text");
        session
            .handle_local_clipboard_event(ClipboardEvent::Text("fits fine".to_string()))
            .await
            .expect("handle normal text");

        let msg = b_control.recv().await.expect("recv").expect("not closed");
        assert_eq!(
            msg,
            ControlMessage::ClipboardUpdate {
                seq: 1,
                content: ClipboardContent::Text("fits fine".to_string()),
            }
        );
    }

    /// Tier 6.3's `seq` rule: an update at or below the highest `seq`
    /// already accepted is ignored.
    #[tokio::test]
    async fn stale_clipboard_update_is_ignored() {
        let (a_control, a_node, b_control, b_node) = loopback_pair().await;
        let layout = adjacent_layout(a_node, b_node, true);
        let (mut session, _sink, clipboard, _suppressed) =
            session_with_clipboard(a_control, a_node, layout, RecordingClipboard::default()).await;
        let _b_control = b_control;

        session
            .handle_control_message(ControlMessage::ClipboardUpdate {
                seq: 5,
                content: ClipboardContent::Text("newer".to_string()),
            })
            .await
            .expect("apply newer update");
        session
            .handle_control_message(ControlMessage::ClipboardUpdate {
                seq: 3,
                content: ClipboardContent::Text("stale".to_string()),
            })
            .await
            .expect("stale update should not error");

        assert_eq!(
            *clipboard.set_texts.lock().expect("mutex poisoned"),
            vec!["newer".to_string()]
        );
    }

    /// An `ImageOffer` alone doesn't carry bytes — the image is only
    /// applied once its matching `ClipboardBlob` arrives on the bulk
    /// channel.
    #[tokio::test]
    async fn image_offer_is_applied_once_its_bulk_blob_arrives() {
        let (a_control, a_node, b_control, b_node) = loopback_pair().await;
        let layout = adjacent_layout(a_node, b_node, true);
        let (mut session, _sink, clipboard, _suppressed) =
            session_with_clipboard(a_control, a_node, layout, RecordingClipboard::default()).await;
        let _b_control = b_control;

        session
            .handle_control_message(ControlMessage::ClipboardUpdate {
                seq: 1,
                content: ClipboardContent::ImageOffer {
                    mime: "image/png".to_string(),
                    size: 4,
                },
            })
            .await
            .expect("accept offer");
        assert!(
            clipboard
                .set_images
                .lock()
                .expect("mutex poisoned")
                .is_empty()
        );

        session
            .handle_bulk_message(BulkMessage::ClipboardBlob {
                seq: 1,
                mime: "image/png".to_string(),
                data: vec![1, 2, 3, 4],
            })
            .expect("apply blob");

        assert_eq!(
            *clipboard.set_images.lock().expect("mutex poisoned"),
            vec![vec![1, 2, 3, 4]]
        );
    }

    /// The size cap is enforced on the offer itself — the receiving side
    /// never waits on (or applies) a blob for an offer it already rejected.
    #[tokio::test]
    async fn oversized_image_offer_is_rejected_without_applying_its_blob() {
        let (a_control, a_node, b_control, b_node) = loopback_pair().await;
        let layout = adjacent_layout(a_node, b_node, true);
        let mut config = Config::new_default();
        config.clipboard_max_bytes = 10;
        let (mut session, _sink, clipboard, _suppressed) = session_with_full(
            a_control,
            a_node,
            layout,
            config,
            RecordingClipboard::default(),
        )
        .await;
        let _b_control = b_control;

        session
            .handle_control_message(ControlMessage::ClipboardUpdate {
                seq: 1,
                content: ClipboardContent::ImageOffer {
                    mime: "image/png".to_string(),
                    size: 1000,
                },
            })
            .await
            .expect("offer over cap should not error");
        session
            .handle_bulk_message(BulkMessage::ClipboardBlob {
                seq: 1,
                mime: "image/png".to_string(),
                data: vec![0; 1000],
            })
            .expect("blob for a rejected offer should not error");

        assert!(
            clipboard
                .set_images
                .lock()
                .expect("mutex poisoned")
                .is_empty()
        );
    }

    // Compile-time proof that ScreenInfo mocks still satisfy the trait
    // boundary after M3/M4/M7's additions — mirrors traits.rs's own tests,
    // kept here too since session.rs is the module most likely to break
    // that boundary by accident. (`ClipboardProvider` gets the same
    // coverage for real, above, via `RecordingClipboard`.)
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
