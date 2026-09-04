//! macOS synthetic input injection via `CGEventPost`.

use seam_core::error::PlatformError;
use seam_core::protocol::{InputEvent, KeyCode, MouseButton};
use seam_core::traits::InputSink;

use super::cg_ffi::{
    CFRelease, CGEventCreateKeyboardEvent, CGEventCreateMouseEvent, CGEventCreateScrollWheelEvent,
    CGEventPost, CGEventRef, CGPoint, CGWarpMouseCursorPosition, K_CG_EVENT_LEFT_MOUSE_DOWN,
    K_CG_EVENT_LEFT_MOUSE_UP, K_CG_EVENT_OTHER_MOUSE_DOWN, K_CG_EVENT_OTHER_MOUSE_UP,
    K_CG_EVENT_RIGHT_MOUSE_DOWN, K_CG_EVENT_RIGHT_MOUSE_UP, K_CG_HID_EVENT_TAP,
    K_CG_MOUSE_BUTTON_CENTER, K_CG_MOUSE_BUTTON_LEFT, K_CG_MOUSE_BUTTON_RIGHT,
};
use super::keycodes::keycode_to_cgkeycode;

/// macOS implementation of [`seam_core::traits::InputSink`].
///
/// Injection tracks the last-known cursor position itself: `CGEventPost`
/// with a mouse-moved event needs an explicit target position, and
/// `InputSink::inject` for a mouse button doesn't carry one (buttons are
/// posted "at the current location" on every other platform's input
/// model) — so button events here are posted at whatever position the
/// last warp/move left the synthetic cursor at.
pub struct Sink {
    last_position: CGPoint,
}

impl Sink {
    /// Creates a sink with an assumed starting position of `(0, 0)`; the
    /// first `warp_cursor` or `MouseMoveAbs` corrects it.
    #[must_use]
    pub fn new() -> Self {
        Self {
            last_position: CGPoint { x: 0.0, y: 0.0 },
        }
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
            InputEvent::MouseDelta { dx, dy } => {
                let x = self.last_position.x + f64::from(dx);
                let y = self.last_position.y + f64::from(dy);
                self.warp_cursor_f64(x, y)
            }
            InputEvent::MouseDown { button } => self.post_mouse_button(button, true),
            InputEvent::MouseUp { button } => self.post_mouse_button(button, false),
            InputEvent::Scroll { dx, dy } => post_scroll(dx, dy),
            InputEvent::KeyDown { code, .. } => post_key(code, true),
            InputEvent::KeyUp { code } => post_key(code, false),
        }
    }

    fn warp_cursor(&mut self, x: i32, y: i32) -> Result<(), PlatformError> {
        self.warp_cursor_f64(f64::from(x), f64::from(y))
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
            post_key(code, false)?;
        }
        Ok(())
    }
}

impl Sink {
    fn warp_cursor_f64(&mut self, x: f64, y: f64) -> Result<(), PlatformError> {
        let point = CGPoint { x, y };
        // SAFETY: `CGWarpMouseCursorPosition` takes a plain value struct
        // with no preconditions.
        let err = unsafe { CGWarpMouseCursorPosition(point) };
        if err != 0 {
            return Err(PlatformError::Other(format!(
                "CGWarpMouseCursorPosition failed: CGError {err}"
            )));
        }
        self.last_position = point;
        Ok(())
    }

    fn post_mouse_button(&self, button: MouseButton, down: bool) -> Result<(), PlatformError> {
        let (event_type, cg_button) = match (button, down) {
            (MouseButton::Left, true) => (K_CG_EVENT_LEFT_MOUSE_DOWN, K_CG_MOUSE_BUTTON_LEFT),
            (MouseButton::Left, false) => (K_CG_EVENT_LEFT_MOUSE_UP, K_CG_MOUSE_BUTTON_LEFT),
            (MouseButton::Right, true) => (K_CG_EVENT_RIGHT_MOUSE_DOWN, K_CG_MOUSE_BUTTON_RIGHT),
            (MouseButton::Right, false) => (K_CG_EVENT_RIGHT_MOUSE_UP, K_CG_MOUSE_BUTTON_RIGHT),
            (MouseButton::Middle, true) => (K_CG_EVENT_OTHER_MOUSE_DOWN, K_CG_MOUSE_BUTTON_CENTER),
            (MouseButton::Middle, false) => (K_CG_EVENT_OTHER_MOUSE_UP, K_CG_MOUSE_BUTTON_CENTER),
            // X1/X2 have no dedicated CGMouseButton constant beyond
            // "center" (2); post them as button numbers 3/4, matching
            // what capture.rs reads back via kCGMouseEventButtonNumber.
            (MouseButton::X1, true) => (K_CG_EVENT_OTHER_MOUSE_DOWN, 3),
            (MouseButton::X1, false) => (K_CG_EVENT_OTHER_MOUSE_UP, 3),
            (MouseButton::X2, true) => (K_CG_EVENT_OTHER_MOUSE_DOWN, 4),
            (MouseButton::X2, false) => (K_CG_EVENT_OTHER_MOUSE_UP, 4),
        };
        // SAFETY: passing a null CGEventSourceRef is documented as valid —
        // the event is created with default source properties.
        let event: CGEventRef = unsafe {
            CGEventCreateMouseEvent(std::ptr::null(), event_type, self.last_position, cg_button)
        };
        post_and_release(event)
    }
}

fn post_scroll(dx: i32, dy: i32) -> Result<(), PlatformError> {
    // kCGScrollEventUnitPixel = 0, kCGScrollEventUnitLine = 1. We received
    // dx/dy as raw wheel-line deltas from capture.rs (or from the wire,
    // relayed from a peer's own line deltas), so line units here keeps a
    // straight passthrough rather than an unwanted unit conversion.
    const K_CG_SCROLL_EVENT_UNIT_LINE: u32 = 1;
    // SAFETY: a null CGEventSourceRef is valid (default source);
    // wheel_count: 2 matches the two trailing i32 varargs supplied.
    let event: CGEventRef = unsafe {
        CGEventCreateScrollWheelEvent(std::ptr::null(), K_CG_SCROLL_EVENT_UNIT_LINE, 2, dy, dx)
    };
    post_and_release(event)
}

fn post_key(code: KeyCode, down: bool) -> Result<(), PlatformError> {
    let Some(cg_code) = keycode_to_cgkeycode(code) else {
        // No Mac equivalent for this key (PrintScreen, ScrollLock, etc.)
        // — nothing to inject. Not an error; release_all_modifiers only
        // ever passes mapped keys.
        return Ok(());
    };
    // SAFETY: a null CGEventSourceRef is valid (default source).
    let event: CGEventRef = unsafe { CGEventCreateKeyboardEvent(std::ptr::null(), cg_code, down) };
    post_and_release(event)
}

fn post_and_release(event: CGEventRef) -> Result<(), PlatformError> {
    if event.is_null() {
        return Err(PlatformError::InjectionRejected);
    }
    // SAFETY: `event` was just confirmed non-null, freshly created, and
    // not yet posted or released.
    unsafe {
        CGEventPost(K_CG_HID_EVENT_TAP, event);
        CFRelease(event.cast());
    }
    Ok(())
}
