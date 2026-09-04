//! Maps macOS `CGKeyCode` values to and from `seam_core::protocol::KeyCode`.
//!
//! # Why normalize
//! Windows VK codes and macOS `CGKeyCode`s disagree on almost everything —
//! see the equivalent module doc in `seam-platform`'s Windows backend. We
//! convert to the shared `KeyCode` enum on capture and back to the native
//! code on injection (Tier 4.7 of the build guide).
//!
//! # Where these numbers come from
//! `CGKeyCode` values are the classic Carbon `HIToolbox` virtual keycodes
//! (`kVK_ANSI_A` etc.) — unlike Win32 VK codes there's no `windows`-crate
//! equivalent shipping named constants for these via `objc2-core-graphics`,
//! so the raw `u16` values are hardcoded here. They're laid out by
//! physical keyboard position on a US ANSI keyboard, not by character, and
//! have been stable since classic Mac OS.
//!
//! Left/right Shift, Control, Option, and Command are all individually
//! addressable `CGKeyCode`s on macOS — unlike Windows, there's no
//! left/right disambiguation problem here at all.

use seam_core::protocol::KeyCode;

/// Converts a macOS `CGKeyCode` into our normalized `KeyCode`.
///
/// # Returns
/// `KeyCode::Unknown(code)` for keys we don't model (media keys, `fn`,
/// tablet-specific keys, ISO/JIS layout extras).
#[must_use]
// A flat 1:1 mapping table; splitting it up would only make it harder to
// scan against the Carbon HIToolbox keycode table it mirrors.
#[allow(clippy::too_many_lines)]
pub fn cgkeycode_to_keycode(code: u16) -> KeyCode {
    match code {
        0x00 => KeyCode::A,
        0x01 => KeyCode::S,
        0x02 => KeyCode::D,
        0x03 => KeyCode::F,
        0x04 => KeyCode::H,
        0x05 => KeyCode::G,
        0x06 => KeyCode::Z,
        0x07 => KeyCode::X,
        0x08 => KeyCode::C,
        0x09 => KeyCode::V,
        0x0B => KeyCode::B,
        0x0C => KeyCode::Q,
        0x0D => KeyCode::W,
        0x0E => KeyCode::E,
        0x0F => KeyCode::R,
        0x10 => KeyCode::Y,
        0x11 => KeyCode::T,
        0x12 => KeyCode::Digit1,
        0x13 => KeyCode::Digit2,
        0x14 => KeyCode::Digit3,
        0x15 => KeyCode::Digit4,
        0x16 => KeyCode::Digit6,
        0x17 => KeyCode::Digit5,
        0x18 => KeyCode::Equal,
        0x19 => KeyCode::Digit9,
        0x1A => KeyCode::Digit7,
        0x1B => KeyCode::Minus,
        0x1C => KeyCode::Digit8,
        0x1D => KeyCode::Digit0,
        0x1E => KeyCode::RightBracket,
        0x1F => KeyCode::O,
        0x20 => KeyCode::U,
        0x21 => KeyCode::LeftBracket,
        0x22 => KeyCode::I,
        0x23 => KeyCode::P,
        0x25 => KeyCode::L,
        0x26 => KeyCode::J,
        0x27 => KeyCode::Quote,
        0x28 => KeyCode::K,
        0x29 => KeyCode::Semicolon,
        0x2A => KeyCode::Backslash,
        0x2B => KeyCode::Comma,
        0x2C => KeyCode::Slash,
        0x2D => KeyCode::N,
        0x2E => KeyCode::M,
        0x2F => KeyCode::Period,
        0x32 => KeyCode::Backquote,

        0x24 => KeyCode::Enter,
        0x30 => KeyCode::Tab,
        0x31 => KeyCode::Space,
        0x33 => KeyCode::Backspace,
        0x35 => KeyCode::Escape,

        0x37 => KeyCode::LeftMeta,
        0x36 => KeyCode::RightMeta,
        0x38 => KeyCode::LeftShift,
        0x3C => KeyCode::RightShift,
        0x39 => KeyCode::CapsLock,
        0x3A => KeyCode::LeftAlt,
        0x3D => KeyCode::RightAlt,
        0x3B => KeyCode::LeftCtrl,
        0x3E => KeyCode::RightCtrl,

        0x7A => KeyCode::F1,
        0x78 => KeyCode::F2,
        0x63 => KeyCode::F3,
        0x76 => KeyCode::F4,
        0x60 => KeyCode::F5,
        0x61 => KeyCode::F6,
        0x62 => KeyCode::F7,
        0x64 => KeyCode::F8,
        0x65 => KeyCode::F9,
        0x6D => KeyCode::F10,
        0x67 => KeyCode::F11,
        0x6F => KeyCode::F12,

        0x72 => KeyCode::Insert, // "Help" key on Mac keyboards; closest analog
        0x73 => KeyCode::Home,
        0x74 => KeyCode::PageUp,
        0x75 => KeyCode::Delete, // "Forward Delete" — physical Delete on Mac keyboards
        0x77 => KeyCode::End,
        0x79 => KeyCode::PageDown,

        0x7B => KeyCode::ArrowLeft,
        0x7C => KeyCode::ArrowRight,
        0x7D => KeyCode::ArrowDown,
        0x7E => KeyCode::ArrowUp,

        0x41 => KeyCode::NumpadDecimal,
        0x43 => KeyCode::NumpadMultiply,
        0x45 => KeyCode::NumpadAdd,
        0x4B => KeyCode::NumpadDivide,
        0x4C => KeyCode::NumpadEnter,
        0x4E => KeyCode::NumpadSubtract,
        0x52 => KeyCode::Numpad0,
        0x53 => KeyCode::Numpad1,
        0x54 => KeyCode::Numpad2,
        0x55 => KeyCode::Numpad3,
        0x56 => KeyCode::Numpad4,
        0x57 => KeyCode::Numpad5,
        0x58 => KeyCode::Numpad6,
        0x59 => KeyCode::Numpad7,
        0x5B => KeyCode::Numpad8,
        0x5C => KeyCode::Numpad9,

        other => KeyCode::Unknown(u32::from(other)),
    }
}

/// Converts our normalized `KeyCode` back into a macOS `CGKeyCode` for
/// injection via `CGEventCreateKeyboardEvent`.
///
/// # Returns
/// `None` for `KeyCode` variants with no direct Mac keyboard equivalent
/// (`PrintScreen`, `ScrollLock`, `Pause`, `ContextMenu`, `NumLock` — none
/// of these exist on Mac keyboards) or an unrecognized `Unknown` code.
#[must_use]
// See the matching allow on `cgkeycode_to_keycode` above: same flat
// mapping table, just inverted.
#[allow(clippy::too_many_lines)]
pub fn keycode_to_cgkeycode(code: KeyCode) -> Option<u16> {
    let vk = match code {
        KeyCode::A => 0x00,
        KeyCode::S => 0x01,
        KeyCode::D => 0x02,
        KeyCode::F => 0x03,
        KeyCode::H => 0x04,
        KeyCode::G => 0x05,
        KeyCode::Z => 0x06,
        KeyCode::X => 0x07,
        KeyCode::C => 0x08,
        KeyCode::V => 0x09,
        KeyCode::B => 0x0B,
        KeyCode::Q => 0x0C,
        KeyCode::W => 0x0D,
        KeyCode::E => 0x0E,
        KeyCode::R => 0x0F,
        KeyCode::Y => 0x10,
        KeyCode::T => 0x11,
        KeyCode::Digit1 => 0x12,
        KeyCode::Digit2 => 0x13,
        KeyCode::Digit3 => 0x14,
        KeyCode::Digit4 => 0x15,
        KeyCode::Digit6 => 0x16,
        KeyCode::Digit5 => 0x17,
        KeyCode::Equal => 0x18,
        KeyCode::Digit9 => 0x19,
        KeyCode::Digit7 => 0x1A,
        KeyCode::Minus => 0x1B,
        KeyCode::Digit8 => 0x1C,
        KeyCode::Digit0 => 0x1D,
        KeyCode::RightBracket => 0x1E,
        KeyCode::O => 0x1F,
        KeyCode::U => 0x20,
        KeyCode::LeftBracket => 0x21,
        KeyCode::I => 0x22,
        KeyCode::P => 0x23,
        KeyCode::L => 0x25,
        KeyCode::J => 0x26,
        KeyCode::Quote => 0x27,
        KeyCode::K => 0x28,
        KeyCode::Semicolon => 0x29,
        KeyCode::Backslash => 0x2A,
        KeyCode::Comma => 0x2B,
        KeyCode::Slash => 0x2C,
        KeyCode::N => 0x2D,
        KeyCode::M => 0x2E,
        KeyCode::Period => 0x2F,
        KeyCode::Backquote => 0x32,

        KeyCode::Enter => 0x24,
        KeyCode::Tab => 0x30,
        KeyCode::Space => 0x31,
        KeyCode::Backspace => 0x33,
        KeyCode::Escape => 0x35,

        KeyCode::LeftMeta => 0x37,
        KeyCode::RightMeta => 0x36,
        KeyCode::LeftShift => 0x38,
        KeyCode::RightShift => 0x3C,
        KeyCode::CapsLock => 0x39,
        KeyCode::LeftAlt => 0x3A,
        KeyCode::RightAlt => 0x3D,
        KeyCode::LeftCtrl => 0x3B,
        KeyCode::RightCtrl => 0x3E,

        KeyCode::F1 => 0x7A,
        KeyCode::F2 => 0x78,
        KeyCode::F3 => 0x63,
        KeyCode::F4 => 0x76,
        KeyCode::F5 => 0x60,
        KeyCode::F6 => 0x61,
        KeyCode::F7 => 0x62,
        KeyCode::F8 => 0x64,
        KeyCode::F9 => 0x65,
        KeyCode::F10 => 0x6D,
        KeyCode::F11 => 0x67,
        KeyCode::F12 => 0x6F,

        KeyCode::Insert => 0x72,
        KeyCode::Home => 0x73,
        KeyCode::PageUp => 0x74,
        KeyCode::Delete => 0x75,
        KeyCode::End => 0x77,
        KeyCode::PageDown => 0x79,

        KeyCode::ArrowLeft => 0x7B,
        KeyCode::ArrowRight => 0x7C,
        KeyCode::ArrowDown => 0x7D,
        KeyCode::ArrowUp => 0x7E,

        KeyCode::NumpadDecimal => 0x41,
        KeyCode::NumpadMultiply => 0x43,
        KeyCode::NumpadAdd => 0x45,
        KeyCode::NumpadDivide => 0x4B,
        KeyCode::NumpadEnter => 0x4C,
        KeyCode::NumpadSubtract => 0x4E,
        KeyCode::Numpad0 => 0x52,
        KeyCode::Numpad1 => 0x53,
        KeyCode::Numpad2 => 0x54,
        KeyCode::Numpad3 => 0x55,
        KeyCode::Numpad4 => 0x56,
        KeyCode::Numpad5 => 0x57,
        KeyCode::Numpad6 => 0x58,
        KeyCode::Numpad7 => 0x59,
        KeyCode::Numpad8 => 0x5B,
        KeyCode::Numpad9 => 0x5C,

        KeyCode::PrintScreen
        | KeyCode::ScrollLock
        | KeyCode::Pause
        | KeyCode::ContextMenu
        | KeyCode::NumLock
        | KeyCode::Unknown(_) => return None,
    };
    Some(vk)
}

#[cfg(test)]
mod tests {
    use super::{cgkeycode_to_keycode, keycode_to_cgkeycode};
    use seam_core::protocol::KeyCode;

    #[test]
    fn common_keys_roundtrip() {
        for code in [
            KeyCode::A,
            KeyCode::Digit1,
            KeyCode::F5,
            KeyCode::Enter,
            KeyCode::LeftCtrl,
            KeyCode::RightMeta,
            KeyCode::Comma,
        ] {
            let cg = keycode_to_cgkeycode(code).expect("mapped key must have a CGKeyCode");
            assert_eq!(cgkeycode_to_keycode(cg), code);
        }
    }

    #[test]
    fn unmapped_cgkeycode_becomes_unknown() {
        // 0x0A is unassigned in the classic ANSI keycode table.
        assert_eq!(cgkeycode_to_keycode(0x0A), KeyCode::Unknown(0x0A));
    }

    #[test]
    fn keys_with_no_mac_equivalent_return_none() {
        for code in [
            KeyCode::PrintScreen,
            KeyCode::ScrollLock,
            KeyCode::Pause,
            KeyCode::ContextMenu,
            KeyCode::NumLock,
        ] {
            assert_eq!(keycode_to_cgkeycode(code), None);
        }
    }
}
