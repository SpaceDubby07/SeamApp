//! Wire protocol: message types, framing, and version negotiation
//! (Tier 6 of the build guide).

pub mod codec;
mod messages;
mod version;

pub use codec::{
    BULK_MAX_FRAME, CONTROL_MAX_FRAME, ProtocolError, bulk_codec, control_codec, decode_frame,
    encode_frame,
};
pub use messages::{
    BulkMessage, ClipboardContent, ClipboardEvent, ControlMessage, FileManifest, Handshake,
    InputEvent, KeyCode, Modifiers, MouseButton, OsKind, TransferId,
};
pub use version::PROTOCOL_VERSION;
