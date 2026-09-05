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
//! - Reconnect on disconnect — M12. `Action::StartReconnect` currently
//!   just ends the session; the caller decides whether to retry.
//! - Missed-heartbeat dead-peer detection (Tier 7.7) — M12's "health
//!   supervisor." Pings are sent and answered, but a silent peer alone
//!   doesn't yet trigger a disconnect.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::config::Config;
use crate::error::PlatformError;
use crate::net::bulk::BulkChannel;
use crate::net::control::{ControlChannel, now_micros};
use crate::protocol::{
    BulkMessage, ClipboardContent, ClipboardEvent, ControlMessage, FileManifest, KeyCode,
    Modifiers, ProtocolError, TransferId,
};
use crate::remap::RemapTable;
use crate::state::{Action, Input, State, StateMachine};
use crate::topology::{Display, Point, Rect};
use crate::traits::{ClipboardProvider, InputCapture, InputSink};
use crate::transfer::manifest::{build_manifest, sanitize_file_name};
use crate::transfer::{AcceptPolicy, CHUNK_SIZE, IncomingTransfer, OutgoingTransfer};

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
    /// Tier 7.2: while `RemoteActive`, the OS clamps our own (suppressed)
    /// cursor's reported absolute position at whichever edge triggered
    /// the handoff — pushing further in that direction can't produce a
    /// new `MouseMoveAbs` reading, only continued `MouseDelta`s (a real
    /// hardware delta on macOS; a since-last-real-sample delta on Windows
    /// — see each platform's `capture.rs`). This tracks where the cursor
    /// would logically be if it weren't clamped, seeded at the exact
    /// crossing point and integrated purely from `MouseDelta`s from then
    /// on, so reclaim detection (which needs a real, unclamped distance
    /// from that edge) keeps working. `None` whenever not `RemoteActive`.
    remote_drive_position: Option<Point>,
    /// Files queued to offer once whatever's currently sending (if
    /// anything) finishes — v1 sends one file at a time (Tier 15's
    /// single-peer simplification applied to transfers; nothing about the
    /// wire protocol requires this).
    pending_sends: VecDeque<PathBuf>,
    /// The transfer currently being sent, from the `TransferOffer` up
    /// through however many chunks have gone out. `None` means nothing is
    /// being sent right now.
    current_outgoing: Option<OutgoingTransfer>,
    /// Transfers currently being received, keyed by id.
    incoming_transfers: HashMap<TransferId, IncomingTransfer>,
    /// Incoming offers awaiting a human decision under
    /// `AcceptPolicy::Ask` — the file isn't opened for writing until
    /// `RespondToOffer { accept: true, .. }` arrives.
    pending_offers: HashMap<TransferId, FileManifest>,
    /// This machine's policy for incoming offers from the (single, v1)
    /// paired peer (Tier 7.5).
    accept_policy: AcceptPolicy,
    /// Where accepted incoming files are written.
    download_dir: PathBuf,
    /// Commands from whatever's driving this session (Tier 4.5: channels,
    /// not a method call, since `run` owns the only handle to the live
    /// channels once it's running).
    command_rx: UnboundedReceiver<SessionCommand>,
    /// `false` once `command_rx`'s sender (the driver's [`SessionHandle`])
    /// is dropped — same guard pattern as `bulk_open` in `run`, so a
    /// closed channel doesn't turn into a busy-loop.
    commands_open: bool,
    /// Where transfer progress/completion/offers are reported to whatever
    /// is driving this session.
    event_tx: UnboundedSender<SessionEvent>,
}

/// Reported out of a running [`Session`] to whatever's driving it (a CLI
/// demo today; a Tauri command layer eventually) — nothing in `session` or
/// `transfer` does UI work of its own (Tier 4.5: channels over shared
/// mutexes, applied to the session/UI boundary too).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type")]
pub enum SessionEvent {
    /// The peer told us its screens (Tier 8.1's layout canvas needs real
    /// aspect ratios/resolutions to render tiles to scale) — sent once
    /// right after connecting; see [`Session::send_screen_config`].
    PeerScreenConfig {
        /// The peer's current displays.
        displays: Vec<Display>,
        /// The peer's current virtual desktop bounds, in the PEER's own
        /// coordinate space — meaningless as a local pixel position, but
        /// its width/height are what a layout canvas tile should be
        /// drawn at scale.
        virtual_bounds: Rect,
    },
    /// The shared layout changed — either the local user rearranged the
    /// canvas (Tier 8.1's drag-and-snap tiles) or the peer did and told us
    /// via `ControlMessage::LayoutUpdate`. `peer_bounds` is in OUR OWN
    /// coordinate space (already translated, regardless of which side
    /// this came from), ready to compare against `local_bounds` for
    /// rendering.
    LayoutChanged {
        /// Where the peer now sits, in our own coordinate space.
        peer_bounds: Rect,
    },
    /// An incoming offer needs a human decision — only sent under
    /// [`AcceptPolicy::Ask`]. Answer with
    /// [`SessionCommand::RespondToOffer`].
    OfferReceived {
        /// Which transfer this offer is for.
        transfer_id: TransferId,
        /// The offered file's metadata.
        manifest: FileManifest,
    },
    /// Bytes sent (outgoing) or received (incoming) so far, for a progress
    /// bar. Emitted at most once per chunk.
    Progress {
        /// Which transfer this is progress for.
        transfer_id: TransferId,
        /// Bytes transferred so far.
        bytes_done: u64,
        /// Total size of the file being transferred.
        total: u64,
    },
    /// The peer rejected a transfer we offered.
    Rejected {
        /// Which transfer was rejected.
        transfer_id: TransferId,
        /// The peer's human-readable reason.
        reason: String,
    },
    /// A transfer finished and (for an incoming one) was verified. `path`
    /// is the final destination path for an incoming transfer, or the
    /// original source path for an outgoing one.
    Completed {
        /// Which transfer completed.
        transfer_id: TransferId,
        /// Where the file ended up (incoming) or was read from (outgoing).
        path: PathBuf,
    },
    /// A transfer failed: a local I/O error, a hash mismatch on receive,
    /// or a peer-initiated cancel.
    Failed {
        /// Which transfer failed.
        transfer_id: TransferId,
        /// Human-readable reason, for logging/display.
        reason: String,
    },
}

/// Commands a driver sends INTO a running [`Session`] — the other half of
/// [`SessionEvent`], since `Session::run` owns the only handle to the live
/// channels and can't be reached by a direct method call once it's
/// running.
#[derive(Debug, Clone)]
pub enum SessionCommand {
    /// Rearranges the shared layout canvas (Tier 8.1): places the peer at
    /// `peer_bounds`, in OUR OWN coordinate space, and tells the peer
    /// about it via `ControlMessage::LayoutUpdate`.
    UpdateLayout {
        /// Where to place the peer, in our own coordinate space.
        peer_bounds: Rect,
    },
    /// Offer `path` to the peer. Queued if a send is already in progress —
    /// v1 sends at most one file at a time (Tier 15's single-peer spirit
    /// applied to transfers too; nothing about the wire protocol prevents
    /// more later).
    SendFile(PathBuf),
    /// Answer a [`SessionEvent::OfferReceived`]. Ignored if `transfer_id`
    /// doesn't match a pending offer (e.g. it already timed out or was
    /// cancelled).
    RespondToOffer {
        /// Which offer this answers.
        transfer_id: TransferId,
        /// Whether to accept it.
        accept: bool,
    },
    /// Cancels a transfer, sent or received.
    CancelTransfer(TransferId),
}

/// The other end of a running [`Session`]'s command/event channels —
/// returned alongside it from [`Session::new`] so a driver (a CLI demo
/// today; a Tauri command layer eventually) can send it work and observe
/// transfer progress without blocking `run`'s select loop.
pub struct SessionHandle {
    /// Send [`SessionCommand`]s into the running session.
    pub command_tx: UnboundedSender<SessionCommand>,
    /// Receive [`SessionEvent`]s from the running session.
    pub event_rx: UnboundedReceiver<SessionEvent>,
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
    /// machine's own remap table (Tier 7.3), clipboard size cap (Tier
    /// 7.4), and transfer accept policy/download directory (Tier 7.5) —
    /// `config.node_id`/`display_name` already went into `control`'s
    /// handshake before this call.
    ///
    /// Returns the session alongside a [`SessionHandle`] — the command/
    /// event channel a driver uses to queue file sends and observe
    /// transfer progress while `run` is blocking on its select loop.
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
    ) -> Result<(Self, SessionHandle), PlatformError> {
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

        let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel();
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();

        let session = Self {
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
            remote_drive_position: None,
            pending_sends: VecDeque::new(),
            current_outgoing: None,
            incoming_transfers: HashMap::new(),
            pending_offers: HashMap::new(),
            accept_policy: config.accept_policy,
            download_dir: config.resolved_download_dir(),
            command_rx,
            commands_open: true,
            event_tx,
        };
        let handle = SessionHandle {
            command_tx,
            event_rx,
        };
        Ok((session, handle))
    }

    /// The current handoff state, mostly useful for logging/diagnostics.
    #[must_use]
    pub fn state(&self) -> State {
        self.state_machine.state()
    }

    /// The peer's current bounds on the shared layout canvas (Tier 8.1),
    /// in our own coordinate space — `None` until either side has placed
    /// it (the initial `Session::new` layout, `SessionCommand::
    /// UpdateLayout`, or the peer's own `ControlMessage::LayoutUpdate`).
    #[must_use]
    pub fn peer_bounds(&self) -> Option<Rect> {
        self.state_machine.peer_bounds()
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
                        Ok(Some(msg)) => self.handle_bulk_message(msg).await?,
                        Ok(None) => {
                            tracing::info!("peer closed the bulk channel; clipboard images and file transfers will no longer sync");
                            bulk_open = false;
                        }
                        Err(e) => return Err(e.into()),
                    }
                }
                cmd = self.command_rx.recv(), if self.commands_open => {
                    match cmd {
                        Some(cmd) => self.handle_session_command(cmd).await?,
                        None => self.commands_open = false,
                    }
                }
                // M10: sends the next outgoing chunk one at a time, ready
                // every tick this select loop runs whenever a transfer is
                // actively sending — so a multi-gigabyte file never
                // monopolizes this task for longer than one chunk write,
                // keeping it interleaved with the input-latency-critical
                // branches above (Tier 7.5's smoothness requirement).
                () = std::future::ready(()), if self.ready_to_send_chunk() => {
                    self.send_next_outgoing_chunk().await?;
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
            let was_remote_active = self.state_machine.state() == State::RemoteActive;
            let actions = self
                .state_machine
                .handle(Input::CursorMoved(Point { x, y }), Instant::now());
            self.execute_actions(actions).await?;
            if !was_remote_active && self.state_machine.state() == State::RemoteActive {
                // Tier 7.2: seeds the virtual drive position (below) at
                // the exact crossing point. Everything past this relies
                // on `MouseDelta`, not further `MouseMoveAbs` readings —
                // see `remote_drive_position`'s docs for why.
                self.remote_drive_position = Some(Point { x, y });
            }
        } else if let InputEvent::MouseDelta { dx, dy } = event
            && self.state_machine.state() == State::RemoteActive
            && let Some(pos) = self.remote_drive_position.as_mut()
        {
            pos.x += dx;
            pos.y += dy;
            let actions = self
                .state_machine
                .handle(Input::CursorMoved(*pos), Instant::now());
            self.execute_actions(actions).await?;
        }

        // `MouseMoveAbs` is deliberately never relayed: the receiving
        // side was already placed via `Handoff`'s entry point, and once
        // driving, our own absolute position is unreliable right at the
        // edge that triggered the handoff (Tier 7.2) — `MouseDelta`
        // (relayed here like every other event) is what carries all
        // motion from here on.
        if self.state_machine.state() == State::RemoteActive
            && !matches!(event, InputEvent::MouseMoveAbs { .. })
        {
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
            ControlMessage::TransferOffer {
                transfer_id,
                manifest,
            } => {
                self.handle_incoming_offer(transfer_id, manifest).await?;
            }
            ControlMessage::TransferAccept {
                transfer_id,
                resume_from,
            } => {
                self.handle_transfer_accept(transfer_id, resume_from).await;
            }
            ControlMessage::TransferReject {
                transfer_id,
                reason,
            } => {
                self.handle_transfer_reject(transfer_id, reason).await?;
            }
            ControlMessage::TransferCancel { transfer_id } => {
                self.handle_transfer_cancel(transfer_id).await?;
            }
            ControlMessage::TransferComplete { transfer_id, hash } => {
                self.handle_transfer_complete(transfer_id, hash).await?;
            }
            ControlMessage::ScreenConfig {
                displays,
                virtual_bounds,
            } => {
                self.handle_peer_screen_config(displays, virtual_bounds);
            }
            ControlMessage::LayoutUpdate {
                sender_bounds,
                peer_bounds,
            } => {
                self.handle_layout_update(sender_bounds, peer_bounds);
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
    /// that matches the pending image offer (ignoring a stale one with no
    /// match), or writes an incoming file transfer `Chunk`.
    ///
    /// # Errors
    /// Returns an error if writing the image to the local clipboard fails.
    async fn handle_bulk_message(&mut self, msg: BulkMessage) -> Result<(), SessionError> {
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
            BulkMessage::Chunk {
                transfer_id,
                offset,
                data,
            } => {
                self.handle_incoming_chunk(transfer_id, offset, data)
                    .await?;
            }
        }
        Ok(())
    }

    /// Sends this machine's own screens to the peer (Tier 8.1's layout
    /// canvas needs both sides' real resolutions to draw tiles to scale).
    /// Meant to be called once, right after a session starts — there's no
    /// milestone yet for re-sending on a live resolution change (module
    /// docs' "what's deliberately not here").
    ///
    /// # Errors
    /// Returns an error if the send fails.
    pub async fn send_screen_config(
        &mut self,
        displays: Vec<Display>,
        virtual_bounds: Rect,
    ) -> Result<(), SessionError> {
        self.control
            .send(&ControlMessage::ScreenConfig {
                displays,
                virtual_bounds,
            })
            .await?;
        Ok(())
    }

    /// Forwards the peer's `ControlMessage::ScreenConfig` to the driver —
    /// nothing here needs it, it's purely so a layout canvas can render
    /// the peer's tile at its real resolution.
    ///
    /// `virtual_bounds` is in the PEER's own coordinate space — never
    /// meaningful as a position in ours (Tier 8.1's layout canvas only
    /// uses it for the tile's real width/height/aspect ratio). If we've
    /// already placed the peer (the naive initial placement `Session::new`
    /// callers make before either side's real size is known, or a prior
    /// `LayoutUpdate`), correct that placement's SIZE to the peer's real
    /// one while preserving its position — keeping it touching whichever
    /// edge it was already on — and tell the driver via `LayoutChanged`
    /// so a layout canvas doesn't have to reconcile size and position
    /// from two separate events itself.
    fn handle_peer_screen_config(&mut self, displays: Vec<Display>, virtual_bounds: Rect) {
        if let Some(current) = self.state_machine.peer_bounds() {
            let corrected = Rect {
                width: virtual_bounds.width,
                height: virtual_bounds.height,
                ..current
            };
            self.state_machine
                .set_peer_placement(self.control.peer_node_id, corrected);
            let _ = self.event_tx.send(SessionEvent::LayoutChanged {
                peer_bounds: corrected,
            });
        }
        let _ = self.event_tx.send(SessionEvent::PeerScreenConfig {
            displays,
            virtual_bounds,
        });
    }

    /// Handles the peer's `ControlMessage::LayoutUpdate`: re-expresses
    /// where it placed us into OUR OWN coordinate space and updates our
    /// layout to match, so both sides agree on the shared edge regardless
    /// of who dragged the canvas.
    ///
    /// # The math
    /// `sender_bounds` and `peer_bounds` (which is US, from the sender's
    /// point of view) are both in the SENDER's coordinate space, which we
    /// can't assume matches ours — a multi-monitor virtual desktop can
    /// have a negative-origin display, so "place the peer at these exact
    /// coordinates" would be wrong. What's frame-independent is the
    /// OFFSET between the two origins: `peer_bounds.origin -
    /// sender_bounds.origin` is the vector from the sender's origin to
    /// where it placed us, and that same vector applies however you look
    /// at it. So the sender sits at `our_origin - offset` in our own
    /// space, sized to `sender_bounds`' width/height (its real screen
    /// size, unaffected by whichever frame we're computing in).
    fn handle_layout_update(&mut self, sender_bounds: Rect, peer_bounds: Rect) {
        let offset_x = peer_bounds.x - sender_bounds.x;
        let offset_y = peer_bounds.y - sender_bounds.y;
        let sender_bounds_here = Rect {
            x: self.local_bounds.x - offset_x,
            y: self.local_bounds.y - offset_y,
            width: sender_bounds.width,
            height: sender_bounds.height,
        };
        self.state_machine
            .set_peer_placement(self.control.peer_node_id, sender_bounds_here);
        let _ = self.event_tx.send(SessionEvent::LayoutChanged {
            peer_bounds: sender_bounds_here,
        });
    }

    /// Handles a `TransferOffer` from the peer, per `accept_policy` (Tier
    /// 7.5): auto-reject, auto-accept, or park it in `pending_offers` and
    /// tell the driver to ask the user.
    async fn handle_incoming_offer(
        &mut self,
        transfer_id: TransferId,
        manifest: FileManifest,
    ) -> Result<(), SessionError> {
        match self.accept_policy {
            AcceptPolicy::AlwaysDeny => {
                self.control
                    .send(&ControlMessage::TransferReject {
                        transfer_id,
                        reason: "this device is not accepting incoming transfers".to_string(),
                    })
                    .await?;
            }
            AcceptPolicy::Ask => {
                let _ = self.event_tx.send(SessionEvent::OfferReceived {
                    transfer_id,
                    manifest: manifest.clone(),
                });
                self.pending_offers.insert(transfer_id, manifest);
            }
            AcceptPolicy::AlwaysAccept => {
                self.accept_offer(transfer_id, manifest).await?;
            }
        }
        Ok(())
    }

    /// Opens the destination file and tells the peer to start sending —
    /// the second half of `handle_incoming_offer`'s `AlwaysAccept` path,
    /// also called from `handle_session_command` once a human answers an
    /// `Ask`-policy offer.
    async fn accept_offer(
        &mut self,
        transfer_id: TransferId,
        manifest: FileManifest,
    ) -> Result<(), SessionError> {
        let dest = self.download_dir.join(sanitize_file_name(&manifest.name));
        match IncomingTransfer::open(transfer_id, manifest, dest).await {
            Ok((incoming, resume_from)) => {
                self.incoming_transfers.insert(transfer_id, incoming);
                self.control
                    .send(&ControlMessage::TransferAccept {
                        transfer_id,
                        resume_from,
                    })
                    .await?;
            }
            Err(e) => {
                tracing::warn!(
                    ?transfer_id,
                    error = %e,
                    "failed to open destination for incoming transfer"
                );
                self.control
                    .send(&ControlMessage::TransferReject {
                        transfer_id,
                        reason: format!("receiver I/O error: {e}"),
                    })
                    .await?;
                let _ = self.event_tx.send(SessionEvent::Failed {
                    transfer_id,
                    reason: e.to_string(),
                });
            }
        }
        Ok(())
    }

    /// Handles the peer rejecting a transfer we offered: gives up on it
    /// and starts the next queued send, if any. Ignored if `transfer_id`
    /// doesn't match `current_outgoing` (e.g. a stale/duplicate reject).
    async fn handle_transfer_reject(
        &mut self,
        transfer_id: TransferId,
        reason: String,
    ) -> Result<(), SessionError> {
        if self
            .current_outgoing
            .as_ref()
            .is_some_and(|t| t.transfer_id == transfer_id)
        {
            self.current_outgoing = None;
            let _ = self.event_tx.send(SessionEvent::Rejected {
                transfer_id,
                reason,
            });
            self.start_next_pending_send().await?;
        }
        Ok(())
    }

    /// Handles the peer cancelling a transfer, sent or received: drops
    /// whichever side we're tracking it on and, if it was our own send,
    /// starts the next queued one.
    async fn handle_transfer_cancel(
        &mut self,
        transfer_id: TransferId,
    ) -> Result<(), SessionError> {
        let was_incoming = self.incoming_transfers.remove(&transfer_id).is_some();
        let was_outgoing = self
            .current_outgoing
            .as_ref()
            .is_some_and(|t| t.transfer_id == transfer_id);
        if was_outgoing {
            self.current_outgoing = None;
        }
        if was_incoming || was_outgoing {
            let _ = self.event_tx.send(SessionEvent::Failed {
                transfer_id,
                reason: "cancelled by peer".to_string(),
            });
        }
        if was_outgoing {
            self.start_next_pending_send().await?;
        }
        Ok(())
    }

    /// Marks the matching outgoing transfer ready to send, seeking to
    /// `resume_from`. Silently ignored if `transfer_id` doesn't match
    /// `current_outgoing` (e.g. we already gave up on it).
    async fn handle_transfer_accept(&mut self, transfer_id: TransferId, resume_from: u64) {
        if let Some(outgoing) = self.current_outgoing.as_mut()
            && outgoing.transfer_id == transfer_id
        {
            if let Err(e) = outgoing.accept(resume_from).await {
                tracing::warn!(?transfer_id, error = %e, "failed to seek outgoing transfer to resume_from");
                self.current_outgoing = None;
                let _ = self.event_tx.send(SessionEvent::Failed {
                    transfer_id,
                    reason: e.to_string(),
                });
            } else {
                tracing::info!(?transfer_id, resume_from, "peer accepted transfer");
            }
        }
    }

    /// Whether `run`'s select loop should send another chunk this tick.
    fn ready_to_send_chunk(&self) -> bool {
        self.current_outgoing.as_ref().is_some_and(|t| t.accepted)
    }

    /// Sends exactly one chunk of `current_outgoing`, or — once the file is
    /// exhausted — the closing `TransferComplete` and starts the next
    /// queued send, if any.
    async fn send_next_outgoing_chunk(&mut self) -> Result<(), SessionError> {
        let Some(outgoing) = self.current_outgoing.as_mut() else {
            return Ok(());
        };
        let transfer_id = outgoing.transfer_id;
        let total = outgoing.manifest.size;

        match outgoing.read_next_chunk().await {
            Ok(Some((offset, data))) => {
                self.bulk
                    .send(&BulkMessage::Chunk {
                        transfer_id,
                        offset,
                        data,
                    })
                    .await?;
                let bytes_done = self
                    .current_outgoing
                    .as_ref()
                    .map_or(total, |t| t.bytes_sent);
                let _ = self.event_tx.send(SessionEvent::Progress {
                    transfer_id,
                    bytes_done,
                    total,
                });
            }
            Ok(None) => {
                let hash = outgoing.manifest.hash;
                let path = outgoing.original_path.clone();
                self.current_outgoing = None;
                self.control
                    .send(&ControlMessage::TransferComplete { transfer_id, hash })
                    .await?;
                let _ = self
                    .event_tx
                    .send(SessionEvent::Completed { transfer_id, path });
                self.start_next_pending_send().await?;
            }
            Err(e) => {
                tracing::warn!(?transfer_id, error = %e, "outgoing transfer read failed");
                self.current_outgoing = None;
                let _ = self.event_tx.send(SessionEvent::Failed {
                    transfer_id,
                    reason: e.to_string(),
                });
                self.start_next_pending_send().await?;
            }
        }
        Ok(())
    }

    /// Pops the next queued file (if any), hashes it, opens it, and offers
    /// it to the peer. A no-op if the queue is empty.
    async fn start_next_pending_send(&mut self) -> Result<(), SessionError> {
        let Some(path) = self.pending_sends.pop_front() else {
            return Ok(());
        };
        let transfer_id = TransferId::new();

        let manifest = match build_manifest(&path, CHUNK_SIZE).await {
            Ok(manifest) => manifest,
            Err(e) => {
                tracing::warn!(?path, error = %e, "failed to read file to send");
                let _ = self.event_tx.send(SessionEvent::Failed {
                    transfer_id,
                    reason: e.to_string(),
                });
                return Ok(());
            }
        };
        match OutgoingTransfer::open(transfer_id, path, manifest.clone()).await {
            Ok(outgoing) => {
                self.current_outgoing = Some(outgoing);
                self.control
                    .send(&ControlMessage::TransferOffer {
                        transfer_id,
                        manifest,
                    })
                    .await?;
            }
            Err(e) => {
                let _ = self.event_tx.send(SessionEvent::Failed {
                    transfer_id,
                    reason: e.to_string(),
                });
            }
        }
        Ok(())
    }

    /// Applies one command from the driver (Tier 4.5's channel-not-mutex
    /// boundary between `Session` and whatever's running it).
    async fn handle_session_command(&mut self, cmd: SessionCommand) -> Result<(), SessionError> {
        match cmd {
            SessionCommand::UpdateLayout { peer_bounds } => {
                self.state_machine
                    .set_peer_placement(self.control.peer_node_id, peer_bounds);
                self.control
                    .send(&ControlMessage::LayoutUpdate {
                        sender_bounds: self.local_bounds,
                        peer_bounds,
                    })
                    .await?;
            }
            SessionCommand::SendFile(path) => {
                self.pending_sends.push_back(path);
                if self.current_outgoing.is_none() {
                    self.start_next_pending_send().await?;
                }
            }
            SessionCommand::RespondToOffer {
                transfer_id,
                accept,
            } => {
                if let Some(manifest) = self.pending_offers.remove(&transfer_id) {
                    if accept {
                        self.accept_offer(transfer_id, manifest).await?;
                    } else {
                        self.control
                            .send(&ControlMessage::TransferReject {
                                transfer_id,
                                reason: "declined by user".to_string(),
                            })
                            .await?;
                    }
                }
            }
            SessionCommand::CancelTransfer(transfer_id) => {
                let was_incoming = self.incoming_transfers.remove(&transfer_id).is_some();
                let was_outgoing = self
                    .current_outgoing
                    .as_ref()
                    .is_some_and(|t| t.transfer_id == transfer_id);
                if was_outgoing {
                    self.current_outgoing = None;
                }
                if was_incoming || was_outgoing {
                    self.control
                        .send(&ControlMessage::TransferCancel { transfer_id })
                        .await?;
                }
                if was_outgoing {
                    self.start_next_pending_send().await?;
                }
            }
        }
        Ok(())
    }

    /// Writes one incoming chunk and reports progress. Silently ignored if
    /// `transfer_id` doesn't match an accepted incoming transfer (e.g. a
    /// stray chunk after we cancelled it).
    async fn handle_incoming_chunk(
        &mut self,
        transfer_id: TransferId,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<(), SessionError> {
        let Some(incoming) = self.incoming_transfers.get_mut(&transfer_id) else {
            tracing::debug!(
                ?transfer_id,
                "ignoring chunk for unknown/not-accepted transfer"
            );
            return Ok(());
        };
        let total = incoming.manifest.size;
        if let Err(e) = incoming.write_chunk(offset, &data).await {
            tracing::warn!(?transfer_id, error = %e, "failed to write incoming chunk");
            self.incoming_transfers.remove(&transfer_id);
            let _ = self.event_tx.send(SessionEvent::Failed {
                transfer_id,
                reason: e.to_string(),
            });
            return Ok(());
        }
        let bytes_done = self.incoming_transfers[&transfer_id].bytes_received;
        let _ = self.event_tx.send(SessionEvent::Progress {
            transfer_id,
            bytes_done,
            total,
        });
        self.maybe_finalize_incoming(transfer_id).await;
        Ok(())
    }

    /// Records the peer's claimed hash for a finished transfer, then
    /// finalizes it if every byte has already arrived.
    async fn handle_transfer_complete(
        &mut self,
        transfer_id: TransferId,
        hash: [u8; 32],
    ) -> Result<(), SessionError> {
        let Some(incoming) = self.incoming_transfers.get_mut(&transfer_id) else {
            tracing::debug!(?transfer_id, "TransferComplete for unknown transfer");
            return Ok(());
        };
        incoming.complete_hash = Some(hash);
        self.maybe_finalize_incoming(transfer_id).await;
        Ok(())
    }

    /// Finalizes `transfer_id` if it's ready (Tier 7.5: verify BLAKE3,
    /// rename `.part` into place, restore mtime) — a no-op otherwise,
    /// since `TransferComplete` and the last `Chunk` can arrive in either
    /// order (different connections, no ordering guarantee between them).
    async fn maybe_finalize_incoming(&mut self, transfer_id: TransferId) {
        let Some(incoming) = self.incoming_transfers.get(&transfer_id) else {
            return;
        };
        if !incoming.is_ready_to_finalize() {
            return;
        }
        let mut incoming = self
            .incoming_transfers
            .remove(&transfer_id)
            .expect("just checked it's present");
        match incoming.finalize().await {
            Ok(path) => {
                let _ = self
                    .event_tx
                    .send(SessionEvent::Completed { transfer_id, path });
            }
            Err(e) => {
                tracing::warn!(?transfer_id, error = %e, "transfer failed verification");
                let _ = self.event_tx.send(SessionEvent::Failed {
                    transfer_id,
                    reason: e.to_string(),
                });
            }
        }
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
    use super::{InputEvent, Session, SessionCommand, SessionEvent};
    use crate::config::Config;
    use crate::error::PlatformError;
    use crate::net::bulk::BulkChannel;
    use crate::net::control::ControlChannel;
    use crate::net::tls::{NodeIdentity, Trust};
    use crate::protocol::{
        BulkMessage, ClipboardContent, ClipboardEvent, ControlMessage, KeyCode, Modifiers,
        MouseButton, OsKind,
    };
    use crate::remap::RemapTable;
    use crate::state::{State, StateMachine};
    use crate::topology::{Layout, NodeId, Rect};
    use crate::traits::{ClipboardProvider, InputCapture, InputSink, ScreenInfo};
    use crate::transfer::AcceptPolicy;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
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

    /// Keeps its `start`/`watch`-provided sender alive for as long as the
    /// mock itself lives, unlike `NoopCapture`/`RecordingClipboard`
    /// (which drop theirs immediately, having no need for it since every
    /// other test drives `Session` via direct method calls rather than
    /// the real `run()` select loop). `run()` treats a closed
    /// `capture_rx`/`clipboard_rx` as "the platform layer died" and ends
    /// the session — needed only by
    /// `full_file_transfer_over_loopback`, the one test that runs the
    /// real select loop end to end.
    #[derive(Default)]
    struct KeepAlivePlatform {
        capture_tx: Option<UnboundedSender<InputEvent>>,
        clipboard_tx: Option<UnboundedSender<ClipboardEvent>>,
    }

    impl InputCapture for KeepAlivePlatform {
        fn start(&mut self, sink: UnboundedSender<InputEvent>) -> Result<(), PlatformError> {
            self.capture_tx = Some(sink);
            Ok(())
        }
        fn stop(&mut self) -> Result<(), PlatformError> {
            Ok(())
        }
        fn set_suppression(&mut self, _suppress: bool) -> Result<(), PlatformError> {
            Ok(())
        }
        fn is_healthy(&self) -> bool {
            true
        }
    }

    impl ClipboardProvider for KeepAlivePlatform {
        fn watch(&mut self, sink: UnboundedSender<ClipboardEvent>) -> Result<(), PlatformError> {
            self.clipboard_tx = Some(sink);
            Ok(())
        }
        fn set_text(&mut self, _text: &str) -> Result<(), PlatformError> {
            Ok(())
        }
        fn set_image(&mut self, _png_bytes: &[u8]) -> Result<(), PlatformError> {
            Ok(())
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
        let a_identity = NodeIdentity::generate().expect("a identity");
        let b_identity = NodeIdentity::generate().expect("b identity");

        let b_task = tokio::spawn(async move {
            ControlChannel::accept(
                &listener,
                b_node,
                "b",
                OsKind::Windows,
                &b_identity,
                Trust::OnFirstUse,
            )
            .await
            .expect("b handshake")
        });
        let a = ControlChannel::connect(
            addr,
            a_node,
            "a",
            OsKind::Windows,
            &a_identity,
            Trust::OnFirstUse,
        )
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
        let server_identity = NodeIdentity::generate().expect("server identity");
        let client_identity = NodeIdentity::generate().expect("client identity");
        let server_fingerprint = server_identity.fingerprint;
        let client_fingerprint = client_identity.fingerprint;

        let server = tokio::spawn(async move {
            BulkChannel::accept(&listener, &server_identity, client_fingerprint)
                .await
                .expect("accept")
        });
        let client = BulkChannel::connect(addr, &client_identity, server_fingerprint)
            .await
            .expect("connect");
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
        let (session, _handle) = Session::new(
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

    /// Tier 7.2: real hardware verification (M4's cross-machine test)
    /// found that once `RemoteActive`, the OS clamps the local cursor's
    /// reported position at the edge that triggered the handoff — every
    /// further `MouseMoveAbs` reads the SAME stuck value no matter how
    /// far the physical mouse keeps moving. Relaying that absolute
    /// position would just repeatedly re-pin the peer's cursor at the
    /// entry point; only `MouseDelta` (independent of the OS's clamped
    /// absolute value on macOS, and computed since the last real sample
    /// on Windows — see each platform's `capture.rs`) can carry
    /// continued motion, so it's the only thing relayed once driving.
    #[tokio::test]
    async fn mouse_move_abs_is_never_relayed_but_delta_is_once_remote_active() {
        let (a_control, a_node, mut b_control, b_node) = loopback_pair().await;
        let layout = adjacent_layout(a_node, b_node, true);
        let (mut session, ..) = session_with(a_control, a_node, layout).await;

        session
            .handle_capture_event(InputEvent::MouseMoveAbs { x: 960, y: 540 })
            .await
            .expect("first move");
        session
            .handle_capture_event(InputEvent::MouseMoveAbs { x: 1919, y: 540 })
            .await
            .expect("edge move");
        assert_eq!(session.state(), State::RemoteActive);

        // Drain the ModifierState + Handoff the crossing itself produced.
        for _ in 0..2 {
            b_control.recv().await.expect("recv").expect("not closed");
        }

        // The OS keeps reporting the same clamped position — must not be
        // relayed as a fresh `MouseMove`.
        session
            .handle_capture_event(InputEvent::MouseMoveAbs { x: 1919, y: 541 })
            .await
            .expect("pinned move");

        // A real delta must be relayed immediately, and — since the
        // pinned move above must not have queued anything — must be the
        // very next message on the wire.
        session
            .handle_capture_event(InputEvent::MouseDelta { dx: 5, dy: 0 })
            .await
            .expect("delta");
        let msg = b_control.recv().await.expect("recv").expect("not closed");
        assert_eq!(msg, ControlMessage::MouseDelta { dx: 5, dy: 0 });
    }

    /// The other half of the same fix: reclaim must key off REAL motion
    /// (accumulated deltas), not the OS-visible absolute position, since
    /// that position can be stuck at the edge indefinitely while the user
    /// is very much still moving the mouse.
    #[tokio::test]
    async fn accumulated_delta_can_trigger_reclaim_even_though_os_position_stays_pinned() {
        let (a_control, a_node, b_control, b_node) = loopback_pair().await;
        let layout = adjacent_layout(a_node, b_node, true);
        let (mut session, _sink, suppressed) = session_with(a_control, a_node, layout).await;
        let _b_control = b_control;

        session
            .handle_capture_event(InputEvent::MouseMoveAbs { x: 960, y: 540 })
            .await
            .expect("first move");
        session
            .handle_capture_event(InputEvent::MouseMoveAbs { x: 1919, y: 540 })
            .await
            .expect("edge move");
        assert_eq!(session.state(), State::RemoteActive);

        // Repeating the exact same clamped absolute reading must never
        // reclaim on its own, no matter how many times it repeats.
        for _ in 0..5 {
            session
                .handle_capture_event(InputEvent::MouseMoveAbs { x: 1919, y: 540 })
                .await
                .expect("pinned move");
        }
        assert_eq!(
            session.state(),
            State::RemoteActive,
            "a repeated clamped absolute reading alone must not trigger reclaim"
        );

        // Real leftward motion — via deltas, since that's all the OS can
        // report once pinned — measurably exceeding the reclaim
        // threshold must reclaim, even though no `MouseMoveAbs` ever
        // reported a position off the edge.
        session
            .handle_capture_event(InputEvent::MouseDelta { dx: -10, dy: 0 })
            .await
            .expect("reclaim delta");
        assert_eq!(session.state(), State::LocalActive);
        assert_eq!(
            *suppressed.lock().expect("mutex poisoned"),
            vec![true, false]
        );
    }

    /// Tier 8.1: dragging the peer's tile on the layout canvas both
    /// updates our own state machine and tells the peer where we've put
    /// it, so both sides agree on the shared edge regardless of which one
    /// dragged.
    #[tokio::test]
    async fn update_layout_command_places_peer_and_notifies_them() {
        let (a_control, a_node, mut b_control, b_node) = loopback_pair().await;
        // Deliberately NOT adjacent yet — that's the point of dragging.
        let layout = adjacent_layout(a_node, b_node, true);
        let (mut session, ..) = session_with(a_control, a_node, layout).await;

        let new_peer_bounds = Rect {
            x: 3840,
            y: 0,
            width: 1920,
            height: 1080,
        };
        session
            .handle_session_command(SessionCommand::UpdateLayout {
                peer_bounds: new_peer_bounds,
            })
            .await
            .expect("update layout");

        assert_eq!(session.peer_bounds(), Some(new_peer_bounds));

        let msg = b_control.recv().await.expect("recv").expect("not closed");
        assert_eq!(
            msg,
            ControlMessage::LayoutUpdate {
                sender_bounds: bounds(),
                peer_bounds: new_peer_bounds,
            }
        );
    }

    /// The receiving side of the same feature: the peer's `LayoutUpdate`
    /// must be re-expressed in OUR OWN coordinate space, not applied
    /// verbatim — proven here with a local origin that ISN'T (0, 0),
    /// which is exactly the multi-monitor case (a display to the left of
    /// primary reports negative x) that would silently misplace the peer
    /// if the raw sender-side coordinates were used directly.
    #[tokio::test]
    async fn incoming_layout_update_is_translated_into_our_own_coordinate_space() {
        let (a_control, a_node, b_node) = {
            let (a, a_node, _b, b_node) = loopback_pair().await;
            (a, a_node, b_node)
        };
        let layout = adjacent_layout(a_node, b_node, true);
        let mut local_bounds = bounds();
        local_bounds.x = -500;
        local_bounds.y = 200;
        let sm = StateMachine::new(a_node, local_bounds, layout);
        let capture = NoopCapture {
            suppressed: Arc::new(Mutex::new(Vec::new())),
        };
        let (bulk, _peer_bulk) = bulk_loopback_pair().await;
        let (mut session, _handle) = Session::new(
            sm,
            a_control,
            bulk,
            Box::new(capture),
            Box::new(RecordingSink::default()),
            Box::new(RecordingClipboard::default()),
            &Config::new_default(),
        )
        .expect("session construction");

        // The peer placed us at (2000, 0) in ITS OWN frame, where its own
        // bounds start at (0, 0) — i.e. we're 2000px to its right. From
        // our side (whose own origin is -500, 200, not 0,0), the peer
        // must land exactly `2000`px to our left, at the SAME y as our
        // own origin (zero relative vertical offset), not at some
        // coordinate derived from the sender's raw numbers.
        session
            .handle_control_message(ControlMessage::LayoutUpdate {
                sender_bounds: Rect {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                },
                peer_bounds: Rect {
                    x: 2000,
                    y: 0,
                    width: 1920,
                    height: 1080,
                },
            })
            .await
            .expect("handle layout update");

        assert_eq!(
            session.peer_bounds(),
            Some(Rect {
                x: local_bounds.x - 2000,
                y: local_bounds.y,
                width: 1920,
                height: 1080,
            })
        );
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
        let (mut session, _handle) = Session::new(
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
            .await
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
            .await
            .expect("blob for a rejected offer should not error");

        assert!(
            clipboard
                .set_images
                .lock()
                .expect("mutex poisoned")
                .is_empty()
        );
    }

    /// The M10 demo (Tier 13), end to end at the protocol/session level:
    /// unlike every other test in this module, this one drives BOTH sides
    /// through the real `Session::run` select loop (not direct method
    /// calls) since chunk-by-chunk sending only happens inside it —
    /// proving the whole `SendFile` → `TransferOffer` → `TransferAccept`
    /// → `Chunk`... → `TransferComplete` → verify-and-rename flow works
    /// over real loopback TCP, not just each piece in isolation.
    #[tokio::test]
    async fn full_file_transfer_over_loopback() {
        let (a_control, a_node, b_control, b_node) = loopback_pair().await;
        let a_layout = adjacent_layout(a_node, b_node, true);
        let b_layout = adjacent_layout(b_node, a_node, false);
        let (a_bulk, b_bulk) = bulk_loopback_pair().await;

        let src_dir = tempfile::tempdir().expect("src tempdir");
        let dest_dir = tempfile::tempdir().expect("dest tempdir");
        let src_path = src_dir.path().join("payload.bin");
        let payload: Vec<u8> = (0..2000u32).map(|i| (i % 251) as u8).collect();
        tokio::fs::write(&src_path, &payload)
            .await
            .expect("write payload");

        let a_config = Config::new_default();
        let mut b_config = Config::new_default();
        b_config.accept_policy = AcceptPolicy::AlwaysAccept;
        b_config.download_dir = Some(dest_dir.path().to_path_buf());

        let (a_session, a_handle) = Session::new(
            StateMachine::new(a_node, bounds(), a_layout),
            a_control,
            a_bulk,
            Box::new(KeepAlivePlatform::default()),
            Box::new(RecordingSink::default()),
            Box::new(KeepAlivePlatform::default()),
            &a_config,
        )
        .expect("a session construction");
        let (b_session, mut b_handle) = Session::new(
            StateMachine::new(b_node, bounds(), b_layout),
            b_control,
            b_bulk,
            Box::new(KeepAlivePlatform::default()),
            Box::new(RecordingSink::default()),
            Box::new(KeepAlivePlatform::default()),
            &b_config,
        )
        .expect("b session construction");

        let mut a_join = tokio::spawn(a_session.run());
        let mut b_join = tokio::spawn(b_session.run());

        a_handle
            .command_tx
            .send(SessionCommand::SendFile(src_path))
            .expect("a's command channel still open");

        let received_path = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                tokio::select! {
                    result = &mut a_join => {
                        panic!("a_session.run() ended early: {result:?}");
                    }
                    result = &mut b_join => {
                        panic!("b_session.run() ended early: {result:?}");
                    }
                    event = b_handle.event_rx.recv() => {
                        let Some(event) = event else {
                            let a_result = (&mut a_join).await;
                            let b_result = (&mut b_join).await;
                            panic!(
                                "b's event channel closed unexpectedly; a_session.run() -> {a_result:?}, b_session.run() -> {b_result:?}"
                            );
                        };
                        match event {
                            SessionEvent::Completed { path, .. } => return path,
                            SessionEvent::Failed { reason, .. } => {
                                panic!("transfer failed: {reason}")
                            }
                            SessionEvent::Rejected { reason, .. } => {
                                panic!("transfer rejected: {reason}")
                            }
                            SessionEvent::Progress { .. }
                            | SessionEvent::OfferReceived { .. }
                            | SessionEvent::PeerScreenConfig { .. }
                            | SessionEvent::LayoutChanged { .. } => {}
                        }
                    }
                }
            }
        })
        .await
        .expect("timed out waiting for the transfer to complete");

        let received = tokio::fs::read(&received_path)
            .await
            .expect("read received file");
        assert_eq!(received, payload);
        assert_eq!(received_path, dest_dir.path().join("payload.bin"));
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
