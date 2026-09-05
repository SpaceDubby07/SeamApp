//! File transfer (M10, Tier 7.5): chunked send/receive over the bulk
//! channel, BLAKE3 verification, resume, and per-peer accept policy.
//!
//! Sender/receiver state (open file handles, byte offsets, resume
//! bookkeeping) lives here. The actual wire exchange — sending
//! `TransferOffer`/`Chunk`/`TransferComplete` and reacting to the peer's
//! replies — is driven by [`crate::session::Session`], since only it holds
//! the live control/bulk channels; this module deliberately does no
//! networking of its own (Tier 7.1's "no I/O in the state layer" spirit,
//! applied to transfers).

pub mod manifest;
pub mod receiver;
pub mod sender;

pub use manifest::ResumeState;
pub use receiver::IncomingTransfer;
pub use sender::OutgoingTransfer;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::protocol::{FileManifest, TransferId};

/// Tier 7.5's chunk size: balances syscall overhead against progress
/// granularity. 512 KiB.
pub const CHUNK_SIZE: u32 = 512 * 1024;

/// Per-peer policy for incoming transfer offers (Tier 7.5). Stored on
/// [`crate::config::Config`], not globally — v1's single-peer
/// simplification means "the one peer" is implicit rather than keyed by
/// [`crate::topology::NodeId`] (Tier 15 covers what a third machine would
/// need).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AcceptPolicy {
    /// Prompt every time (default) — `Session` emits
    /// [`TransferEvent::OfferReceived`] and waits for a
    /// [`SessionCommand::RespondToOffer`].
    #[default]
    Ask,
    /// Auto-accept every incoming offer from the paired peer.
    AlwaysAccept,
    /// Silently reject every incoming offer, logging it.
    AlwaysDeny,
}

/// Reported out of a running [`crate::session::Session`] to whatever's
/// driving it (a CLI demo today; a Tauri command layer eventually) —
/// nothing in `transfer` or `session` does UI work of its own (Tier 4.5:
/// channels over shared mutexes, applied to the session/UI boundary too).
#[derive(Debug, Clone)]
pub enum TransferEvent {
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

/// Commands a driver sends INTO a running [`crate::session::Session`] —
/// the other half of [`TransferEvent`], since `Session::run` owns the only
/// handle to the live channels and can't be reached by a direct method
/// call once it's running.
#[derive(Debug, Clone)]
pub enum SessionCommand {
    /// Offer `path` to the peer. Queued if a send is already in progress —
    /// v1 sends at most one file at a time (Tier 15's single-peer spirit
    /// applied to transfers too; nothing about the wire protocol prevents
    /// more later).
    SendFile(PathBuf),
    /// Answer a [`TransferEvent::OfferReceived`]. Ignored if `transfer_id`
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

/// Everything that can go wrong reading, writing, or verifying a transfer.
#[derive(Debug, thiserror::Error)]
pub enum TransferError {
    /// A filesystem read/write/rename/metadata call failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The re-hashed received file doesn't match the sender's claimed
    /// hash — Tier 7.5's integrity check failed.
    #[error("received file hash does not match the sender's claimed hash")]
    HashMismatch,
}
