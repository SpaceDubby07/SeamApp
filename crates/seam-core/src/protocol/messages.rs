//! Local input/clipboard event types, the normalized physical key set, and
//! (as of M3) the wire protocol: the handshake and `ControlMessage`/
//! `BulkMessage` themselves (Tier 6.3 of the build guide).
//!
//! `InputEvent` is the shape events take right after OS capture — local
//! pixel coordinates, no normalization. The state machine and network layer
//! are responsible for converting these into wire `ControlMessage`s
//! (normalized coordinates). Keeping the two separate means a capture bug
//! can never accidentally leak raw pixels onto the wire.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::topology::{Display, EdgePoint, NodeId, Rect};

/// A normalized physical key code, independent of OS or keyboard layout.
///
/// Every platform implementation is responsible for mapping its native key
/// representation (Win32 virtual-key codes, macOS `CGKeyCode`) to and from
/// this enum — see `seam-platform`'s `keycodes.rs` modules (M1 for Windows,
/// M5 for macOS). Variant names describe the physical key, not the
/// character it produces, since that's what needs to travel identically
/// between a US and a non-US keyboard layout.
///
/// Variant names are self-describing, so per-variant doc comments would
/// just restate the name — documented as a group here instead.
#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum KeyCode {
    A,
    B,
    C,
    D,
    E,
    F,
    G,
    H,
    I,
    J,
    K,
    L,
    M,
    N,
    O,
    P,
    Q,
    R,
    S,
    T,
    U,
    V,
    W,
    X,
    Y,
    Z,

    Digit0,
    Digit1,
    Digit2,
    Digit3,
    Digit4,
    Digit5,
    Digit6,
    Digit7,
    Digit8,
    Digit9,

    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,

    Escape,
    Tab,
    CapsLock,
    Space,
    Enter,
    Backspace,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,

    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,

    LeftShift,
    RightShift,
    LeftCtrl,
    RightCtrl,
    LeftAlt,
    RightAlt,
    /// Cmd on macOS, the Windows key on Windows.
    LeftMeta,
    /// Cmd on macOS, the Windows key on Windows.
    RightMeta,

    PrintScreen,
    ScrollLock,
    Pause,
    /// The menu / "application" key some keyboards have next to right Ctrl.
    ContextMenu,

    Minus,
    Equal,
    LeftBracket,
    RightBracket,
    Backslash,
    Semicolon,
    Quote,
    Comma,
    Period,
    Slash,
    /// The key to the left of `1` on US layouts (backtick/tilde).
    Backquote,

    NumLock,
    Numpad0,
    Numpad1,
    Numpad2,
    Numpad3,
    Numpad4,
    Numpad5,
    Numpad6,
    Numpad7,
    Numpad8,
    Numpad9,
    NumpadAdd,
    NumpadSubtract,
    NumpadMultiply,
    NumpadDivide,
    NumpadDecimal,
    NumpadEnter,

    /// A physical key we don't (yet) have a normalized variant for. Carries
    /// the raw platform-native code so it can still be logged and, in
    /// principle, round-tripped — but capture/inject code should generally
    /// drop these rather than guess at their meaning.
    Unknown(u32),
}

/// A physical mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MouseButton {
    /// Primary (usually left) button.
    Left,
    /// Secondary (usually right) button.
    Right,
    /// Middle button / wheel click.
    Middle,
    /// First side/"back" button, if present.
    X1,
    /// Second side/"forward" button, if present.
    X2,
}

/// Snapshot of which modifier keys are physically held.
///
/// Sent as its own message immediately before every handoff and reclaim
/// (Tier 7.1) — this, plus releasing all modifiers on every exit from a
/// driving state, is the fix for the stuck-modifier bug class.
// Each field is an independent physical key state, not a mode selector, so
// a state machine/enum split (clippy's usual suggestion here) wouldn't fit —
// this mirrors the wire format field-for-field (Tier 6.3 of the build guide).
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Modifiers {
    /// Either physical Shift key held.
    pub shift: bool,
    /// Either physical Ctrl key held.
    pub ctrl: bool,
    /// Either physical Alt key held (Option on macOS).
    pub alt: bool,
    /// Either physical Meta key held (Cmd on macOS, Win key on Windows).
    pub meta: bool,
    /// Caps Lock is currently toggled on. This is toggle state, not a held
    /// key — track it, don't forward raw press/release for it naively.
    pub caps: bool,
}

/// A raw input event as it comes off OS capture: local pixel coordinates,
/// no normalization, no wire framing. See the module docs for why this is a
/// distinct type from the eventual wire `ControlMessage`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum InputEvent {
    /// Absolute cursor position, in local virtual-desktop pixel coordinates.
    MouseMoveAbs {
        /// X coordinate in local virtual-desktop pixels.
        x: i32,
        /// Y coordinate in local virtual-desktop pixels.
        y: i32,
    },
    /// Relative motion since the last event, for when the receiver has
    /// pointer acceleration or a captured cursor.
    MouseDelta {
        /// Horizontal delta in pixels.
        dx: i32,
        /// Vertical delta in pixels.
        dy: i32,
    },
    /// A mouse button was pressed.
    MouseDown {
        /// Which button.
        button: MouseButton,
    },
    /// A mouse button was released.
    MouseUp {
        /// Which button.
        button: MouseButton,
    },
    /// A scroll wheel or trackpad scroll gesture.
    Scroll {
        /// Horizontal scroll delta.
        dx: i32,
        /// Vertical scroll delta.
        dy: i32,
    },
    /// A key was pressed (or is auto-repeating while held).
    KeyDown {
        /// The physical key.
        code: KeyCode,
        /// `true` if this is an OS auto-repeat, not the initial press.
        repeat: bool,
    },
    /// A key was released.
    KeyUp {
        /// The physical key.
        code: KeyCode,
    },
}

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
// Everything below this line is Tier 6.3 of the build guide: the
// handshake and the two message enums that actually cross the wire.
// `ControlMessage`/`BulkMessage` reuse the local types above (`KeyCode`,
// `Modifiers`, `MouseButton`) and the topology types (`EdgePoint`,
// `Display`, `Rect`, `NodeId`) rather than redefining wire-shaped
// equivalents — Tier 4.3's point about exhaustive `match` applies to all
// of it: add a variant, the compiler finds every place that needs
// updating.

/// Which OS a node is running. Affects nothing about the protocol itself,
/// but matters for diagnostics and for platform-specific UI (e.g. showing
/// "Cmd" vs "Ctrl" in the remap editor).
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
/// actual chunk data (which travels over the bulk channel — Tier 7.5).
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
/// the input-latency-critical control channel (Tier 7.4).
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

/// Every message that can cross the control channel: input (the hot
/// path), handoff, clipboard coordination, transfer coordination, and
/// housekeeping.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ControlMessage {
    // --- Input (the hot path) ---
    /// Cursor position, normalized `0.0..=1.0` within the target's virtual
    /// desktop. NORMALIZED, not pixels — this is what makes mismatched
    /// resolutions and DPI work correctly; the receiver multiplies by its
    /// own bounds.
    MouseMove {
        /// Normalized X, `0.0..=1.0`.
        x: f32,
        /// Normalized Y, `0.0..=1.0`.
        y: f32,
    },
    /// Relative motion, for when the receiver has pointer acceleration or
    /// a captured cursor. Sent alongside `MouseMove` when available.
    MouseDelta {
        /// Horizontal delta.
        dx: i16,
        /// Vertical delta.
        dy: i16,
    },
    /// A mouse button was pressed.
    MouseDown {
        /// Which button.
        button: MouseButton,
    },
    /// A mouse button was released.
    MouseUp {
        /// Which button.
        button: MouseButton,
    },
    /// A scroll wheel or trackpad scroll gesture.
    Scroll {
        /// Horizontal delta.
        dx: i16,
        /// Vertical delta.
        dy: i16,
        /// Whether `dx`/`dy` are high-precision trackpad units rather than
        /// discrete wheel clicks.
        precise: bool,
    },
    /// A key was pressed (or is auto-repeating). Carries a NORMALIZED
    /// physical key code, not a character — remapping happens on the
    /// RECEIVING side, so each machine owns its own remap rules.
    KeyDown {
        /// The physical key.
        code: KeyCode,
        /// `true` if this is an OS auto-repeat, not the initial press.
        repeat: bool,
    },
    /// A key was released.
    KeyUp {
        /// The physical key.
        code: KeyCode,
    },
    /// Authoritative snapshot of which modifiers are physically held. MUST
    /// be sent immediately before every `Handoff` and every `Reclaim` —
    /// the fix for the stuck-modifier class of bug.
    ModifierState {
        /// The full modifier snapshot.
        mods: Modifiers,
    },

    // --- Handoff ---
    /// "You have control now. Put your cursor here."
    Handoff {
        /// Normalized entry point on the receiver's screen.
        entry: EdgePoint,
    },
    /// "I'm taking control back." Sent by the machine reclaiming.
    Reclaim,
    /// Sent by whichever node the escape hotkey was pressed on. Both
    /// nodes immediately release all modifiers and return control local.
    EmergencyRelease,

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
    /// The peer's screen layout changed (monitor plugged in, resolution
    /// change).
    ScreenConfig {
        /// The peer's current displays.
        displays: Vec<Display>,
        /// The peer's current virtual desktop bounds.
        virtual_bounds: Rect,
    },
    /// Graceful shutdown notice.
    Goodbye {
        /// Human-readable reason, for logging.
        reason: String,
    },
    /// The sender has (re)arranged the shared layout canvas (Tier 8.1's
    /// drag-and-snap tiles, M11). Both `Rect`s are in the SENDER's own
    /// coordinate space, which the receiver can't assume matches its own
    /// (a multi-monitor virtual desktop can have a negative-origin
    /// display) — carrying both lets the receiver derive the pure
    /// origin-to-origin offset and re-express it in its own space. See
    /// `Session::handle_layout_update`'s doc comment for the exact math.
    LayoutUpdate {
        /// The sender's own bounds, in the sender's coordinate space.
        sender_bounds: Rect,
        /// Where the sender has placed the RECEIVER, also in the
        /// sender's coordinate space.
        peer_bounds: Rect,
    },
    /// Sent by the machine currently BEING driven once the peer-driven
    /// cursor has been pushed back out through the shared edge it entered
    /// on — the driven-side half of reclaim. The driver watches the
    /// visible cursor on the driven screen (relayed motion, integrated),
    /// never its own suppressed local cursor. On receipt the driver
    /// returns to `LocalActive`: drop suppression, warp its cursor back to
    /// where the user left off, release all modifiers.
    ///
    /// Kept LAST in this enum on purpose — see `PROTOCOL_VERSION`'s
    /// "adding a variant at the end is backward-compatible" note.
    ReleaseBack,
}

/// Messages that cross the bulk channel: file chunks and large clipboard
/// payloads. Kept off the control channel entirely — that's the whole
/// point of the two-channel split (Tier 6.1, Tier 7.5).
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
