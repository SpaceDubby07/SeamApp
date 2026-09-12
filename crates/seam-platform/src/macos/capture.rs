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
    CFAbsoluteTimeGetCurrent, CFMachPortCreateRunLoopSource, CFMachPortInvalidate, CFMachPortRef,
    CFRelease, CFRunLoopAddSource, CFRunLoopAddTimer, CFRunLoopGetCurrent, CFRunLoopRef,
    CFRunLoopRun, CFRunLoopStop, CFRunLoopTimerCreate, CFRunLoopTimerInvalidate, CFRunLoopTimerRef,
    CGDisplayBounds, CGDisplayHideCursor, CGDisplayShowCursor, CGEventGetIntegerValueField,
    CGEventGetLocation, CGEventRef, CGEventTapCreate, CGEventTapEnable, CGEventTapIsEnabled,
    CGEventTapProxy, CGMainDisplayID, CGPoint, CGSetLocalEventsSuppressionInterval,
    CGWarpMouseCursorPosition, K_CG_EVENT_FLAGS_CHANGED, K_CG_EVENT_KEY_DOWN, K_CG_EVENT_KEY_UP,
    K_CG_EVENT_LEFT_MOUSE_DOWN, K_CG_EVENT_LEFT_MOUSE_DRAGGED, K_CG_EVENT_LEFT_MOUSE_UP,
    K_CG_EVENT_MOUSE_MOVED, K_CG_EVENT_OTHER_MOUSE_DOWN, K_CG_EVENT_OTHER_MOUSE_DRAGGED,
    K_CG_EVENT_OTHER_MOUSE_UP, K_CG_EVENT_RIGHT_MOUSE_DOWN, K_CG_EVENT_RIGHT_MOUSE_DRAGGED,
    K_CG_EVENT_RIGHT_MOUSE_UP, K_CG_EVENT_SCROLL_WHEEL, K_CG_EVENT_TAP_DISABLED_BY_TIMEOUT,
    K_CG_EVENT_TAP_DISABLED_BY_USER_INPUT, K_CG_EVENT_TAP_OPTION_DEFAULT,
    K_CG_HEAD_INSERT_EVENT_TAP, K_CG_HID_EVENT_TAP, K_CG_KEYBOARD_EVENT_AUTOREPEAT,
    K_CG_KEYBOARD_EVENT_KEYCODE, K_CG_MOUSE_EVENT_BUTTON_NUMBER,
    K_CG_SCROLL_WHEEL_EVENT_DELTA_AXIS_1, K_CG_SCROLL_WHEEL_EVENT_DELTA_AXIS_2,
    kCFRunLoopCommonModes,
};
use super::keycodes::cgkeycode_to_keycode;

/// Mirrors `seam-platform`'s Windows `Capture`: one process-wide
/// suppression flag, since there's only ever one active capture instance
/// (`current_platform()` constructs exactly one `Platform` bundle).
static SUPPRESS: AtomicBool = AtomicBool::new(false);

/// Where the tap callback warps the hidden cursor back to on every move
/// while suppressed — the main display's centre, set by `set_suppression`.
/// `CGWarpMouseCursorPosition` generates no event, so the warp never
/// re-enters this tap; `CGEventGetLocation` stays pinned here between real
/// moves.
static ANCHOR_X: AtomicI32 = AtomicI32::new(0);
static ANCHOR_Y: AtomicI32 = AtomicI32::new(0);

/// How close a raw motion delta is allowed to get to the distance between
/// the anchor and the primary display's edge before it's dropped as
/// possibly clamped by the OS before the tap saw it — Barrier's
/// `bogusZoneSize` (`OSXScreen::onMouseMove`), same technique as this
/// crate's Windows `capture.rs`.
const BOGUS_ZONE_PX: f64 = 10.0;

/// How often the watchdog timer checks the tap is still enabled. macOS
/// disables the tap across sleep/wake, a screen lock, and fast user
/// switching (delivering `kCGEventTapDisabledByUserInput` — but only if
/// its run loop is being serviced, which it may not be right at wake); a
/// 1s poll re-enables it regardless of whether the pseudo-event arrived.
const WATCHDOG_INTERVAL_SECS: f64 = 1.0;

thread_local! {
    // The tap callback runs on the thread that created it (CGEventTap
    // delivers callbacks via that thread's run loop), so this only needs
    // to be visible there.
    static SINK: RefCell<Option<UnboundedSender<InputEvent>>> = const { RefCell::new(None) };
    static TAP_PORT: RefCell<CFMachPortRef> = const { RefCell::new(std::ptr::null_mut()) };
    static WATCHDOG_TIMER: RefCell<CFRunLoopTimerRef> = const { RefCell::new(std::ptr::null_mut()) };
    // Which modifier KeyCodes we currently believe are held — see the
    // module docs on why flagsChanged needs toggle-tracking rather than a
    // flags-bitmask diff.
    static HELD_MODIFIERS: RefCell<HashSet<KeyCode>> = RefCell::new(HashSet::new());

    // Last cursor position we saw, in global display coordinates — updated
    // on every move regardless of suppression state. Motion deltas are
    // computed by diffing against this (Barrier's `m_xCursor`/`m_yCursor` in
    // `OSXScreen::onMouseMove`), not read from the HID report's own delta
    // fields, so the same position-diff arithmetic applies whether the
    // cursor is free-running or being warped back to an anchor every move.
    static LAST_CURSOR: RefCell<Option<(f64, f64)>> = const { RefCell::new(None) };

    // Sub-pixel remainder left over after truncating each move's delta to
    // an integer, one accumulator per axis (Barrier's `m_xFractionalMove`/
    // `m_yFractionalMove`) — CGFloat positions carry sub-pixel precision
    // that a raw per-move `as i32` truncation would otherwise lose over many
    // small moves.
    static FRAC_X: RefCell<f64> = const { RefCell::new(0.0) };
    static FRAC_Y: RefCell<f64> = const { RefCell::new(0.0) };
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

/// Sets (or clears) the local-event suppression window
/// `CGWarpMouseCursorPosition` normally opens after every warp — the OS's
/// anti-feedback-loop guard, which would otherwise delay/drop the very HID
/// deltas we're relying on to keep flowing while warping back to the anchor
/// on every move. Barrier's `setZeroSuppressionInterval`/
/// `avoidHesitatingCursor` (`OSXScreen.mm`), called from the same
/// suppress-on/suppress-off transitions this crate's `set_suppression` uses.
fn set_local_events_suppression_interval(seconds: f64) {
    // SAFETY: plain C call taking an f64, no preconditions. Process-wide;
    // `set_suppression(false)` / `stop` / `Drop` always restore it to 0.
    unsafe {
        CGSetLocalEventsSuppressionInterval(seconds);
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

                // Watchdog: re-enables the tap after macOS disables it
                // across sleep/wake, a lock, or fast user switching
                // (Tier 12's sleep/wake recovery). Repeats forever; torn
                // down after the run loop stops, below.
                // SAFETY: `tap_watchdog` matches `CFRunLoopTimerCallBack`;
                // a null context is allowed; `flags`/`order` of 0 are the
                // documented defaults.
                let watchdog = unsafe {
                    CFRunLoopTimerCreate(
                        std::ptr::null(),
                        CFAbsoluteTimeGetCurrent() + WATCHDOG_INTERVAL_SECS,
                        WATCHDOG_INTERVAL_SECS,
                        0,
                        0,
                        tap_watchdog,
                        std::ptr::null_mut(),
                    )
                };
                WATCHDOG_TIMER.with(|cell| *cell.borrow_mut() = watchdog);
                // SAFETY: `run_loop` and `watchdog` are both valid; adding
                // a timer to this thread's own run loop is the documented
                // pattern.
                unsafe { CFRunLoopAddTimer(run_loop, watchdog, kCFRunLoopCommonModes) };

                let _ = ready_tx.send(Ok(run_loop as usize));

                // SAFETY: no preconditions; this blocks until
                // `CFRunLoopStop` is called on `run_loop` from `stop()`.
                unsafe { CFRunLoopRun() };

                // SAFETY: `watchdog` and `tap` are the same valid ports
                // created above and not yet invalidated.
                unsafe {
                    CFRunLoopTimerInvalidate(watchdog);
                    CFRelease(watchdog.cast());
                    CFMachPortInvalidate(tap);
                    CFRelease(tap.cast());
                }
                SINK.with(|cell| *cell.borrow_mut() = None);
                TAP_PORT.with(|cell| *cell.borrow_mut() = std::ptr::null_mut());
                WATCHDOG_TIMER.with(|cell| *cell.borrow_mut() = std::ptr::null_mut());
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
                // Barrier's `avoidHesitatingCursor`: a small non-zero
                // interval, not exactly zero — a bare 0.0 here reintroduces
                // a hesitating cursor on the transition per Barrier's own
                // history (see `OSXScreen::leave`).
                set_local_events_suppression_interval(0.0001);
                let (cx, cy) = main_display_centre();
                ANCHOR_X.store(cx, Ordering::SeqCst);
                ANCHOR_Y.store(cy, Ordering::SeqCst);
                warp_cursor_to(cx, cy);
                set_cursor_hidden(true);
            } else {
                set_cursor_hidden(false);
                // Barrier's `setZeroSuppressionInterval`, called on
                // `OSXScreen::enter` when regaining local control.
                set_local_events_suppression_interval(0.0);
            }
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

/// Handles a mouse-moved / -dragged event, mirroring
/// `OSXScreen::onMouseMove`: the delta is computed by diffing the current
/// position against the last one we saw (`LAST_CURSOR`), not read from the
/// HID report's own delta fields — this is Barrier's technique on both
/// platforms, and keeps the arithmetic identical whether the cursor is
/// free-running or being warped back to an anchor every move.
///
/// Emits exactly one event per move, matching Barrier's `isOnScreen`
/// branch: an absolute position while local, or (after warping back to
/// `ANCHOR_*` and passing the bogus-zone check) an accumulated delta while
/// suppressed — never both.
#[allow(clippy::cast_possible_truncation)]
fn handle_mouse_moved(event: CGEventRef) -> Option<InputEvent> {
    // SAFETY: `event` is valid for the duration of the tap callback that
    // called us.
    let CGPoint { x: mx, y: my } = unsafe { CGEventGetLocation(event) };

    let previous = LAST_CURSOR.with(|cell| cell.borrow_mut().replace((mx, my)));
    let Some((prev_x, prev_y)) = previous else {
        // First move since the tap started (or since it was torn down and
        // restarted) — no baseline to diff against yet.
        return None;
    };
    let x = mx - prev_x;
    let y = my - prev_y;
    if x == 0.0 && y == 0.0 {
        return None;
    }

    if !SUPPRESS.load(Ordering::SeqCst) {
        return Some(InputEvent::MouseMoveAbs {
            x: mx as i32,
            y: my as i32,
        });
    }

    // Motion on the secondary (peer) screen: warp the hidden cursor back to
    // the anchor so it never approaches an edge, then examine the motion
    // that led up to this warp.
    warp_cursor_to(
        ANCHOR_X.load(Ordering::SeqCst),
        ANCHOR_Y.load(Ordering::SeqCst),
    );

    // SAFETY: plain C call, no preconditions.
    let bounds = unsafe { CGDisplayBounds(CGMainDisplayID()) };
    let (cx, cy) = (
        f64::from(ANCHOR_X.load(Ordering::SeqCst)),
        f64::from(ANCHOR_Y.load(Ordering::SeqCst)),
    );
    // If the motion is about the distance from the anchor to a screen edge,
    // the OS may have clamped the real motion at that edge before the tap
    // ever saw it — Barrier's `bogusZoneSize` check.
    let bogus = -x + BOGUS_ZONE_PX > cx - bounds.origin.x
        || x + BOGUS_ZONE_PX > bounds.origin.x + bounds.size.width - cx
        || -y + BOGUS_ZONE_PX > cy - bounds.origin.y
        || y + BOGUS_ZONE_PX > bounds.origin.y + bounds.size.height - cy;
    if bogus {
        tracing::debug!(x, y, "dropped bogus motion");
        return None;
    }

    // Accumulate the sub-pixel remainder so repeated truncation across many
    // small moves doesn't lose motion.
    let (int_x, int_y) = (
        FRAC_X.with(|cell| {
            let mut frac = cell.borrow_mut();
            *frac += x;
            let whole = frac.trunc();
            *frac -= whole;
            whole as i32
        }),
        FRAC_Y.with(|cell| {
            let mut frac = cell.borrow_mut();
            *frac += y;
            let whole = frac.trunc();
            *frac -= whole;
            whole as i32
        }),
    );
    if int_x == 0 && int_y == 0 {
        return None;
    }
    Some(InputEvent::MouseDelta {
        dx: int_x,
        dy: int_y,
    })
}

/// Watchdog run-loop timer callback (fires on the capture thread every
/// [`WATCHDOG_INTERVAL_SECS`]). If macOS has disabled the tap — sleep/
/// wake, screen lock, fast user switch — re-enable it. `CGEventTapEnable`
/// on an already-enabled tap is a no-op, so the common case costs one
/// `CGEventTapIsEnabled` call.
///
/// # Safety
/// Matches the `CFRunLoopTimerCallBack` ABI; both args are unused.
unsafe extern "C" fn tap_watchdog(_timer: CFRunLoopTimerRef, _info: *mut std::ffi::c_void) {
    TAP_PORT.with(|cell| {
        let tap = *cell.borrow();
        if tap.is_null() {
            return;
        }
        // SAFETY: `tap` is the live port this same thread created and has
        // not invalidated (that only happens after the run loop stops,
        // which also invalidates this timer first).
        if !unsafe { CGEventTapIsEnabled(tap) } {
            tracing::warn!("macOS disabled the event tap (sleep/wake, lock, or load); re-enabling");
            // SAFETY: as above.
            unsafe { CGEventTapEnable(tap, true) };
        }
    });
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
        | K_CG_EVENT_OTHER_MOUSE_DRAGGED => handle_mouse_moved(event),
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

    // Mouse-moved/dragged events are NEVER swallowed, suppressed or not —
    // Barrier's exact behaviour (`OSXScreen::handleCGInputEvent`,
    // `OSXScreen.mm:1935-1943`) and documented reason: the OS silently
    // ignores subsequent `CGWarpMouseCursorPosition` calls from this same
    // tap if a mouse-moved event was just swallowed (returned null) instead
    // of passed through. Swallowing this event type would break the very
    // anchor warp `handle_mouse_moved` just issued, letting the real cursor
    // wander to actual screen edges instead of staying pinned near the
    // anchor. The cursor being hidden (`CGDisplayHideCursor`) is what
    // actually keeps it invisible; consuming the event was never load-
    // bearing for that and only breaks the warp. Barrier's own comment:
    // "This should be harmless, but might register as slight movement to
    // other apps on the system. It hasn't been a problem before, though."
    let is_mouse_motion = matches!(
        event_type,
        K_CG_EVENT_MOUSE_MOVED
            | K_CG_EVENT_LEFT_MOUSE_DRAGGED
            | K_CG_EVENT_RIGHT_MOUSE_DRAGGED
            | K_CG_EVENT_OTHER_MOUSE_DRAGGED
    );

    if !is_mouse_motion && SUPPRESS.load(Ordering::SeqCst) {
        // Returning null swallows the event — it never reaches any other
        // app. This is what makes clicks/keys/scroll "disappear" during a
        // remote handoff.
        std::ptr::null_mut()
    } else {
        event
    }
}
