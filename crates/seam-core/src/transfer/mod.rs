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

use serde::{Deserialize, Serialize};

/// Tier 7.5's chunk size: balances syscall overhead against progress
/// granularity. 512 KiB.
pub const CHUNK_SIZE: u32 = 512 * 1024;

/// Minimum gap between `SessionEvent::Progress` emissions for the same
/// transfer. At 512 KiB chunks, a fast LAN transfer produces hundreds of
/// chunks per second — emitting one event per chunk floods the Tauri IPC/
/// render pipeline badly enough that the progress bar visually stalls and
/// then jumps to done, rather than animating smoothly. ~10 Hz is plenty
/// for a human-visible progress bar and cuts event volume by an order of
/// magnitude or more; [`OutgoingTransfer::should_report_progress`] and
/// [`IncomingTransfer::should_report_progress`] apply this, but always let
/// the final chunk through regardless so the bar visibly reaches 100%.
pub const PROGRESS_EMIT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Per-peer policy for incoming transfer offers (Tier 7.5). Stored on
/// [`crate::config::Config`], not globally — v1's single-peer
/// simplification means "the one peer" is implicit rather than keyed by
/// [`crate::topology::NodeId`] (Tier 15 covers what a third machine would
/// need).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AcceptPolicy {
    /// Prompt every time (default) — `Session` emits
    /// [`crate::session::SessionEvent::OfferReceived`] and waits for a
    /// [`crate::session::SessionCommand::RespondToOffer`].
    #[default]
    Ask,
    /// Auto-accept every incoming offer from the paired peer.
    AlwaysAccept,
    /// Silently reject every incoming offer, logging it.
    AlwaysDeny,
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
