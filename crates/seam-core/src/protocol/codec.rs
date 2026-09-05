//! Wire framing: a `u32` length prefix followed by a postcard-encoded body
//! (Tier 6.1 of the build guide). Built on `tokio_util::codec::LengthDelimitedCodec`
//! rather than hand-rolled length parsing.
//!
//! Postcard is a compact, no-schema binary format built for embedded use —
//! a `MouseMove` encodes to roughly 7 bytes vs ~40 for JSON, and more
//! importantly the *decode* cost is near zero (no string parsing) on the
//! latency-critical input path (Tier 6.2).

use bytes::Bytes;
use serde::{Serialize, de::DeserializeOwned};
use tokio_util::codec::LengthDelimitedCodec;

/// Max control-channel frame size: 1 MB. Control traffic is tiny messages
/// (input events, handoff, small clipboard text) — anything this large is
/// almost certainly a malformed length rather than a legitimate message,
/// so we reject it outright rather than risk a huge allocation.
pub const CONTROL_MAX_FRAME: usize = 1024 * 1024;

/// Max bulk-channel frame size: 16 MB, matching the largest single chunk
/// or clipboard blob we'd ever legitimately send (Tier 6.1).
pub const BULK_MAX_FRAME: usize = 16 * 1024 * 1024;

/// Errors from encoding, decoding, or negotiating the wire protocol.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    /// The encoded message exceeds the channel's max frame size.
    #[error("message exceeds max frame size: {0} bytes")]
    FrameTooLarge(usize),

    /// The peer speaks a different, incompatible protocol version.
    #[error("protocol version mismatch: peer speaks v{peer}, we speak v{ours}")]
    VersionMismatch {
        /// The peer's advertised protocol version.
        peer: u16,
        /// The version this build speaks.
        ours: u16,
    },

    /// The peer explicitly rejected our handshake.
    #[error("peer rejected the handshake: {0}")]
    Rejected(String),

    /// Received a handshake message that doesn't fit the expected sequence
    /// (e.g. a `Hello` where a `HelloAck` was expected).
    #[error("unexpected handshake message")]
    UnexpectedHandshakeMessage,

    /// The peer closed the connection cleanly before we expected it to.
    #[error("connection closed by peer")]
    ConnectionClosed,

    /// Decoding a postcard frame failed — almost always a version skew or
    /// a corrupted frame.
    #[error("decode failed: {0}")]
    Decode(#[from] postcard::Error),

    /// The underlying socket errored — this is also how a TLS handshake
    /// failure surfaces (`tokio_rustls` reports certificate-verification
    /// failures, including a pinned-fingerprint mismatch, as an `io::Error`
    /// wrapping the underlying `rustls::Error`).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Building this node's TLS identity/config failed (Tier 7.6, M8).
    #[error(transparent)]
    Tls(#[from] crate::net::tls::TlsError),

    /// A TLS handshake completed with no peer certificate available. Both
    /// `client_config` and `server_config` make client-cert presentation
    /// mandatory, so this should be unreachable in practice — a defensive
    /// variant rather than a panic.
    #[error("TLS handshake completed with no peer certificate")]
    MissingPeerCertificate,
}

/// Builds a [`LengthDelimitedCodec`] for the control channel: `u32`
/// length-prefixed frames, capped at [`CONTROL_MAX_FRAME`].
#[must_use]
pub fn control_codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .max_frame_length(CONTROL_MAX_FRAME)
        .length_field_type::<u32>()
        .new_codec()
}

/// Builds a [`LengthDelimitedCodec`] for the bulk channel: `u32`
/// length-prefixed frames, capped at [`BULK_MAX_FRAME`].
#[must_use]
pub fn bulk_codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .max_frame_length(BULK_MAX_FRAME)
        .length_field_type::<u32>()
        .new_codec()
}

/// Encodes `msg` to postcard bytes, rejecting it if it would exceed
/// `max_frame`.
///
/// # Errors
/// Returns [`ProtocolError::FrameTooLarge`] if the encoded message exceeds
/// `max_frame`, or [`ProtocolError::Decode`]... actually encoding failures
/// surface as [`ProtocolError::Decode`] too, since postcard uses one error
/// type for both directions.
pub fn encode_frame<T: Serialize>(msg: &T, max_frame: usize) -> Result<Bytes, ProtocolError> {
    let bytes = postcard::to_allocvec(msg)?;
    if bytes.len() > max_frame {
        return Err(ProtocolError::FrameTooLarge(bytes.len()));
    }
    Ok(Bytes::from(bytes))
}

/// Decodes a postcard-encoded frame into `T`.
///
/// # Errors
/// Returns [`ProtocolError::Decode`] if `frame` isn't a valid encoding of
/// `T` — a version skew or a corrupted frame.
pub fn decode_frame<T: DeserializeOwned>(frame: &[u8]) -> Result<T, ProtocolError> {
    postcard::from_bytes(frame).map_err(ProtocolError::Decode)
}

#[cfg(test)]
mod tests {
    use super::{decode_frame, encode_frame};
    use crate::protocol::{ControlMessage, MouseButton};

    #[test]
    fn round_trips_a_control_message() {
        let msg = ControlMessage::MouseDown {
            button: MouseButton::Left,
        };
        let bytes = encode_frame(&msg, super::CONTROL_MAX_FRAME).expect("encode");
        let decoded: ControlMessage = decode_frame(&bytes).expect("decode");
        assert_eq!(decoded, msg);
    }

    #[test]
    fn mouse_move_is_compact() {
        // Tier 6.2's claim: a MouseMove should encode to well under the
        // ~40 bytes a JSON equivalent would take.
        let msg = ControlMessage::MouseMove { x: 0.5, y: 0.5 };
        let bytes = encode_frame(&msg, super::CONTROL_MAX_FRAME).expect("encode");
        assert!(bytes.len() < 16, "encoded to {} bytes", bytes.len());
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let msg = ControlMessage::Goodbye {
            reason: "x".repeat(100),
        };
        let result = encode_frame(&msg, 10);
        assert!(matches!(
            result,
            Err(super::ProtocolError::FrameTooLarge(_))
        ));
    }

    #[test]
    fn decoding_garbage_fails_cleanly() {
        let garbage = [0xFFu8; 8];
        let result: Result<ControlMessage, _> = decode_frame(&garbage);
        assert!(result.is_err());
    }
}
