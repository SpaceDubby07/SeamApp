//! Local clipboard event type and the wire protocol: the handshake and
//! `ControlMessage`/`BulkMessage` themselves.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::topology::NodeId;

/// Local clipboard content, as reported by a `ClipboardProvider` watcher.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClipboardEvent {
    /// Plain text content.
    Text(String),
    /// Image content, tagged with its MIME type (e.g. `image/png`).
    Image {
        /// MIME type of `data`.
        mime: String,
        /// Raw encoded image bytes.
        data: Vec<u8>,
    },
}

// ─────────────────────────── Wire protocol ────────────────────────────
//
// The handshake and the two message enums that actually cross the wire.
// Tier 4.3's point about exhaustive `match` applies to all of it: add a
// variant, the compiler finds every place that needs updating.

/// Which OS a node is running. Affects nothing about the protocol itself,
/// but matters for diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OsKind {
    /// macOS.
    MacOs,
    /// Windows.
    Windows,
}

/// The first messages exchanged on any new control connection, before
/// either side sends anything else.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Handshake {
    /// Sent by the connecting side.
    Hello {
        /// The wire protocol version this build speaks.
        protocol_version: u16,
        /// Stable UUID, generated on first run and persisted.
        node_id: NodeId,
        /// User-facing name, e.g. "Zach's laptop".
        display_name: String,
        /// Which OS this node runs.
        os: OsKind,
        /// The app's own version string, for diagnostics.
        app_version: String,
    },
    /// Sent in response by the accepting side. `accepted: false` means a
    /// version mismatch or an unpaired/unknown peer.
    HelloAck {
        /// The wire protocol version this build speaks.
        protocol_version: u16,
        /// Stable UUID, generated on first run and persisted.
        node_id: NodeId,
        /// User-facing name, e.g. "Zach's Desktop".
        display_name: String,
        /// Which OS this node runs.
        os: OsKind,
        /// Whether the handshake is accepted.
        accepted: bool,
        /// Human-readable reason when `accepted` is `false`.
        reason: Option<String>,
    },
}

/// Stable identity for one file transfer, generated when the sender offers
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TransferId(pub Uuid);

impl TransferId {
    /// Generates a new, random transfer identity.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for TransferId {
    fn default() -> Self {
        Self::new()
    }
}

/// Metadata describing a file offered for transfer, sent ahead of the
/// actual chunk data (which travels over the bulk channel).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileManifest {
    /// The file's name (not a full path — the receiver chooses where it
    /// lands).
    pub name: String,
    /// Total size in bytes.
    pub size: u64,
    /// BLAKE3 hash of the complete file, for integrity verification and
    /// resume matching.
    pub hash: [u8; 32],
    /// Chunk size the sender will use, in bytes.
    pub chunk_size: u32,
    /// Original modification time, unix seconds, preserved on receive if
    /// present.
    pub modified: Option<u64>,
}

/// Clipboard content as it travels on the control channel. Large content
/// (images) is offered here and pulled separately over the bulk channel as
/// a `BulkMessage::ClipboardBlob` — never inline, since that would stall
/// the control channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClipboardContent {
    /// Plain text, small enough to send inline.
    Text(String),
    /// An image too large to inline — the receiver pulls the actual bytes
    /// over the bulk channel.
    ImageOffer {
        /// MIME type of the pending image.
        mime: String,
        /// Size in bytes, so the receiver can apply its size cap before
        /// pulling anything.
        size: u64,
    },
}

/// Every message that can cross the control channel: clipboard
/// coordination, transfer coordination, and housekeeping.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ControlMessage {
    // --- Clipboard ---
    /// A clipboard change to sync. `seq` is monotonic; out-of-order
    /// updates (an older `seq` arriving after a newer one) are ignored.
    ClipboardUpdate {
        /// Monotonic sequence number.
        seq: u64,
        /// The content, or an offer to pull larger content separately.
        content: ClipboardContent,
    },

    // --- Transfer coordination (the data itself goes over bulk) ---
    /// Offers a file for transfer.
    TransferOffer {
        /// Identity for this transfer.
        transfer_id: TransferId,
        /// Metadata for the offered file.
        manifest: FileManifest,
    },
    /// Accepts an offered transfer, optionally resuming from a byte
    /// offset if a matching partial file already exists.
    TransferAccept {
        /// Which transfer this responds to.
        transfer_id: TransferId,
        /// Byte offset to resume from; `0` for a fresh transfer.
        resume_from: u64,
    },
    /// Rejects an offered transfer.
    TransferReject {
        /// Which transfer this responds to.
        transfer_id: TransferId,
        /// Human-readable reason, surfaced to the user.
        reason: String,
    },
    /// Cancels an in-progress transfer.
    TransferCancel {
        /// Which transfer to cancel.
        transfer_id: TransferId,
    },
    /// Announces that every chunk has been sent, with the sender's hash
    /// for the receiver to verify against.
    TransferComplete {
        /// Which transfer completed.
        transfer_id: TransferId,
        /// BLAKE3 hash of the complete file, as computed by the sender.
        hash: [u8; 32],
    },

    // --- Housekeeping ---
    /// Sent periodically; doubles as the latency measurement (the sender's
    /// `Pong` handler computes round-trip time from `sent_at_micros`).
    Ping {
        /// Monotonic sequence number.
        seq: u64,
        /// Send-time timestamp, microseconds since the Unix epoch.
        sent_at_micros: u64,
    },
    /// Echoes a `Ping`'s `seq`/`sent_at_micros` back unchanged.
    Pong {
        /// The `Ping`'s sequence number.
        seq: u64,
        /// The `Ping`'s original send-time timestamp, unchanged.
        sent_at_micros: u64,
    },
    /// Graceful shutdown notice.
    Goodbye {
        /// Human-readable reason, for logging.
        reason: String,
    },
}

/// Messages that cross the bulk channel: file chunks and large clipboard
/// payloads. Kept off the control channel entirely — that's the whole
/// point of the two-channel split.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BulkMessage {
    /// One chunk of file data. `offset` is the byte position in the file,
    /// which is what makes resume work.
    Chunk {
        /// Which transfer this chunk belongs to.
        transfer_id: TransferId,
        /// Byte offset of `data` within the file.
        offset: u64,
        /// The chunk's raw bytes (512 KiB by default).
        data: Vec<u8>,
    },
    /// A clipboard image too large for the control channel.
    ClipboardBlob {
        /// Matches the `seq` from the originating `ClipboardUpdate`.
        seq: u64,
        /// MIME type of `data`.
        mime: String,
        /// Raw encoded image bytes.
        data: Vec<u8>,
    },
}
