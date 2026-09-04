//! Protocol version negotiation.

/// The wire protocol version this build speaks.
///
/// # Versioning rule
/// Adding a variant at the END of [`crate::protocol::ControlMessage`] is
/// backward-compatible with postcard (older peers will fail to decode it,
/// so gate new variants behind the negotiated version once that matters).
/// Reordering or removing variants is a BREAKING change — bump this.
pub const PROTOCOL_VERSION: u16 = 1;
