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
