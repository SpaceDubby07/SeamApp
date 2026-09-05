//! macOS global input capture via `CGEventTap`.
//!
//! `CGEventTapCreate` requires a thread running a `CFRunLoop`, so this
//! module spawns a dedicated OS thread and calls `CFRunLoopRun` on it. The
//! tap callback is invoked ON THAT THREAD by the system.
//!
//! CRITICAL: the callback must return quickly. A slow callback causes
//! macOS to disable the tap and deliver a
//! `kCGEventTapDisabledByTimeout`/`kCGEventTapDisabledByUserInput`
//! pseudo-event instead of a real one — handled explicitly below by
//! re-enabling the tap, per Tier 5.5 of the build guide ("not optional").
//!
//! # Modifier keys
//! Unlike Windows, macOS delivers one `kCGEventFlagsChanged` event per
//! physical modifier key transition (Shift/Ctrl/Option/Command/CapsLock),
//! carrying that key's own `CGKeyCode` — so there's no left/right
//! ambiguity to resolve the way Windows' `WH_KEYBOARD_LL` needs. What it
//! doesn't give directly is press-vs-release: two physical keys sharing
//! one conceptual modifier (both Shift keys) share one bit in
//! `CGEventGetFlags`, so that bit alone can't disambiguate which one
//! changed when the other is already held. Instead we track, per key,
//! whether we currently believe it down — `flagsChanged` always fires in
//! down/up pairs per physical key in delivery order, so a simple toggle
//! against that set is correct without touching the flags bitmask at all.

use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc as std_mpsc;
use std::thread::JoinHandle;

use tokio::sync::mpsc::UnboundedSender;

use seam_core::error::PlatformError;
use seam_core::protocol::{InputEvent, KeyCode, MouseButton};
use seam_core::traits::InputCapture;

use super::cg_ffi::{
    CFMachPortCreateRunLoopSource, CFMachPortInvalidate, CFMachPortRef, CFRelease,
    CFRunLoopAddSource, CFRunLoopGetCurrent, CFRunLoopRef, CFRunLoopRun, CFRunLoopStop,
    CGDisplayBounds, CGDisplayHideCursor, CGDisplayShowCursor, CGEventGetIntegerValueField,
    CGEventGetLocation, CGEventRef, CGEventTapCreate, CGEventTapEnable, CGEventTapProxy,
    CGMainDisplayID, CGPoint, CGWarpMouseCursorPosition, K_CG_EVENT_FLAGS_CHANGED,
    K_CG_EVENT_KEY_DOWN, K_CG_EVENT_KEY_UP, K_CG_EVENT_LEFT_MOUSE_DOWN,
    K_CG_EVENT_LEFT_MOUSE_DRAGGED, K_CG_EVENT_LEFT_MOUSE_UP, K_CG_EVENT_MOUSE_MOVED,
    K_CG_EVENT_OTHER_MOUSE_DOWN, K_CG_EVENT_OTHER_MOUSE_DRAGGED, K_CG_EVENT_OTHER_MOUSE_UP,
    K_CG_EVENT_RIGHT_MOUSE_DOWN, K_CG_EVENT_RIGHT_MOUSE_DRAGGED, K_CG_EVENT_RIGHT_MOUSE_UP,
    K_CG_EVENT_SCROLL_WHEEL, K_CG_EVENT_TAP_DISABLED_BY_TIMEOUT,
    K_CG_EVENT_TAP_DISABLED_BY_USER_INPUT, K_CG_EVENT_TAP_OPTION_DEFAULT,
    K_CG_HEAD_INSERT_EVENT_TAP, K_CG_HID_EVENT_TAP, K_CG_KEYBOARD_EVENT_AUTOREPEAT,
    K_CG_KEYBOARD_EVENT_KEYCODE, K_CG_MOUSE_EVENT_BUTTON_NUMBER, K_CG_MOUSE_EVENT_DELTA_X,
    K_CG_MOUSE_EVENT_DELTA_Y, K_CG_SCROLL_WHEEL_EVENT_DELTA_AXIS_1,
    K_CG_SCROLL_WHEEL_EVENT_DELTA_AXIS_2, kCFRunLoopCommonModes,
};
use super::keycodes::cgkeycode_to_keycode;

/// Mirrors `seam-platform`'s Windows `Capture`: one process-wide
/// suppression flag, since there's only ever one active capture instance
/// (`current_platform()` constructs exactly one `Platform` bundle).
static SUPPRESS: AtomicBool = AtomicBool::new(false);

/// Where the tap callback warps the (hidden) cursor back to on every move
/// while suppressed — set to the main display's centre by
/// `set_suppression`. Pinning it there (rather than
/// `CGAssociateMouseAndMouseCursorPosition(false)`) keeps the window server
/// from drifting the cursor off-screen, which on a sustained one-direction
/// drag would starve `kCGMouseEventDeltaX/Y` and freeze the peer's cursor
/// — the "moves onto Windows fine but can't come back" bug. `CGWarpMouse…`
/// generates no event, so the warp never re-enters this tap.
static ANCHOR_X: AtomicI32 = AtomicI32::new(0);
static ANCHOR_Y: AtomicI32 = AtomicI32::new(0);

thread_local! {
    // The tap callback runs on the thread that created it (CGEventTap
    // delivers callbacks via that thread's run loop), so this only needs
    // to be visible there.
    static SINK: RefCell<Option<UnboundedSender<InputEvent>>> = const { RefCell::new(None) };
    static TAP_PORT: RefCell<CFMachPortRef> = const { RefCell::new(std::ptr::null_mut()) };
    // Which modifier KeyCodes we currently believe are held — see the
    // module docs on why flagsChanged needs toggle-tracking rather than a
    // flags-bitmask diff.
    static HELD_MODIFIERS: RefCell<HashSet<KeyCode>> = RefCell::new(HashSet::new());
}

/// A `CFRunLoopRef` obtained on the capture thread and sent to the caller
/// of `start` so `stop` can call `CFRunLoopStop` on it from another
/// thread. Apple documents `CFRunLoopStop` as safe to call across threads
/// — that's the whole mechanism this type exists to use.
struct SendableRunLoop(CFRunLoopRef);
// SAFETY: `CFRunLoopStop` is documented by Apple as callable from any
// thread to stop a run loop running on another thread; this wrapper only
// ever has that one operation performed on it after being sent.
unsafe impl Send for SendableRunLoop {}

/// macOS implementation of [`seam_core::traits::InputCapture`].
pub struct Capture {
    thread: Option<JoinHandle<()>>,
    run_loop: Option<SendableRunLoop>,
    /// Mirror of `SUPPRESS`, but owned by this handle so `set_suppression`
    /// only touches the (ref-counted) cursor hide/show and the pointer
    /// association on an actual change — never re-hiding or re-showing.
    suppressing: bool,
}

impl Capture {
    /// Creates an inactive capture. Call `start` to actually install the
    /// event tap.
    #[must_use]
    pub fn new() -> Self {
        Self {
            thread: None,
            run_loop: None,
            suppressing: false,
        }
    }
}

/// Warps the cursor to `(x, y)` in global display coordinates. Used to pin
/// the hidden cursor at `ANCHOR_*` while suppressed. `CGWarpMouseCursorPosition`
/// moves the cursor without posting an event, so this never re-enters the
/// tap as a spurious move.
fn warp_cursor_to(x: i32, y: i32) {
    // SAFETY: takes a plain value struct, no preconditions.
    unsafe {
        CGWarpMouseCursorPosition(CGPoint {
            x: f64::from(x),
            y: f64::from(y),
        });
    }
}

/// The centre of the main display, in global coordinates — the anchor the
/// suppressed cursor is pinned to.
fn main_display_centre() -> (i32, i32) {
    // SAFETY: plain C calls, no preconditions.
    let bounds = unsafe { CGDisplayBounds(CGMainDisplayID()) };
    #[allow(clippy::cast_possible_truncation)]
    (
        (bounds.origin.x + bounds.size.width / 2.0) as i32,
        (bounds.origin.y + bounds.size.height / 2.0) as i32,
    )
}

/// Hides or shows the hardware cursor on the main display. Hide/show are
/// ref-counted by the OS, so callers must issue exactly one show per hide
/// — `Capture::set_suppression` gates on `self.suppressing` to guarantee
/// that.
fn set_cursor_hidden(hidden: bool) {
    // SAFETY: plain C calls taking a display id, no preconditions.
    unsafe {
        if hidden {
            CGDisplayHideCursor(CGMainDisplayID());
        } else {
            CGDisplayShowCursor(CGMainDisplayID());
        }
    }
}

impl Default for Capture {
    fn default() -> Self {
        Self::new()
    }
}

impl InputCapture for Capture {
    fn start(&mut self, sink: UnboundedSender<InputEvent>) -> Result<(), PlatformError> {
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<usize, String>>();

        let handle = std::thread::Builder::new()
            .name("seam-input-capture".into())
            .spawn(move || {
                SINK.with(|cell| *cell.borrow_mut() = Some(sink));

                let mask: u64 = (1u64 << K_CG_EVENT_LEFT_MOUSE_DOWN)
                    | (1u64 << K_CG_EVENT_LEFT_MOUSE_UP)
                    | (1u64 << K_CG_EVENT_RIGHT_MOUSE_DOWN)
                    | (1u64 << K_CG_EVENT_RIGHT_MOUSE_UP)
                    | (1u64 << K_CG_EVENT_MOUSE_MOVED)
                    | (1u64 << K_CG_EVENT_LEFT_MOUSE_DRAGGED)
                    | (1u64 << K_CG_EVENT_RIGHT_MOUSE_DRAGGED)
                    | (1u64 << K_CG_EVENT_OTHER_MOUSE_DOWN)
                    | (1u64 << K_CG_EVENT_OTHER_MOUSE_UP)
                    | (1u64 << K_CG_EVENT_OTHER_MOUSE_DRAGGED)
                    | (1u64 << K_CG_EVENT_KEY_DOWN)
                    | (1u64 << K_CG_EVENT_KEY_UP)
                    | (1u64 << K_CG_EVENT_FLAGS_CHANGED)
                    | (1u64 << K_CG_EVENT_SCROLL_WHEEL);

                // SAFETY: `tap_callback` matches the `CGEventTapCallBack`
                // signature. `user_info` is unused (state instead lives in
                // thread-locals, since this callback also has to satisfy a
                // plain C function pointer, not a capturing closure).
                let tap = unsafe {
                    CGEventTapCreate(
                        K_CG_HID_EVENT_TAP,
                        K_CG_HEAD_INSERT_EVENT_TAP,
                        K_CG_EVENT_TAP_OPTION_DEFAULT,
                        mask,
                        tap_callback,
                        std::ptr::null_mut(),
                    )
                };
                if tap.is_null() {
                    let _ = ready_tx.send(Err(
                        "CGEventTapCreate returned null — missing Accessibility (and/or Input \
                         Monitoring) permission for this app"
                            .to_string(),
                    ));
                    SINK.with(|cell| *cell.borrow_mut() = None);
                    return;
                }
                TAP_PORT.with(|cell| *cell.borrow_mut() = tap);

                // SAFETY: `tap` was just confirmed non-null above and is a
                // valid CFMachPortRef; `order: 0` is the standard value
                // for a run loop source with no ordering requirement.
                let source = unsafe { CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0) };
                // SAFETY: `CFRunLoopGetCurrent` has no preconditions.
                let run_loop = unsafe { CFRunLoopGetCurrent() };
                // SAFETY: `run_loop` and `source` are both valid; adding a
                // source to the run loop that will run on this same
                // thread is the documented setup for CGEventTap.
                unsafe { CFRunLoopAddSource(run_loop, source, kCFRunLoopCommonModes) };
                // SAFETY: `tap` is a freshly created, not-yet-enabled tap.
                unsafe { CGEventTapEnable(tap, true) };

                let _ = ready_tx.send(Ok(run_loop as usize));

                // SAFETY: no preconditions; this blocks until
                // `CFRunLoopStop` is called on `run_loop` from `stop()`.
                unsafe { CFRunLoopRun() };

                // SAFETY: `tap` is still the same valid, non-null port
                // created above and not yet invalidated.
                unsafe {
                    CFMachPortInvalidate(tap);
                    CFRelease(tap.cast());
                }
                SINK.with(|cell| *cell.borrow_mut() = None);
                TAP_PORT.with(|cell| *cell.borrow_mut() = std::ptr::null_mut());
                HELD_MODIFIERS.with(|cell| cell.borrow_mut().clear());
            })
            .map_err(|e| PlatformError::HookRegistrationFailed(e.to_string()))?;

        match ready_rx.recv() {
            Ok(Ok(run_loop)) => {
                self.thread = Some(handle);
                self.run_loop = Some(SendableRunLoop(run_loop as CFRunLoopRef));
                Ok(())
            }
            Ok(Err(reason)) => {
                let _ = handle.join();
                Err(PlatformError::PermissionDenied(reason))
            }
            Err(_) => {
                let _ = handle.join();
                Err(PlatformError::HookRegistrationFailed(
                    "capture thread exited before signaling readiness".to_string(),
                ))
            }
        }
    }

    fn stop(&mut self) -> Result<(), PlatformError> {
        // Never tear down leaving the cursor hidden or events suppressed —
        // this runs on every session end, including an `abort()`
        // mid-handoff (via `Session`'s `Drop`).
        let _ = self.set_suppression(false);
        if let Some(SendableRunLoop(run_loop)) = self.run_loop.take() {
            // SAFETY: `run_loop` came from `CFRunLoopGetCurrent()` on the
            // still-running capture thread; `CFRunLoopStop` is documented
            // safe to call cross-thread.
            unsafe { CFRunLoopStop(run_loop) };
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
        Ok(())
    }

    fn set_suppression(&mut self, suppress: bool) -> Result<(), PlatformError> {
        SUPPRESS.store(suppress, Ordering::SeqCst);
        if suppress != self.suppressing {
            if suppress {
                // Pin the (now hidden) cursor at the display centre; the
                // tap callback warps it back here on every move so it
                // can't drift off-screen and stall the delta stream, and
                // so it can't trip hot corners while the peer is driven.
                let (cx, cy) = main_display_centre();
                ANCHOR_X.store(cx, Ordering::SeqCst);
                ANCHOR_Y.store(cy, Ordering::SeqCst);
                warp_cursor_to(cx, cy);
            }
            set_cursor_hidden(suppress);
            self.suppressing = suppress;
        }
        Ok(())
    }

    fn is_healthy(&self) -> bool {
        self.thread.as_ref().is_some_and(|h| !h.is_finished())
    }
}

impl Drop for Capture {
    /// Backstop for `Session`'s own `Drop`: if a `Capture` is ever
    /// dropped without `stop()` having been called, the event tap thread
    /// must still be torn down and the pointer restored — a leaked,
    /// suppressing `CGEventTap` swallows all input system-wide until the
    /// process exits.
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// A single raw HID mouse-move delta report is always tiny (well within
/// `i32` range) — this only guards against a theoretical extreme value
/// rather than anything expected in practice.
fn clamp_to_i32(v: i64) -> i32 {
    i32::try_from(v.clamp(i64::from(i32::MIN), i64::from(i32::MAX))).unwrap_or(0)
}

fn forward(event: InputEvent) {
    SINK.with(|cell| {
        if let Some(sink) = cell.borrow().as_ref() {
            let _ = sink.send(event);
        }
    });
}

/// Maps `CGEventGetIntegerValueField(event, kCGMouseEventButtonNumber)` for
/// an `OtherMouse*` event (button number `>= 2`) to our `MouseButton`.
/// `2` is conventionally the middle button; `3`/`4` the side buttons.
fn other_mouse_button(button_number: i64) -> MouseButton {
    match button_number {
        3 => MouseButton::X1,
        4 => MouseButton::X2,
        _ => MouseButton::Middle,
    }
}

/// Resolves one `kCGEventFlagsChanged` event into the `InputEvent` for
/// whichever modifier key changed, using toggle-tracking — see the module
/// docs for why.
fn resolve_flags_changed(event: CGEventRef) -> Option<InputEvent> {
    // SAFETY: `event` is the live event handed to us by the tap callback
    // for the duration of this call.
    let raw_code = unsafe { CGEventGetIntegerValueField(event, K_CG_KEYBOARD_EVENT_KEYCODE) };
    let code = cgkeycode_to_keycode(u16::try_from(raw_code).unwrap_or(0));
    let is_modifier = matches!(
        code,
        KeyCode::LeftShift
            | KeyCode::RightShift
            | KeyCode::LeftCtrl
            | KeyCode::RightCtrl
            | KeyCode::LeftAlt
            | KeyCode::RightAlt
            | KeyCode::LeftMeta
            | KeyCode::RightMeta
            | KeyCode::CapsLock
    );
    if !is_modifier {
        return None;
    }

    let now_down = HELD_MODIFIERS.with(|cell| {
        let mut held = cell.borrow_mut();
        if held.remove(&code) {
            false
        } else {
            held.insert(code);
            true
        }
    });

    Some(if now_down {
        InputEvent::KeyDown {
            code,
            repeat: false,
        }
    } else {
        InputEvent::KeyUp { code }
    })
}

/// # Safety
/// Called by the OS per the `CGEventTapCallBack` contract: `event` is a
/// valid `CGEventRef` for the duration of this call, and returning it
/// unchanged (rather than null) lets it continue through the system.
unsafe extern "C" fn tap_callback(
    _proxy: CGEventTapProxy,
    event_type: u32,
    event: CGEventRef,
    _user_info: *mut std::ffi::c_void,
) -> CGEventRef {
    if event_type == K_CG_EVENT_TAP_DISABLED_BY_TIMEOUT
        || event_type == K_CG_EVENT_TAP_DISABLED_BY_USER_INPUT
    {
        TAP_PORT.with(|cell| {
            let tap = *cell.borrow();
            if !tap.is_null() {
                // SAFETY: `tap` is the same live port this thread created
                // and hasn't invalidated.
                unsafe { CGEventTapEnable(tap, true) };
            }
        });
        return event;
    }

    let parsed = match event_type {
        K_CG_EVENT_MOUSE_MOVED
        | K_CG_EVENT_LEFT_MOUSE_DRAGGED
        | K_CG_EVENT_RIGHT_MOUSE_DRAGGED
        | K_CG_EVENT_OTHER_MOUSE_DRAGGED => {
            // SAFETY: `event` is valid for the duration of this callback.
            let CGPoint { x, y } = unsafe { CGEventGetLocation(event) };

            // Tier 7.2: the raw per-event delta, straight from the HID
            // report — independent of cursor position, so it keeps
            // flowing in every direction while `RemoteActive` pins our own
            // (hidden) cursor at `ANCHOR_*`. This is what carries the
            // peer's motion over the wire while driving.
            // SAFETY: same as above.
            let dx = unsafe { CGEventGetIntegerValueField(event, K_CG_MOUSE_EVENT_DELTA_X) };
            // SAFETY: same as above.
            let dy = unsafe { CGEventGetIntegerValueField(event, K_CG_MOUSE_EVENT_DELTA_Y) };
            if dx != 0 || dy != 0 {
                forward(InputEvent::MouseDelta {
                    dx: clamp_to_i32(dx),
                    dy: clamp_to_i32(dy),
                });
            }

            // Keep the suppressed cursor from actually moving: warp it
            // straight back to the anchor. Done after reading the delta
            // (which is unaffected), and `CGWarpMouseCursorPosition` posts
            // no event, so this doesn't feed back into the tap.
            if SUPPRESS.load(Ordering::SeqCst) {
                warp_cursor_to(
                    ANCHOR_X.load(Ordering::SeqCst),
                    ANCHOR_Y.load(Ordering::SeqCst),
                );
            }

            #[allow(clippy::cast_possible_truncation)]
            Some(InputEvent::MouseMoveAbs {
                x: x as i32,
                y: y as i32,
            })
        }
        K_CG_EVENT_LEFT_MOUSE_DOWN => Some(InputEvent::MouseDown {
            button: MouseButton::Left,
        }),
        K_CG_EVENT_LEFT_MOUSE_UP => Some(InputEvent::MouseUp {
            button: MouseButton::Left,
        }),
        K_CG_EVENT_RIGHT_MOUSE_DOWN => Some(InputEvent::MouseDown {
            button: MouseButton::Right,
        }),
        K_CG_EVENT_RIGHT_MOUSE_UP => Some(InputEvent::MouseUp {
            button: MouseButton::Right,
        }),
        K_CG_EVENT_OTHER_MOUSE_DOWN => {
            // SAFETY: `event` is valid for the duration of this callback.
            let button =
                unsafe { CGEventGetIntegerValueField(event, K_CG_MOUSE_EVENT_BUTTON_NUMBER) };
            Some(InputEvent::MouseDown {
                button: other_mouse_button(button),
            })
        }
        K_CG_EVENT_OTHER_MOUSE_UP => {
            // SAFETY: `event` is valid for the duration of this callback.
            let button =
                unsafe { CGEventGetIntegerValueField(event, K_CG_MOUSE_EVENT_BUTTON_NUMBER) };
            Some(InputEvent::MouseUp {
                button: other_mouse_button(button),
            })
        }
        K_CG_EVENT_SCROLL_WHEEL => {
            // SAFETY: `event` is valid for the duration of this callback.
            let dy =
                unsafe { CGEventGetIntegerValueField(event, K_CG_SCROLL_WHEEL_EVENT_DELTA_AXIS_1) };
            // SAFETY: same as above.
            let dx =
                unsafe { CGEventGetIntegerValueField(event, K_CG_SCROLL_WHEEL_EVENT_DELTA_AXIS_2) };
            Some(InputEvent::Scroll {
                dx: i32::try_from(dx).unwrap_or(0),
                dy: i32::try_from(dy).unwrap_or(0),
            })
        }
        K_CG_EVENT_KEY_DOWN => {
            // SAFETY: `event` is valid for the duration of this callback.
            let raw_code =
                unsafe { CGEventGetIntegerValueField(event, K_CG_KEYBOARD_EVENT_KEYCODE) };
            // SAFETY: same as above.
            let repeat =
                unsafe { CGEventGetIntegerValueField(event, K_CG_KEYBOARD_EVENT_AUTOREPEAT) } != 0;
            Some(InputEvent::KeyDown {
                code: cgkeycode_to_keycode(u16::try_from(raw_code).unwrap_or(0)),
                repeat,
            })
        }
        K_CG_EVENT_KEY_UP => {
            // SAFETY: `event` is valid for the duration of this callback.
            let raw_code =
                unsafe { CGEventGetIntegerValueField(event, K_CG_KEYBOARD_EVENT_KEYCODE) };
            Some(InputEvent::KeyUp {
                code: cgkeycode_to_keycode(u16::try_from(raw_code).unwrap_or(0)),
            })
        }
        K_CG_EVENT_FLAGS_CHANGED => resolve_flags_changed(event),
        _ => None,
    };

    if let Some(parsed) = parsed {
        forward(parsed);
    }

    if SUPPRESS.load(Ordering::SeqCst) {
        // Returning null swallows the event — it never reaches any other
        // app. This is what makes the local cursor "disappear" during a
        // remote handoff.
        std::ptr::null_mut()
    } else {
        event
    }
}
