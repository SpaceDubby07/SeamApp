//! Session: wires the control channel, the bulk channel, the transfer
//! engine, and clipboard sync together into one runnable async loop.
//!
//! Entirely portable — it only touches `seam-core` types and the
//! `ClipboardProvider` trait, never a concrete OS API.
//!
//! # What's deliberately NOT here
//! - Reconnect on disconnect — `run` simply ends with an error and lets
//!   the caller (`seam-app`'s reconnect-with-backoff supervisor) decide
//!   whether to retry.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::config::Config;
use crate::error::PlatformError;
use crate::net::bulk::BulkChannel;
use crate::net::control::{ControlChannel, now_micros};
use crate::protocol::{
    BulkMessage, ClipboardContent, ClipboardEvent, ControlMessage, FileManifest, ProtocolError,
    TransferId,
};
use crate::traits::ClipboardProvider;
use crate::transfer::manifest::{build_manifest, sanitize_file_name};
use crate::transfer::{AcceptPolicy, CHUNK_SIZE, IncomingTransfer, OutgoingTransfer};

/// Plain text travels inline on the control channel only up to this size.
/// There's no bulk-relay path for text in the wire protocol (only images
/// have an offer/blob split) — oversized text is skipped entirely rather
/// than partially synced, same as an oversized image.
const CLIPBOARD_TEXT_INLINE_MAX_BYTES: usize = 256 * 1024;

/// How often the session sends a heartbeat `Ping`. Also the cadence of the
/// silence check below.
const PING_INTERVAL: Duration = Duration::from_secs(2);

/// If no control message of any kind arrives for this long, the peer is
/// declared dead and the session ends with an error so the app's
/// supervisor can reconnect — without this, a half-open TCP connection
/// (peer's machine slept, no FIN) hangs until the OS retransmit timeout,
/// which is minutes. Three missed [`PING_INTERVAL`] heartbeats.
const PEER_SILENCE_TIMEOUT: Duration = Duration::from_secs(6);

/// An accepted `ClipboardContent::ImageOffer` awaiting its matching
/// `BulkMessage::ClipboardBlob`. Only one can be outstanding at a time — a
/// newer offer simply replaces it, matching the "ignore anything not the
/// latest" spirit of the `seq` ordering rule.
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
    /// A clipboard read/write call failed.
    #[error(transparent)]
    Platform(#[from] PlatformError),
    /// The connection to the peer ended, or went silent for too long. The
    /// caller (`seam-app`'s supervisor) decides whether to reconnect.
    #[error("connection to the peer lost")]
    Disconnected,
}

/// Owns one node's live connection to its peer: the handshaked control
/// channel, the bulk channel, clipboard sync, and the file-transfer
/// engine. Constructing one immediately seeds an on-connect clipboard sync
/// from whatever the local clipboard currently holds (see
/// [`ClipboardProvider::watch`]'s contract) — see [`Session::new`].
pub struct Session {
    control: ControlChannel,
    /// The bulk channel: clipboard images and file chunks travel here,
    /// never on `control` — a multi-MB payload on the control channel
    /// would stall pairing/session housekeeping.
    bulk: BulkChannel,
    clipboard: Box<dyn ClipboardProvider>,
    clipboard_rx: UnboundedReceiver<ClipboardEvent>,
    /// Hard cap on outgoing clipboard content; see
    /// [`Config::clipboard_max_bytes`].
    clipboard_max_bytes: u64,
    /// Monotonic `seq` for our own outgoing `ClipboardUpdate`s.
    next_clipboard_seq: u64,
    /// Highest peer `ClipboardUpdate` `seq` we've accepted (Text applied
    /// immediately; an image offer counts once accepted, not once its blob
    /// arrives) — anything at or below this is a stale/duplicate/
    /// out-of-order update and is ignored.
    last_seen_peer_clipboard_seq: u64,
    /// An accepted image offer waiting on its bulk-channel blob.
    pending_image: Option<PendingClipboardImage>,
    /// Content we just wrote to the local clipboard because the PEER sent
    /// it. Compared against the next local `ClipboardEvent` our own watcher
    /// reports — if it matches, that event is our own write echoing back
    /// rather than a genuine new local change, and must NOT be broadcast
    /// again (without this, two synced machines would ping-pong the same
    /// update back and forth forever).
    last_applied_from_peer: Option<ClipboardEvent>,
    ping_seq: u64,
    /// Files queued to offer once whatever's currently sending (if
    /// anything) finishes — v1 sends one file at a time; nothing about the
    /// wire protocol requires this.
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
    /// paired peer.
    accept_policy: AcceptPolicy,
    /// Where accepted incoming files are written.
    download_dir: PathBuf,
    /// Commands from whatever's driving this session — channels, not a
    /// method call, since `run` owns the only handle to the live channels
    /// once it's running.
    command_rx: UnboundedReceiver<SessionCommand>,
    /// `false` once `command_rx`'s sender (the driver's [`SessionHandle`])
    /// is dropped — same guard pattern as `bulk_open` in `run`, so a
    /// closed channel doesn't turn into a busy-loop.
    commands_open: bool,
    /// Where transfer progress/completion/offers are reported to whatever
    /// is driving this session.
    event_tx: UnboundedSender<SessionEvent>,
    /// Most recent control-channel round-trip (from the last pong), in
    /// microseconds — `None` until the first pong comes back.
    last_rtt_micros: Option<u64>,
    /// Set once a graceful shutdown has been requested (the user hit
    /// Disconnect, or the peer sent `Goodbye`). [`Session::run`] checks it
    /// after each event and returns `Ok(())` — a clean stop, distinct from
    /// the `Err` a dropped connection produces.
    stop_reason: Option<&'static str>,
    /// When the last control message of any kind arrived. The health
    /// check in [`Session::run`] compares it against `peer_silence_timeout`
    /// to catch a half-open connection fast. Seeded when `run` starts.
    last_control_activity: Instant,
    /// How long the peer can be silent before the health check ends the
    /// session. [`PEER_SILENCE_TIMEOUT`] in production; tests shorten it.
    peer_silence_timeout: Duration,
}

/// Reported out of a running [`Session`] to whatever's driving it (a Tauri
/// command layer) — nothing in `session` or `transfer` does UI work of its
/// own.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type")]
pub enum SessionEvent {
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
    /// bar. Emitted at most once per chunk. Carries `name`/`incoming` on
    /// every tick (not just once) so the Transfers panel can render a row
    /// for a transfer it never saw an `OfferReceived` for — an outgoing
    /// send, or an incoming one under [`AcceptPolicy::AlwaysAccept`].
    Progress {
        /// Which transfer this is progress for.
        transfer_id: TransferId,
        /// The file's name, for the transfer row's label.
        name: String,
        /// `true` if we're receiving this file, `false` if sending it.
        incoming: bool,
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
    /// Connection health for the status bar. Sent on every pong (~every
    /// 2s, so the latency reading stays fresh).
    Status {
        /// Most recent control-channel round-trip, in microseconds —
        /// `None` until the first pong comes back.
        rtt_micros: Option<u64>,
    },
}

/// Commands a driver sends INTO a running [`Session`] — the other half of
/// [`SessionEvent`], since `Session::run` owns the only handle to the live
/// channels and can't be reached by a direct method call once it's
/// running.
#[derive(Debug, Clone)]
pub enum SessionCommand {
    /// Offer `path` to the peer. Queued if a send is already in progress —
    /// v1 sends at most one file at a time; nothing about the wire
    /// protocol prevents more later.
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
    /// Ends the session cleanly (the user hit Disconnect): tells the peer
    /// with a `Goodbye` and lets [`Session::run`] return `Ok(())`.
    Shutdown,
}

/// The other end of a running [`Session`]'s command/event channels —
/// returned alongside it from [`Session::new`] so a driver (a Tauri
/// command layer) can send it work and observe transfer progress without
/// blocking `run`'s select loop.
pub struct SessionHandle {
    /// Send [`SessionCommand`]s into the running session.
    pub command_tx: UnboundedSender<SessionCommand>,
    /// Receive [`SessionEvent`]s from the running session.
    pub event_rx: UnboundedReceiver<SessionEvent>,
}

impl Session {
    /// Builds a session from an already-handshaked `control` channel.
    ///
    /// `bulk` is this session's already-connected bulk channel — clipboard
    /// images and file chunks travel there. `clipboard` is this machine's
    /// clipboard watcher/setter; its `watch` call immediately seeds an
    /// on-connect sync if the local clipboard already holds something (see
    /// [`ClipboardProvider::watch`]'s contract). `config` supplies this
    /// machine's clipboard size cap and transfer accept policy/download
    /// directory — `config.node_id`/`display_name` already went into
    /// `control`'s handshake before this call.
    ///
    /// Returns the session alongside a [`SessionHandle`] — the command/
    /// event channel a driver uses to queue file sends and observe
    /// transfer progress while `run` is blocking on its select loop.
    ///
    /// # Errors
    /// Returns an error if `clipboard` fails to start watching (e.g. a
    /// missing OS permission).
    pub fn new(
        control: ControlChannel,
        bulk: BulkChannel,
        mut clipboard: Box<dyn ClipboardProvider>,
        config: &Config,
    ) -> Result<(Self, SessionHandle), PlatformError> {
        let (clipboard_tx, clipboard_rx) = tokio::sync::mpsc::unbounded_channel();
        clipboard.watch(clipboard_tx)?;

        let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel();
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();

        let session = Self {
            control,
            bulk,
            clipboard,
            clipboard_rx,
            clipboard_max_bytes: config.clipboard_max_bytes,
            next_clipboard_seq: 0,
            last_seen_peer_clipboard_seq: 0,
            pending_image: None,
            last_applied_from_peer: None,
            ping_seq: 0,
            pending_sends: VecDeque::new(),
            current_outgoing: None,
            incoming_transfers: HashMap::new(),
            pending_offers: HashMap::new(),
            accept_policy: config.accept_policy,
            download_dir: config.resolved_download_dir(),
            command_rx,
            commands_open: true,
            event_tx,
            last_rtt_micros: None,
            stop_reason: None,
            last_control_activity: Instant::now(),
            peer_silence_timeout: PEER_SILENCE_TIMEOUT,
        };
        let handle = SessionHandle {
            command_tx,
            event_rx,
        };
        Ok((session, handle))
    }

    /// Test-only: the graceful-stop reason, once one has been requested.
    #[cfg(test)]
    fn stop_reason(&self) -> Option<&'static str> {
        self.stop_reason
    }

    /// Test-only: when the last control message arrived.
    #[cfg(test)]
    fn last_control_activity(&self) -> Instant {
        self.last_control_activity
    }

    /// Test-only: shorten the health check's silence tolerance so a
    /// "peer went silent" test doesn't take six real seconds.
    #[cfg(test)]
    fn set_peer_silence_timeout(&mut self, timeout: Duration) {
        self.peer_silence_timeout = timeout;
    }

    /// Records a heartbeat round-trip and pushes a fresh
    /// [`SessionEvent::Status`] so the status bar's latency reading stays
    /// current.
    fn on_pong(&mut self, seq: u64, sent_at_micros: u64) {
        let rtt_micros = now_micros().saturating_sub(sent_at_micros);
        tracing::debug!(seq, rtt_micros, "pong received");
        self.last_rtt_micros = Some(rtt_micros);
        let _ = self.event_tx.send(SessionEvent::Status {
            rtt_micros: Some(rtt_micros),
        });
    }

    /// Runs the session until the connection ends or an unrecoverable
    /// error occurs: reads clipboard events and control/bulk-channel
    /// messages, sends periodic heartbeat pings, and drains outgoing
    /// transfer chunks.
    ///
    /// # Errors
    /// Returns an error once the peer disconnects or goes silent (the
    /// app's supervisor turns that into a reconnect), or if an underlying
    /// network/platform call fails. Returns `Ok(())` on a graceful stop
    /// (`SessionCommand::Shutdown` or a peer `Goodbye`).
    pub async fn run(mut self) -> Result<(), SessionError> {
        let mut ping_interval = tokio::time::interval(PING_INTERVAL);
        ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick fires immediately; skip it so we don't ping before
        // the peer has even seen us as connected.
        ping_interval.tick().await;
        // Checks the peer hasn't gone silent (half-open connection). A
        // third of the silence timeout, so production lands at the same
        // 2s cadence as the ping while tests can shrink it.
        let mut health_interval = tokio::time::interval(self.peer_silence_timeout / 3);
        health_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        health_interval.tick().await;
        // Don't count the pre-`run` construction gap against the peer.
        self.last_control_activity = Instant::now();

        // Once the bulk channel closes, `recv()` would return `None`
        // immediately forever — this guard stops polling it rather than
        // busy-looping. Losing bulk sync degrades clipboard images and
        // file transfers only; the control channel stays authoritative
        // for whether the session as a whole is still alive.
        let mut bulk_open = true;

        loop {
            tokio::select! {
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
                            return Err(SessionError::Disconnected);
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
                // Sends the next outgoing chunk one at a time, ready every
                // tick this select loop runs whenever a transfer is
                // actively sending — so a multi-gigabyte file never
                // monopolizes this task for longer than one chunk write,
                // keeping it interleaved with every other branch above.
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
                _ = health_interval.tick() => {
                    if self.last_control_activity.elapsed() > self.peer_silence_timeout {
                        tracing::warn!(
                            silent_for_ms = u64::try_from(
                                self.last_control_activity.elapsed().as_millis()
                            ).unwrap_or(u64::MAX),
                            "peer went silent; ending session so the supervisor can reconnect"
                        );
                        return Err(SessionError::Disconnected);
                    }
                }
            }

            if let Some(reason) = self.stop_reason {
                tracing::info!(reason, "session ended gracefully");
                return Ok(());
            }
        }
    }

    /// Processes one message received from the peer: handshake-adjacent
    /// housekeeping (ping/pong), clipboard sync, transfer coordination,
    /// and graceful shutdown.
    ///
    /// # Errors
    /// Returns an error if a resulting action fails (network send or
    /// clipboard write).
    pub async fn handle_control_message(
        &mut self,
        msg: ControlMessage,
    ) -> Result<(), SessionError> {
        let now = Instant::now();
        // Any message — even the peer's own `Ping` — proves it's alive.
        self.last_control_activity = now;
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
                self.on_pong(seq, sent_at_micros);
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
            ControlMessage::Goodbye { reason } => self.on_peer_goodbye(&reason),
        }
        Ok(())
    }

    /// Handles one clipboard change our own `ClipboardProvider` reported —
    /// either a genuine local edit, or the initial "here's what's already
    /// on the clipboard" event `watch` fires on startup.
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

    /// Handles a `TransferOffer` from the peer, per `accept_policy`:
    /// auto-reject, auto-accept, or park it in `pending_offers` and tell
    /// the driver to ask the user.
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
        let name = outgoing.manifest.name.clone();

        match outgoing.read_next_chunk().await {
            Ok(Some((offset, data))) => {
                self.bulk
                    .send(&BulkMessage::Chunk {
                        transfer_id,
                        offset,
                        data,
                    })
                    .await?;
                // Throttled (see `PROGRESS_EMIT_INTERVAL`'s docs) — one
                // event per 512 KiB chunk floods the IPC/render pipeline
                // on a fast LAN. Always let the last chunk through
                // regardless, so the bar visibly reaches 100%.
                let Some(outgoing) = self.current_outgoing.as_mut() else {
                    return Ok(());
                };
                let bytes_done = outgoing.bytes_sent;
                let is_last_chunk = bytes_done >= total;
                if outgoing.should_report_progress() || is_last_chunk {
                    let _ = self.event_tx.send(SessionEvent::Progress {
                        transfer_id,
                        name,
                        incoming: false,
                        bytes_done,
                        total,
                    });
                }
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

    /// Applies one command from the driver (a channel, not a method call,
    /// since `run` owns the only handle to the live session once it's
    /// running).
    async fn handle_session_command(&mut self, cmd: SessionCommand) -> Result<(), SessionError> {
        match cmd {
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
            SessionCommand::Shutdown => self.begin_shutdown().await?,
        }
        Ok(())
    }

    /// The receive half of a graceful stop: a peer `Goodbye` tears the
    /// session down cleanly rather than being merely logged.
    fn on_peer_goodbye(&mut self, reason: &str) {
        tracing::info!(reason, "peer sent goodbye; closing session");
        self.stop_reason = Some("peer goodbye");
    }

    /// The local half of a graceful stop: tell the peer with a `Goodbye`
    /// (best-effort — if the socket's already gone it'll see the close
    /// anyway) and arm `run`'s clean `Ok(())` exit.
    async fn begin_shutdown(&mut self) -> Result<(), SessionError> {
        tracing::info!("local shutdown requested");
        let _ = self
            .control
            .send(&ControlMessage::Goodbye {
                reason: "peer disconnected".to_string(),
            })
            .await;
        self.stop_reason = Some("local shutdown");
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
        let name = incoming.manifest.name.clone();
        if let Err(e) = incoming.write_chunk(offset, &data).await {
            tracing::warn!(?transfer_id, error = %e, "failed to write incoming chunk");
            self.incoming_transfers.remove(&transfer_id);
            let _ = self.event_tx.send(SessionEvent::Failed {
                transfer_id,
                reason: e.to_string(),
            });
            return Ok(());
        }
        // Throttled, same as the outgoing side — see
        // `PROGRESS_EMIT_INTERVAL`'s docs.
        let Some(incoming) = self.incoming_transfers.get_mut(&transfer_id) else {
            return Ok(());
        };
        let bytes_done = incoming.bytes_received;
        let is_last_chunk = bytes_done >= total;
        if incoming.should_report_progress() || is_last_chunk {
            let _ = self.event_tx.send(SessionEvent::Progress {
                transfer_id,
                name,
                incoming: true,
                bytes_done,
                total,
            });
        }
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

    /// Finalizes `transfer_id` if it's ready (verify BLAKE3, rename
    /// `.part` into place, restore mtime) — a no-op otherwise, since
    /// `TransferComplete` and the last `Chunk` can arrive in either order
    /// (different connections, no ordering guarantee between them).
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
}

#[cfg(test)]
mod tests {
    use super::{Session, SessionCommand, SessionEvent};
    use crate::config::Config;
    use crate::error::PlatformError;
    use crate::net::bulk::BulkChannel;
    use crate::net::control::ControlChannel;
    use crate::net::tls::{NodeIdentity, Trust};
    use crate::protocol::{BulkMessage, ClipboardContent, ClipboardEvent, ControlMessage, OsKind};
    use crate::traits::ClipboardProvider;
    use crate::transfer::AcceptPolicy;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tokio::sync::mpsc::UnboundedSender;

    /// Keeps its `watch`-provided sender alive for as long as the mock
    /// itself lives, unlike `RecordingClipboard` (which drops its sender
    /// immediately, having no need for it since every other test drives
    /// `Session` via direct method calls rather than the real `run()`
    /// select loop). `run()` treats a closed `clipboard_rx` as "the
    /// platform layer died" and ends the session — needed only by the
    /// two tests that run the real select loop end to end.
    #[derive(Default)]
    struct KeepAliveClipboard {
        clipboard_tx: Option<UnboundedSender<ClipboardEvent>>,
    }

    impl ClipboardProvider for KeepAliveClipboard {
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

    /// Builds a handshaked `ControlChannel` pair over real loopback TCP.
    async fn loopback_pair() -> (ControlChannel, ControlChannel) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let a_node = crate::topology::NodeId::new();
        let b_node = crate::topology::NodeId::new();
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
        (a, b)
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

    async fn session_with(control: ControlChannel) -> Session {
        let (session, _clipboard) =
            session_with_full(control, Config::new_default(), RecordingClipboard::default()).await;
        session
    }

    async fn session_with_clipboard(
        control: ControlChannel,
        clipboard: RecordingClipboard,
    ) -> (Session, RecordingClipboard) {
        session_with_full(control, Config::new_default(), clipboard).await
    }

    async fn session_with_full(
        control: ControlChannel,
        config: Config,
        clipboard: RecordingClipboard,
    ) -> (Session, RecordingClipboard) {
        // Only one end is ever driven directly in these tests (via
        // `handle_local_clipboard_event`/`handle_bulk_message`, not the
        // real `run()` select loop), so the peer end just needs to stay
        // alive — kept in the returned tuple's drop scope by virtue of
        // `bulk_loopback_pair`'s server task, not read from here.
        let (bulk, _peer_bulk) = bulk_loopback_pair().await;
        let (session, _handle) =
            Session::new(control, bulk, Box::new(clipboard.clone()), &config)
                .expect("session construction");
        (session, clipboard)
    }

    #[tokio::test]
    async fn shutdown_command_sends_goodbye_and_stops() {
        let (a_control, mut b_control) = loopback_pair().await;
        let mut session = session_with(a_control).await;

        session
            .handle_session_command(SessionCommand::Shutdown)
            .await
            .expect("shutdown");

        assert_eq!(session.stop_reason(), Some("local shutdown"));

        // A `Goodbye` reached the peer.
        let saw_goodbye = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match b_control.recv().await {
                    Ok(Some(ControlMessage::Goodbye { .. })) => break true,
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break false,
                }
            }
        })
        .await
        .expect("timed out waiting for Goodbye");
        assert!(saw_goodbye, "peer never received a Goodbye");
    }

    /// Health supervisor: a peer that goes silent — no messages at all,
    /// not even its own pings — ends the session with an error so the
    /// app's reconnect supervisor takes over, rather than hanging on a
    /// half-open socket until the OS retransmit timeout.
    #[tokio::test]
    async fn silent_peer_ends_the_session_with_an_error() {
        let (a_control, b_control) = loopback_pair().await;
        let (a_bulk, _b_bulk) = bulk_loopback_pair().await;

        // `KeepAliveClipboard` retains the clipboard sender so `run`
        // doesn't immediately exit on a closed channel — we want it to
        // reach the health check.
        let (mut session, _handle) = Session::new(
            a_control,
            a_bulk,
            Box::new(KeepAliveClipboard::default()),
            &Config::new_default(),
        )
        .expect("session construction");
        session.set_peer_silence_timeout(Duration::from_millis(150));

        // Hold the peer's sockets open but never read or write them —
        // `a`'s pings buffer, and `a` sees nothing come back.
        let _b_control = b_control;

        let outcome = tokio::time::timeout(Duration::from_secs(5), session.run()).await;
        assert!(
            matches!(outcome, Ok(Err(_))),
            "expected the session to end with an error once the peer went silent, got {outcome:?}"
        );
    }

    /// Any control message — including the peer's own `Ping` — resets the
    /// silence timer that the health supervisor watches.
    #[tokio::test]
    async fn any_control_message_refreshes_the_liveness_timestamp() {
        let (a_control, b_control) = loopback_pair().await;
        let mut session = session_with(a_control).await;
        let _b_control = b_control;

        let before = session.last_control_activity();
        tokio::time::sleep(Duration::from_millis(10)).await;
        session
            .handle_control_message(ControlMessage::Ping {
                seq: 1,
                sent_at_micros: 0,
            })
            .await
            .expect("ping");

        assert!(session.last_control_activity() > before);
    }

    /// The receive side of the same flow: a peer `Goodbye` tears the
    /// session down cleanly rather than being merely logged.
    #[tokio::test]
    async fn peer_goodbye_stops_the_session() {
        let (a_control, b_control) = loopback_pair().await;
        let mut session = session_with(a_control).await;
        let _b_control = b_control;

        session
            .handle_control_message(ControlMessage::Goodbye {
                reason: "peer disconnected".to_string(),
            })
            .await
            .expect("goodbye");

        assert_eq!(session.stop_reason(), Some("peer goodbye"));
    }

    #[tokio::test]
    async fn local_text_change_is_synced_to_the_peer() {
        let (a_control, mut b_control) = loopback_pair().await;
        let mut session = session_with(a_control).await;

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

    /// An image never touches the control channel — only an offer does,
    /// with the bytes themselves following on the bulk channel.
    #[tokio::test]
    async fn local_image_change_is_offered_on_control_then_sent_on_bulk() {
        let (a_control, mut b_control) = loopback_pair().await;
        let (a_bulk, mut b_bulk) = bulk_loopback_pair().await;
        let (session, _handle) = Session::new(
            a_control,
            a_bulk,
            Box::new(RecordingClipboard::default()),
            &Config::new_default(),
        )
        .expect("session construction");
        let mut session = session;

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
        let (a_control, mut b_control) = loopback_pair().await;
        let (mut session, clipboard) =
            session_with_clipboard(a_control, RecordingClipboard::default()).await;

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

    /// Content over the size limit is skipped entirely — not partially
    /// sent — and doesn't consume a `seq`.
    #[tokio::test]
    async fn oversized_text_is_not_synced() {
        let (a_control, mut b_control) = loopback_pair().await;
        let mut session = session_with(a_control).await;

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

    /// The `seq` rule: an update at or below the highest `seq` already
    /// accepted is ignored.
    #[tokio::test]
    async fn stale_clipboard_update_is_ignored() {
        let (a_control, b_control) = loopback_pair().await;
        let (mut session, clipboard) =
            session_with_clipboard(a_control, RecordingClipboard::default()).await;
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
        let (a_control, b_control) = loopback_pair().await;
        let (mut session, clipboard) =
            session_with_clipboard(a_control, RecordingClipboard::default()).await;
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
        let (a_control, b_control) = loopback_pair().await;
        let mut config = Config::new_default();
        config.clipboard_max_bytes = 10;
        let (mut session, clipboard) =
            session_with_full(a_control, config, RecordingClipboard::default()).await;
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

    /// End to end at the protocol/session level: unlike every other test
    /// in this module, this one drives BOTH sides through the real
    /// `Session::run` select loop (not direct method calls) since
    /// chunk-by-chunk sending only happens inside it — proving the whole
    /// `SendFile` → `TransferOffer` → `TransferAccept` → `Chunk`... →
    /// `TransferComplete` → verify-and-rename flow works over real
    /// loopback TCP, not just each piece in isolation.
    #[tokio::test]
    async fn full_file_transfer_over_loopback() {
        let (a_control, b_control) = loopback_pair().await;
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
            a_control,
            a_bulk,
            Box::new(KeepAliveClipboard::default()),
            &a_config,
        )
        .expect("a session construction");
        let (b_session, mut b_handle) = Session::new(
            b_control,
            b_bulk,
            Box::new(KeepAliveClipboard::default()),
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
                            | SessionEvent::Status { .. }
                            | SessionEvent::OfferReceived { .. } => {}
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
}
