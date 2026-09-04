//! Windows synthetic input injection via `SendInput`.

use std::mem::size_of;

use seam_core::error::PlatformError;
use seam_core::protocol::{InputEvent, KeyCode, MouseButton};
use seam_core::traits::InputSink;

use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, MOUSE_EVENT_FLAGS, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL,
    MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, SendInput, SetCursorPos, VIRTUAL_KEY, XBUTTON1,
    XBUTTON2,
};

use super::keycodes::keycode_to_vk;

/// Windows implementation of [`seam_core::traits::InputSink`].
pub struct Sink;

impl Sink {
    /// Creates a sink. Injection is stateless on Windows — there's nothing
    /// to set up ahead of time.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for Sink {
    fn default() -> Self {
        Self::new()
    }
}

impl InputSink for Sink {
    fn inject(&mut self, event: &InputEvent) -> Result<(), PlatformError> {
        match *event {
            InputEvent::MouseMoveAbs { x, y } => self.warp_cursor(x, y),
            InputEvent::MouseDelta { dx, dy } => send_one(mouse_input(dx, dy, 0, MOUSEEVENTF_MOVE)),
            InputEvent::MouseDown { button } => send_one(mouse_button_input(button, true)),
            InputEvent::MouseUp { button } => send_one(mouse_button_input(button, false)),
            InputEvent::Scroll { dx, dy } => {
                if dy != 0 {
                    send_one(mouse_input(0, 0, wheel_units(dy), MOUSEEVENTF_WHEEL))?;
                }
                if dx != 0 {
                    send_one(mouse_input(0, 0, wheel_units(dx), MOUSEEVENTF_HWHEEL))?;
                }
                Ok(())
            }
            InputEvent::KeyDown { code, .. } => send_key(code, false),
            InputEvent::KeyUp { code } => send_key(code, true),
        }
    }

    fn warp_cursor(&mut self, x: i32, y: i32) -> Result<(), PlatformError> {
        // SAFETY: `SetCursorPos` takes plain integer coordinates; the only
        // precondition is desktop access, which our interactive GUI process
        // has.
        unsafe { SetCursorPos(x, y) }.map_err(|e| PlatformError::Other(e.to_string()))
    }

    fn release_all_modifiers(&mut self) -> Result<(), PlatformError> {
        for code in [
            KeyCode::LeftShift,
            KeyCode::RightShift,
            KeyCode::LeftCtrl,
            KeyCode::RightCtrl,
            KeyCode::LeftAlt,
            KeyCode::RightAlt,
            KeyCode::LeftMeta,
            KeyCode::RightMeta,
        ] {
            send_key(code, true)?;
        }
        Ok(())
    }
}

fn send_one(input: INPUT) -> Result<(), PlatformError> {
    // SAFETY: `input` is a single, fully-initialized `INPUT` whose union
    // tag (`r#type`) matches the populated union field, which is the
    // contract `SendInput` requires.
    let sent = unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
    if sent == 0 {
        return Err(PlatformError::InjectionRejected);
    }
    Ok(())
}

fn mouse_input(dx: i32, dy: i32, mouse_data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: mouse_data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn mouse_button_input(button: MouseButton, down: bool) -> INPUT {
    let (flags, mouse_data) = match (button, down) {
        (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
        (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
        (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
        (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
        (MouseButton::X1, true) => (MOUSEEVENTF_XDOWN, i32::from(XBUTTON1.0)),
        (MouseButton::X1, false) => (MOUSEEVENTF_XUP, i32::from(XBUTTON1.0)),
        (MouseButton::X2, true) => (MOUSEEVENTF_XDOWN, i32::from(XBUTTON2.0)),
        (MouseButton::X2, false) => (MOUSEEVENTF_XUP, i32::from(XBUTTON2.0)),
    };
    mouse_input(0, 0, mouse_data, flags)
}

/// `MOUSEEVENTF_WHEEL`/`_HWHEEL` expect a delta in units of `WHEEL_DELTA`
/// (120), matching what a physical wheel click reports.
fn wheel_units(delta: i32) -> i32 {
    delta * 120
}

/// Keys whose `SendInput` injection must set `KEYEVENTF_EXTENDEDKEY` — the
/// injection-side mirror of the capture-side `LLKHF_EXTENDED` check in
/// `capture.rs`. Getting this wrong doesn't break the key itself, just
/// which physical key Windows *thinks* it was (matters for e.g. right Ctrl
/// vs left Ctrl remap rules).
fn is_extended(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::RightCtrl
            | KeyCode::RightAlt
            | KeyCode::NumpadEnter
            | KeyCode::Insert
            | KeyCode::Delete
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::ArrowUp
            | KeyCode::ArrowDown
            | KeyCode::ArrowLeft
            | KeyCode::ArrowRight
            | KeyCode::NumLock
    )
}

fn send_key(code: KeyCode, up: bool) -> Result<(), PlatformError> {
    let Some(vk) = keycode_to_vk(code) else {
        // Nothing to inject for a key we don't have a VK mapping for. Not
        // an error: `release_all_modifiers` iterates a fixed, always-mapped
        // list, and ordinary capture never produces raw `Unknown` codes for
        // modifier keys.
        return Ok(());
    };

    let mut bits = if up { KEYEVENTF_KEYUP.0 } else { 0 };
    if is_extended(code) {
        bits |= KEYEVENTF_EXTENDEDKEY.0;
    }

    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: 0,
                dwFlags: KEYBD_EVENT_FLAGS(bits),
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    send_one(input)
}
