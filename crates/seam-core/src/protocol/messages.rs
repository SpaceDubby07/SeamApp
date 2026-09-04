//! Local input/clipboard event types and the normalized physical key set.
//!
//! `InputEvent` is the shape events take right after OS capture — local
//! pixel coordinates, no normalization. The state machine and network layer
//! (M2–M4) are responsible for converting these into wire `ControlMessage`s
//! (normalized coordinates, protocol framing). Keeping the two separate
//! means a capture bug can never accidentally leak raw pixels onto the wire.

use serde::{Deserialize, Serialize};

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
