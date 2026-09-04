//! Wire protocol and local event types.
//!
//! `messages.rs` currently holds the types needed by the platform trait
//! boundary (`InputEvent`, `KeyCode`, `Modifiers`, `ClipboardEvent`). The
//! full wire protocol — `ControlMessage`, `BulkMessage`, framing, version
//! negotiation — lands in M3 (Tier 6 of `documentation/kvm-app-build-guide.md`)
//! and will build on these same types rather than redefine them.

mod messages;

pub use messages::{ClipboardEvent, InputEvent, KeyCode, Modifiers, MouseButton};
