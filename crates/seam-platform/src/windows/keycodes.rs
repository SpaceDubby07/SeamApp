//! Maps Win32 virtual-key codes to and from `seam_core::protocol::KeyCode`.
//!
//! # Why normalize
//! Windows VK codes and macOS `CGKeyCode`s disagree on almost everything.
//! Sending a raw VK code to a Mac would inject garbage. We convert to the
//! shared `KeyCode` enum on capture and back to the native code on
//! injection (Tier 4.7 of the build guide).
//!
//! # The left/right modifier problem
//! `WH_KEYBOARD_LL` reports Ctrl/Alt as the generic `VK_CONTROL`/`VK_MENU`
//! for both physical keys; the `LLKHF_EXTENDED` flag distinguishes them
//! (right Ctrl/Alt are "extended" keys, left are not — see
//! `left_or_right_ctrl_alt` in `capture.rs`). Shift is the odd one out:
//! Windows does *not* set the extended flag for either physical Shift key,
//! so left/right Shift has to be disambiguated by hardware scan code
//! instead (0x2A = left, 0x36 = right). This file only handles the
//! extended-flag-independent mapping; `capture.rs` resolves Ctrl/Alt/Shift
//! laterality before calling in here.

use seam_core::protocol::KeyCode;

/// Converts a Win32 virtual-key code into our normalized `KeyCode`.
///
/// Left/right Ctrl, Alt, and Shift are handled by the caller (`capture.rs`)
/// before this is reached, since disambiguating them needs the scan code
/// and extended-key flag, not just the VK code — see the module docs.
///
/// # Returns
/// `KeyCode::Unknown(vk)` for VK codes we don't model (media keys,
/// IME-specific keys, OEM keys not in the mapping below). Callers should
/// generally still forward these rather than silently dropping them, since
/// `Unknown` round-trips.
#[must_use]
// This is a flat 1:1 mapping table; splitting it up or naming every VK_*
// import individually would only make it harder to scan against the Win32
// header it mirrors.
#[allow(clippy::too_many_lines, clippy::wildcard_imports)]
pub fn vk_to_keycode(vk: u16) -> KeyCode {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;

    match VIRTUAL_KEY(vk) {
        VK_A => KeyCode::A,
        VK_B => KeyCode::B,
        VK_C => KeyCode::C,
        VK_D => KeyCode::D,
        VK_E => KeyCode::E,
        VK_F => KeyCode::F,
        VK_G => KeyCode::G,
        VK_H => KeyCode::H,
        VK_I => KeyCode::I,
        VK_J => KeyCode::J,
        VK_K => KeyCode::K,
        VK_L => KeyCode::L,
        VK_M => KeyCode::M,
        VK_N => KeyCode::N,
        VK_O => KeyCode::O,
        VK_P => KeyCode::P,
        VK_Q => KeyCode::Q,
        VK_R => KeyCode::R,
        VK_S => KeyCode::S,
        VK_T => KeyCode::T,
        VK_U => KeyCode::U,
        VK_V => KeyCode::V,
        VK_W => KeyCode::W,
        VK_X => KeyCode::X,
        VK_Y => KeyCode::Y,
        VK_Z => KeyCode::Z,

        VK_0 => KeyCode::Digit0,
        VK_1 => KeyCode::Digit1,
        VK_2 => KeyCode::Digit2,
        VK_3 => KeyCode::Digit3,
        VK_4 => KeyCode::Digit4,
        VK_5 => KeyCode::Digit5,
        VK_6 => KeyCode::Digit6,
        VK_7 => KeyCode::Digit7,
        VK_8 => KeyCode::Digit8,
        VK_9 => KeyCode::Digit9,

        VK_F1 => KeyCode::F1,
        VK_F2 => KeyCode::F2,
        VK_F3 => KeyCode::F3,
        VK_F4 => KeyCode::F4,
        VK_F5 => KeyCode::F5,
        VK_F6 => KeyCode::F6,
        VK_F7 => KeyCode::F7,
        VK_F8 => KeyCode::F8,
        VK_F9 => KeyCode::F9,
        VK_F10 => KeyCode::F10,
        VK_F11 => KeyCode::F11,
        VK_F12 => KeyCode::F12,

        VK_ESCAPE => KeyCode::Escape,
        VK_TAB => KeyCode::Tab,
        VK_CAPITAL => KeyCode::CapsLock,
        VK_SPACE => KeyCode::Space,
        VK_RETURN => KeyCode::Enter,
        VK_BACK => KeyCode::Backspace,
        VK_DELETE => KeyCode::Delete,
        VK_INSERT => KeyCode::Insert,
        VK_HOME => KeyCode::Home,
        VK_END => KeyCode::End,
        VK_PRIOR => KeyCode::PageUp,
        VK_NEXT => KeyCode::PageDown,

        VK_UP => KeyCode::ArrowUp,
        VK_DOWN => KeyCode::ArrowDown,
        VK_LEFT => KeyCode::ArrowLeft,
        VK_RIGHT => KeyCode::ArrowRight,

        VK_LSHIFT => KeyCode::LeftShift,
        VK_RSHIFT => KeyCode::RightShift,
        VK_LCONTROL => KeyCode::LeftCtrl,
        VK_RCONTROL => KeyCode::RightCtrl,
        VK_LMENU => KeyCode::LeftAlt,
        VK_RMENU => KeyCode::RightAlt,
        VK_LWIN => KeyCode::LeftMeta,
        VK_RWIN => KeyCode::RightMeta,

        VK_SNAPSHOT => KeyCode::PrintScreen,
        VK_SCROLL => KeyCode::ScrollLock,
        VK_PAUSE => KeyCode::Pause,
        VK_APPS => KeyCode::ContextMenu,

        VK_OEM_MINUS => KeyCode::Minus,
        VK_OEM_PLUS => KeyCode::Equal,
        VK_OEM_4 => KeyCode::LeftBracket,
        VK_OEM_6 => KeyCode::RightBracket,
        VK_OEM_5 => KeyCode::Backslash,
        VK_OEM_1 => KeyCode::Semicolon,
        VK_OEM_7 => KeyCode::Quote,
        VK_OEM_COMMA => KeyCode::Comma,
        VK_OEM_PERIOD => KeyCode::Period,
        VK_OEM_2 => KeyCode::Slash,
        VK_OEM_3 => KeyCode::Backquote,

        VK_NUMLOCK => KeyCode::NumLock,
        VK_NUMPAD0 => KeyCode::Numpad0,
        VK_NUMPAD1 => KeyCode::Numpad1,
        VK_NUMPAD2 => KeyCode::Numpad2,
        VK_NUMPAD3 => KeyCode::Numpad3,
        VK_NUMPAD4 => KeyCode::Numpad4,
        VK_NUMPAD5 => KeyCode::Numpad5,
        VK_NUMPAD6 => KeyCode::Numpad6,
        VK_NUMPAD7 => KeyCode::Numpad7,
        VK_NUMPAD8 => KeyCode::Numpad8,
        VK_NUMPAD9 => KeyCode::Numpad9,
        VK_ADD => KeyCode::NumpadAdd,
        VK_SUBTRACT => KeyCode::NumpadSubtract,
        VK_MULTIPLY => KeyCode::NumpadMultiply,
        VK_DIVIDE => KeyCode::NumpadDivide,
        VK_DECIMAL => KeyCode::NumpadDecimal,

        other => KeyCode::Unknown(u32::from(other.0)),
    }
}

/// Converts our normalized `KeyCode` back into a Win32 virtual-key code for
/// injection via `SendInput`.
///
/// # Returns
/// `None` for `KeyCode` variants `SendInput` can't target directly
/// (`NumpadEnter` shares `VK_RETURN` with plain Enter and is disambiguated
/// by the extended-key flag at the injection site instead — see
/// `inject.rs`) or an unrecognized `Unknown` code.
#[must_use]
// See the matching allow on `vk_to_keycode` above: same flat mapping table,
// just inverted.
#[allow(clippy::too_many_lines, clippy::wildcard_imports)]
pub fn keycode_to_vk(code: KeyCode) -> Option<u16> {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;

    let vk = match code {
        KeyCode::A => VK_A,
        KeyCode::B => VK_B,
        KeyCode::C => VK_C,
        KeyCode::D => VK_D,
        KeyCode::E => VK_E,
        KeyCode::F => VK_F,
        KeyCode::G => VK_G,
        KeyCode::H => VK_H,
        KeyCode::I => VK_I,
        KeyCode::J => VK_J,
        KeyCode::K => VK_K,
        KeyCode::L => VK_L,
        KeyCode::M => VK_M,
        KeyCode::N => VK_N,
        KeyCode::O => VK_O,
        KeyCode::P => VK_P,
        KeyCode::Q => VK_Q,
        KeyCode::R => VK_R,
        KeyCode::S => VK_S,
        KeyCode::T => VK_T,
        KeyCode::U => VK_U,
        KeyCode::V => VK_V,
        KeyCode::W => VK_W,
        KeyCode::X => VK_X,
        KeyCode::Y => VK_Y,
        KeyCode::Z => VK_Z,

        KeyCode::Digit0 => VK_0,
        KeyCode::Digit1 => VK_1,
        KeyCode::Digit2 => VK_2,
        KeyCode::Digit3 => VK_3,
        KeyCode::Digit4 => VK_4,
        KeyCode::Digit5 => VK_5,
        KeyCode::Digit6 => VK_6,
        KeyCode::Digit7 => VK_7,
        KeyCode::Digit8 => VK_8,
        KeyCode::Digit9 => VK_9,

        KeyCode::F1 => VK_F1,
        KeyCode::F2 => VK_F2,
        KeyCode::F3 => VK_F3,
        KeyCode::F4 => VK_F4,
        KeyCode::F5 => VK_F5,
        KeyCode::F6 => VK_F6,
        KeyCode::F7 => VK_F7,
        KeyCode::F8 => VK_F8,
        KeyCode::F9 => VK_F9,
        KeyCode::F10 => VK_F10,
        KeyCode::F11 => VK_F11,
        KeyCode::F12 => VK_F12,

        KeyCode::Escape => VK_ESCAPE,
        KeyCode::Tab => VK_TAB,
        KeyCode::CapsLock => VK_CAPITAL,
        KeyCode::Space => VK_SPACE,
        KeyCode::Enter | KeyCode::NumpadEnter => VK_RETURN,
        KeyCode::Backspace => VK_BACK,
        KeyCode::Delete => VK_DELETE,
        KeyCode::Insert => VK_INSERT,
        KeyCode::Home => VK_HOME,
        KeyCode::End => VK_END,
        KeyCode::PageUp => VK_PRIOR,
        KeyCode::PageDown => VK_NEXT,

        KeyCode::ArrowUp => VK_UP,
        KeyCode::ArrowDown => VK_DOWN,
        KeyCode::ArrowLeft => VK_LEFT,
        KeyCode::ArrowRight => VK_RIGHT,

        KeyCode::LeftShift => VK_LSHIFT,
        KeyCode::RightShift => VK_RSHIFT,
        KeyCode::LeftCtrl => VK_LCONTROL,
        KeyCode::RightCtrl => VK_RCONTROL,
        KeyCode::LeftAlt => VK_LMENU,
        KeyCode::RightAlt => VK_RMENU,
        KeyCode::LeftMeta => VK_LWIN,
        KeyCode::RightMeta => VK_RWIN,

        KeyCode::PrintScreen => VK_SNAPSHOT,
        KeyCode::ScrollLock => VK_SCROLL,
        KeyCode::Pause => VK_PAUSE,
        KeyCode::ContextMenu => VK_APPS,

        KeyCode::Minus => VK_OEM_MINUS,
        KeyCode::Equal => VK_OEM_PLUS,
        KeyCode::LeftBracket => VK_OEM_4,
        KeyCode::RightBracket => VK_OEM_6,
        KeyCode::Backslash => VK_OEM_5,
        KeyCode::Semicolon => VK_OEM_1,
        KeyCode::Quote => VK_OEM_7,
        KeyCode::Comma => VK_OEM_COMMA,
        KeyCode::Period => VK_OEM_PERIOD,
        KeyCode::Slash => VK_OEM_2,
        KeyCode::Backquote => VK_OEM_3,

        KeyCode::NumLock => VK_NUMLOCK,
        KeyCode::Numpad0 => VK_NUMPAD0,
        KeyCode::Numpad1 => VK_NUMPAD1,
        KeyCode::Numpad2 => VK_NUMPAD2,
        KeyCode::Numpad3 => VK_NUMPAD3,
        KeyCode::Numpad4 => VK_NUMPAD4,
        KeyCode::Numpad5 => VK_NUMPAD5,
        KeyCode::Numpad6 => VK_NUMPAD6,
        KeyCode::Numpad7 => VK_NUMPAD7,
        KeyCode::Numpad8 => VK_NUMPAD8,
        KeyCode::Numpad9 => VK_NUMPAD9,
        KeyCode::NumpadAdd => VK_ADD,
        KeyCode::NumpadSubtract => VK_SUBTRACT,
        KeyCode::NumpadMultiply => VK_MULTIPLY,
        KeyCode::NumpadDivide => VK_DIVIDE,
        KeyCode::NumpadDecimal => VK_DECIMAL,

        KeyCode::Unknown(_) => return None,
    };
    Some(vk.0)
}

#[cfg(test)]
mod tests {
    use super::{keycode_to_vk, vk_to_keycode};
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
            let vk = keycode_to_vk(code).expect("mapped key must have a VK code");
            assert_eq!(vk_to_keycode(vk), code);
        }
    }

    #[test]
    fn unmapped_vk_becomes_unknown() {
        // VK 0x07 is reserved/unassigned in the Win32 VK table.
        assert_eq!(vk_to_keycode(0x07), KeyCode::Unknown(0x07));
    }
}
