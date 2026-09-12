//! Windows global input capture via low-level hooks.
//!
//! `SetWindowsHookEx(WH_MOUSE_LL, ...)` / `WH_KEYBOARD_LL` requires a thread
//! with a running message pump, so this module spawns a dedicated OS thread
//! and runs `GetMessage` on it. The hook callbacks are invoked ON THAT
//! THREAD by the OS, synchronously, for every mouse/keyboard event on the
//! whole desktop.
//!
//! CRITICAL: the callback must return in well under the system
//! `LowLevelHooksTimeout`. If it exceeds it, Windows silently unregisters
//! the hook with no error and the app stops working. We therefore do
//! nothing in the callback except normalize the event and forward it to a
//! channel — see Tier 5.5 of the build guide.
//!
//! Unlike macOS, Windows gives no notification when a low-level hook is
//! dropped — for being too slow (`LowLevelHooksTimeout`), or across a
//! sleep/wake or a session switch. A `WM_TIMER` watchdog on the pump
//! thread covers all of those: it compares [`GetLastInputInfo`]'s
//! system-wide last-input time against the last time our own hooks fired,
//! and reinstalls the hooks if the system saw input we didn't (Tier 12's
//! sleep/wake recovery). `is_healthy()` still only reports whether the
//! pump thread is alive.

use std::cell::RefCell;
use std::collections::HashSet;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::mpsc as std_mpsc;
use std::thread::JoinHandle;

use tokio::sync::mpsc::UnboundedSender;
use windows::Win32::Foundation::{LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetLastInputInfo, LASTINPUTINFO, VK_CONTROL, VK_MENU, VK_RETURN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, GetSystemMetrics, HC_ACTION, HHOOK,
    KBDLLHOOKSTRUCT, KillTimer, LLKHF_EXTENDED, MSG, MSLLHOOKSTRUCT, PostThreadMessageW,
    SM_CXSCREEN, SM_CYSCREEN, SetCursorPos, SetTimer, SetWindowsHookExW, TranslateMessage,
    UnhookWindowsHookEx, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_APP, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER, WM_XBUTTONDOWN,
    WM_XBUTTONUP,
};

use seam_core::error::PlatformError;
use seam_core::protocol::{InputEvent, KeyCode, MouseButton};
use seam_core::traits::InputCapture;

use super::keycodes::vk_to_keycode;

/// Shared with the two hook callbacks below. There is only ever one active
/// capture instance per process — `current_platform()` constructs exactly
/// one `Platform` bundle — so a module-level static is simpler than, and
/// just as correct as, threading `self` through a raw OS callback pointer
/// (which the `HOOKPROC` signature has no room for anyway).
static SUPPRESS: AtomicBool = AtomicBool::new(false);

/// Center of the PRIMARY display, in virtual-desktop coordinates — where
/// the suppressed cursor is warped back to after EVERY real move while
/// driving a peer, not just when it happens to hit a monitor edge. Set
/// once in `set_suppression(true)`. Mirrors macOS's `ANCHOR_X`/`ANCHOR_Y`
/// and Barrier's own Windows primary-screen implementation
/// (`MSWindowsScreen::onMouseMove`'s `warpCursorNoFlush(m_xCenter,
/// m_yCenter)` on every move while driving a secondary). See
/// `handle_mouse_move`'s docs for why "warp to a fixed anchor every move"
/// replaced the earlier "only nudge off an edge once pinned" approach.
static ANCHOR_X: AtomicI32 = AtomicI32::new(0);
static ANCHOR_Y: AtomicI32 = AtomicI32::new(0);
/// Half the primary display's width/height, set alongside the anchor —
/// used by [`is_bogus_delta`] to drop a sample the OS may have clamped
/// before we saw it (Barrier's `bogusZoneSize` check).
static HALF_WIDTH: AtomicI32 = AtomicI32::new(0);
static HALF_HEIGHT: AtomicI32 = AtomicI32::new(0);

/// `GetTickCount()` (ms since boot) when a hook callback last fired.
/// Compared by the watchdog against [`GetLastInputInfo`]. Written from the
/// capture thread only; read there too — an atomic just to avoid UB on the
/// wrapping `u32`.
static LAST_HOOK_TICK: AtomicU32 = AtomicU32::new(0);

/// Watchdog cadence (ms). One `WM_TIMER` per this interval.
const WATCHDOG_INTERVAL_MS: u32 = 2000;
/// If the system saw input this many ms more recently than our hooks did,
/// the hooks are presumed dead and get reinstalled. Comfortably above the
/// watchdog interval so an idle machine never trips it.
const WATCHDOG_GRACE_MS: u32 = 5000;
/// Above this, treat the gap as a `GetTickCount` wrap (~49.7 days) rather
/// than a real miss.
const WATCHDOG_WRAP_GUARD_MS: u32 = u32::MAX / 2;
/// Arbitrary non-zero timer id for `SetTimer`/`KillTimer`.
const WATCHDOG_TIMER_ID: usize = 1;

/// Custom thread messages that move real mouse-move handling off the hook
/// callback (which must return in well under 1ms) and onto the pump
/// thread's own message queue, and fence the anchor warp against both its
/// own synthetic echo and any real event racing it — mirrors Barrier's
/// `BARRIER_MSG_MOUSE_MOVE`/`_PRE_WARP`/`_POST_WARP`
/// (`MSWindowsHook.cpp`/`MSWindowsScreen.cpp`). Declared in ascending
/// order: [`discard_until_post_warp`] filters `GetMessageW` to exactly this
/// range.
const MOUSE_MOVE_MSG: u32 = WM_APP + 1;
const PRE_WARP_MSG: u32 = WM_APP + 2;
const POST_WARP_MSG: u32 = WM_APP + 3;

thread_local! {
    // The hook callbacks run on the thread that called `SetWindowsHookExW`
    // (Windows delivers low-level hook events synchronously on that
    // thread's message queue), so this only needs to be visible there —
    // no lock needed on the hot path.
    static SINK: RefCell<Option<UnboundedSender<InputEvent>>> = const { RefCell::new(None) };

    // The currently-installed hooks, so the watchdog (same thread) can
    // swap them and `stop`'s teardown can unhook whatever is live now
    // rather than the originals.
    static MOUSE_HOOK: RefCell<Option<HHOOK>> = const { RefCell::new(None) };
    static KEYBOARD_HOOK: RefCell<Option<HHOOK>> = const { RefCell::new(None) };

    // `KBDLLHOOKSTRUCT` carries no "is this a repeat" bit (that only
    // existed in the classic WM_KEYDOWN lParam, not the low-level hook
    // struct), so we track currently-held keys ourselves to derive it.
    static HELD_KEYS: RefCell<HashSet<KeyCode>> = RefCell::new(HashSet::new());

    // The last absolute position we saw, used to compute
    // `InputEvent::MouseDelta` by diffing consecutive readings —
    // `MSLLHOOKSTRUCT` carries no delta field of its own (unlike macOS's
    // `CGEventGetIntegerValueField` with `kCGMouseEventDeltaX/Y`). Updated
    // both by `handle_mouse_move` (real motion) and by the `PRE_WARP_MSG`
    // handler in the pump loop (the upcoming warp target), mirroring
    // Barrier's `saveMousePosition` being called from both
    // `MSWindowsScreen::onMouseMove` and its `BARRIER_MSG_PRE_WARP` handler.
    // Only ever touched on the pump thread, since `handle_mouse_move` now
    // runs there too (deferred from the hook callback via `MOUSE_MOVE_MSG`)
    // rather than inside `mouse_proc` itself.
    static LAST_REAL_POS: RefCell<Option<POINT>> = const { RefCell::new(None) };
}

/// Windows implementation of [`seam_core::traits::InputCapture`].
pub struct Capture {
    thread: Option<JoinHandle<()>>,
    thread_id: Option<u32>,
}

impl Capture {
    /// Creates an inactive capture. Call `start` to actually install the
    /// hooks.
    #[must_use]
    pub fn new() -> Self {
        Self {
            thread: None,
            thread_id: None,
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
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<u32, String>>();

        let handle = std::thread::Builder::new()
            .name("seam-input-capture".into())
            .spawn(move || {
                SINK.with(|cell| *cell.borrow_mut() = Some(sink));

                // SAFETY: `mouse_proc`/`keyboard_proc` are `extern "system"`
                // functions matching the exact signature `SetWindowsHookExW`
                // requires. We pass `None` for `hmod` because both hooks are
                // installed for this process on this thread with no DLL
                // module to load, which is the documented combination for
                // WH_MOUSE_LL/WH_KEYBOARD_LL.
                let mouse_hook =
                    unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), None, 0) };
                // SAFETY: same reasoning as the mouse hook above.
                let keyboard_hook =
                    unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), None, 0) };

                match (mouse_hook, keyboard_hook) {
                    (Ok(m), Ok(k)) => {
                        MOUSE_HOOK.with(|c| *c.borrow_mut() = Some(m));
                        KEYBOARD_HOOK.with(|c| *c.borrow_mut() = Some(k));
                    }
                    (m, k) => {
                        // Clean up whichever one *did* register before
                        // reporting failure.
                        if let Ok(m) = m {
                            // SAFETY: `m` was just returned by a successful
                            // SetWindowsHookExW call above and hasn't been
                            // unhooked yet.
                            let _ = unsafe { UnhookWindowsHookEx(m) };
                        }
                        if let Ok(k) = k {
                            // SAFETY: same as above, for the keyboard hook.
                            let _ = unsafe { UnhookWindowsHookEx(k) };
                        }
                        let _ = ready_tx.send(Err(
                            "SetWindowsHookExW failed for one or both hooks — is this running \
                             interactively (not as a service)?"
                                .to_string(),
                        ));
                        SINK.with(|cell| *cell.borrow_mut() = None);
                        return;
                    }
                }

                // Seed the watchdog baseline so a tick before any real
                // input doesn't read as a miss, and start its timer.
                // SAFETY: `GetTickCount` has no preconditions.
                LAST_HOOK_TICK.store(unsafe { GetTickCount() }, Ordering::Relaxed);
                // SAFETY: a null `hwnd` + null `TIMERPROC` posts plain
                // `WM_TIMER` messages to this thread's queue, retrieved by
                // the `GetMessageW` loop below; the id is arbitrary.
                unsafe { SetTimer(None, WATCHDOG_TIMER_ID, WATCHDOG_INTERVAL_MS, None) };

                // SAFETY: `GetCurrentThreadId` has no preconditions.
                let thread_id = unsafe { GetCurrentThreadId() };
                let _ = ready_tx.send(Ok(thread_id));

                // Message pump. Low-level hooks are only delivered while
                // this thread is pumping messages — this loop IS the
                // capture, not just bookkeeping. `GetMessageW` blocks until
                // a message (including our own WM_QUIT from `stop()`, the
                // watchdog's `WM_TIMER`, and `mouse_proc`'s deferred
                // `MOUSE_MOVE_MSG`/`PRE_WARP_MSG`) arrives.
                let mut msg = MSG::default();
                // SAFETY: `msg` is a valid, exclusively-owned MSG the OS
                // fills in; `None, 0, 0` means "any message for this
                // thread".
                while unsafe { GetMessageW(&raw mut msg, None, 0, 0) }.as_bool() {
                    if msg.message == WM_TIMER && msg.wParam.0 == WATCHDOG_TIMER_ID {
                        watchdog_tick();
                        continue;
                    }
                    if msg.message == MOUSE_MOVE_MSG {
                        let (x, y) = unpack_point(msg.wParam, msg.lParam);
                        handle_mouse_move(x, y);
                        continue;
                    }
                    if msg.message == PRE_WARP_MSG {
                        // Save the warp target as the new delta baseline —
                        // Barrier's `saveMousePosition` inside its own
                        // `BARRIER_MSG_PRE_WARP` handler
                        // (`MSWindowsScreen.cpp:997`) — then fence off
                        // everything up to the matching `POST_WARP_MSG`.
                        let (x, y) = unpack_point(msg.wParam, msg.lParam);
                        LAST_REAL_POS.with(|cell| *cell.borrow_mut() = Some(POINT { x, y }));
                        discard_until_post_warp();
                        continue;
                    }
                    if msg.message == POST_WARP_MSG {
                        tracing::warn!("unmatched POST_WARP_MSG on the capture pump thread");
                        continue;
                    }
                    // SAFETY: `msg` was just populated by GetMessageW above.
                    unsafe {
                        let _ = TranslateMessage(&raw const msg);
                        DispatchMessageW(&raw const msg);
                    }
                }

                // SAFETY: `None` id-matches the timer set with a null hwnd
                // above; the hooks in the thread-locals are whatever the
                // watchdog last installed (or the originals) and are live.
                unsafe {
                    let _ = KillTimer(None, WATCHDOG_TIMER_ID);
                    if let Some(m) = MOUSE_HOOK.with(|c| c.borrow_mut().take()) {
                        let _ = UnhookWindowsHookEx(m);
                    }
                    if let Some(k) = KEYBOARD_HOOK.with(|c| c.borrow_mut().take()) {
                        let _ = UnhookWindowsHookEx(k);
                    }
                }
                SINK.with(|cell| *cell.borrow_mut() = None);
                HELD_KEYS.with(|cell| cell.borrow_mut().clear());
            })
            .map_err(|e| PlatformError::HookRegistrationFailed(e.to_string()))?;

        match ready_rx.recv() {
            Ok(Ok(thread_id)) => {
                self.thread = Some(handle);
                self.thread_id = Some(thread_id);
                Ok(())
            }
            Ok(Err(reason)) => {
                let _ = handle.join();
                Err(PlatformError::HookRegistrationFailed(reason))
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
        // Never tear down with suppression still latched on — this runs on
        // every session end, including an `abort()` mid-handoff (via
        // `Session`'s `Drop`).
        SUPPRESS.store(false, Ordering::SeqCst);
        if let Some(thread_id) = self.thread_id.take() {
            // SAFETY: posting WM_QUIT to a thread ID we obtained from
            // `GetCurrentThreadId` on that same (still-running) thread is
            // exactly the documented way to break its `GetMessageW` loop.
            let posted = unsafe { PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
            if posted.is_err() {
                return Err(PlatformError::Other(
                    "failed to post WM_QUIT to the capture thread".to_string(),
                ));
            }
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
        Ok(())
    }

    fn set_suppression(&mut self, suppress: bool) -> Result<(), PlatformError> {
        SUPPRESS.store(suppress, Ordering::SeqCst);
        if suppress {
            // SAFETY: `GetSystemMetrics` takes a plain metric index and has
            // no preconditions. `SM_CXSCREEN`/`SM_CYSCREEN` are the PRIMARY
            // display's size — its origin is always (0, 0) in Windows'
            // virtual-desktop coordinate space, so half its size is
            // directly the anchor point.
            let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
            let (cx, cy) = (w / 2, h / 2);
            ANCHOR_X.store(cx, Ordering::SeqCst);
            ANCHOR_Y.store(cy, Ordering::SeqCst);
            HALF_WIDTH.store(cx, Ordering::SeqCst);
            HALF_HEIGHT.store(cy, Ordering::SeqCst);
            // Deliberately no immediate `SetCursorPos` here to snap the
            // cursor to the anchor right away: unlike macOS's
            // `CGWarpMouseCursorPosition` (documented to never generate a
            // tap event, from any thread), Windows' `SetCursorPos` DOES
            // generate a real `WM_MOUSEMOVE` the low-level hook will see —
            // and this runs on the session thread, not the capture pump
            // thread, so it can't go through the `PRE_WARP_MSG`/
            // `POST_WARP_MSG` fence that protects every other warp (that
            // fence only works when posted from the pump thread itself).
            // Barrier's own `OSXScreen::leave`/Windows equivalent don't
            // warp proactively either — the anchor recentring happens
            // lazily on the first real move after suppression turns on,
            // inside `handle_mouse_move`'s normal (fenced) suppressed
            // branch. The local cursor visibly sits wherever it was until
            // then, matching Barrier's own Windows behaviour.
        }
        Ok(())
    }

    fn is_healthy(&self) -> bool {
        self.thread.as_ref().is_some_and(|h| !h.is_finished())
    }
}

impl Drop for Capture {
    /// Backstop for `Session`'s own `Drop`: a `Capture` dropped without
    /// `stop()` would leave the low-level hooks installed and (if a
    /// handoff was active) suppression latched on, swallowing all input.
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Extracts which side button (XBUTTON1/XBUTTON2) from `MSLLHOOKSTRUCT`'s
/// packed `mouseData` field: the button index lives in the high word.
fn xbutton(mouse_data: u32) -> MouseButton {
    if (mouse_data >> 16) & 0xFFFF == 1 {
        MouseButton::X1
    } else {
        MouseButton::X2
    }
}

/// Unpacks a wheel delta from `MSLLHOOKSTRUCT`'s packed `mouseData` field:
/// a signed 16-bit value in the high word, in units of `WHEEL_DELTA` (120).
fn wheel_delta(mouse_data: u32) -> i32 {
    let raw = (((mouse_data >> 16) & 0xFFFF) as u16).cast_signed();
    i32::from(raw) / 120
}

fn forward(event: InputEvent) {
    SINK.with(|cell| {
        if let Some(sink) = cell.borrow().as_ref() {
            // A full/closed channel means the async side is gone or
            // stalled; dropping the event is preferable to blocking this
            // callback, which must return in well under 1ms.
            let _ = sink.send(event);
        }
    });
}

/// How close a raw `dx`/`dy` is allowed to get to the distance between
/// the anchor and the primary screen's edge before it's treated as
/// possibly clamped and dropped — see [`is_bogus_delta`]. Barrier's own
/// `bogusZoneSize` (its Windows primary-screen implementation, same
/// technique).
const BOGUS_ZONE_PX: i32 = 10;

/// Packs an `(x, y)` pair into a thread-message's `WPARAM`/`LPARAM` —
/// sign-extended through the pointer-sized fields so [`unpack_point`]
/// round-trips negative coordinates (a monitor left of the primary has
/// negative virtual-desktop coordinates) exactly.
#[allow(clippy::cast_sign_loss, clippy::cast_possible_wrap)]
fn pack_point(x: i32, y: i32) -> (WPARAM, LPARAM) {
    (WPARAM(x as isize as usize), LPARAM(y as isize))
}

/// Inverse of [`pack_point`].
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn unpack_point(wparam: WPARAM, lparam: LPARAM) -> (i32, i32) {
    (wparam.0 as isize as i32, lparam.0 as i32)
}

/// Warps the cursor to `(x, y)` while fencing off both the resulting
/// synthetic move and any real hardware move that races it — Barrier's
/// `warpCursorNoFlush` (`MSWindowsScreen.cpp`), ported exactly. MUST be
/// called from the capture pump thread (the same thread the low-level
/// hooks and this thread's own message queue belong to); `set_suppression`
/// deliberately does NOT call this itself (see its doc comment) since it
/// runs on the session thread instead.
fn warp_cursor_no_flush(x: i32, y: i32) {
    // SAFETY: `GetCurrentThreadId` has no preconditions; this always
    // returns the id of whichever thread is executing right now, which by
    // this function's contract is the pump thread itself.
    let tid = unsafe { GetCurrentThreadId() };
    let (wx, wy) = pack_point(x, y);
    // SAFETY: posting to our own thread's message queue with a plain
    // integer payload; the pump loop's `PRE_WARP_MSG` arm reads it back via
    // `unpack_point`.
    let _ = unsafe { PostThreadMessageW(tid, PRE_WARP_MSG, wx, wy) };
    // SAFETY: `SetCursorPos` takes plain integer coordinates.
    let _ = unsafe { SetCursorPos(x, y) };
    // Yield the timeslice: there's a race where a hardware move occurs but
    // the hook isn't serviced yet because this thread has the CPU; without
    // yielding here, `POST_WARP_MSG` could get posted before that hardware
    // event's own `MOUSE_MOVE_MSG`, defeating the fence below. Barrier's
    // `ARCH->sleep(0.0)`, same rationale (`MSWindowsScreen.cpp:1526-1541`).
    std::thread::yield_now();
    // SAFETY: as above.
    let _ = unsafe { PostThreadMessageW(tid, POST_WARP_MSG, WPARAM(0), LPARAM(0)) };
}

/// Discards every message in `[MOUSE_MOVE_MSG, POST_WARP_MSG]` until
/// `POST_WARP_MSG` itself arrives — Barrier's exact `BARRIER_MSG_PRE_WARP`
/// handler (`MSWindowsScreen.cpp:994-1010`). This is what makes the warp
/// safe: it deterministically eats both `SetCursorPos`'s own synthetic echo
/// and any real hardware move that raced it, rather than trying to
/// distinguish them after the fact.
fn discard_until_post_warp() {
    let mut msg = MSG::default();
    loop {
        // SAFETY: `msg` is a valid, exclusively-owned MSG the OS fills in;
        // `GetMessageW`'s range filter is the documented way to restrict
        // which messages it retrieves.
        let ok = unsafe { GetMessageW(&raw mut msg, None, MOUSE_MOVE_MSG, POST_WARP_MSG) };
        if !ok.as_bool() {
            // WM_QUIT bypasses GetMessageW's id-range filter (Microsoft
            // documents it as always retrieved) and would otherwise be
            // silently consumed here mid-fence, hanging `stop()`'s
            // `PostThreadMessageW(WM_QUIT)` forever. Repost it so the outer
            // pump loop still observes it and exits.
            // SAFETY: posting to our own thread, no preconditions beyond
            // that.
            let _ =
                unsafe { PostThreadMessageW(GetCurrentThreadId(), WM_QUIT, WPARAM(0), LPARAM(0)) };
            break;
        }
        if msg.message == POST_WARP_MSG {
            break;
        }
    }
}

/// Handles one real mouse move already dequeued from the pump thread's own
/// queue as [`MOUSE_MOVE_MSG`] — mirrors `MSWindowsScreen::onMouseMove`.
/// `mouse_proc` never calls this directly: it only posts `MOUSE_MOVE_MSG`
/// and returns immediately, keeping the hook callback itself fast.
///
/// Delta is computed by diffing against [`LAST_REAL_POS`] (`MSLLHOOKSTRUCT`
/// carries no delta field of its own, unlike macOS's
/// `CGEventGetIntegerValueField` with `kCGMouseEventDeltaX/Y`). Emits
/// exactly one event, matching Barrier's `isOnScreen` branch: an absolute
/// position while local, or (after `warp_cursor_no_flush` and a
/// `is_bogus_delta` pass) an accumulated delta while suppressed — never
/// both, and only for genuine motion since the fence in
/// `discard_until_post_warp` already stripped the warp's own echo before it
/// could ever reach here.
fn handle_mouse_move(mx: i32, my: i32) {
    let previous = LAST_REAL_POS.with(|cell| cell.borrow_mut().replace(POINT { x: mx, y: my }));
    let Some(previous) = previous else {
        return;
    };
    let (dx, dy) = (mx - previous.x, my - previous.y);
    if dx == 0 && dy == 0 {
        return;
    }

    if !SUPPRESS.load(Ordering::SeqCst) {
        forward(InputEvent::MouseMoveAbs { x: mx, y: my });
        return;
    }

    // Motion on the secondary (peer) screen: warp back to the anchor so the
    // cursor never approaches an edge, then examine the motion that led up
    // to this warp.
    warp_cursor_no_flush(
        ANCHOR_X.load(Ordering::SeqCst),
        ANCHOR_Y.load(Ordering::SeqCst),
    );

    if is_bogus_delta(dx, dy) {
        tracing::debug!(dx, dy, "dropped bogus motion");
        return;
    }
    forward(InputEvent::MouseDelta { dx, dy });
}

/// While suppressed, `dx`/`dy` are measured from the fixed anchor (see
/// `handle_mouse_move`). If either component is within [`BOGUS_ZONE_PX`]
/// of the distance from the anchor to the primary screen's edge, the
/// physical motion may have been larger than reported — the OS clamps the
/// cursor at the real screen edge before our hook ever sees it, so a
/// single very fast flick can under-report. Barrier keeps this same
/// `bogusZoneSize` check as a backup even with its PRE_WARP/POST_WARP fence
/// in place, and so do we.
fn is_bogus_delta(dx: i32, dy: i32) -> bool {
    let half_width = HALF_WIDTH.load(Ordering::SeqCst);
    let half_height = HALF_HEIGHT.load(Ordering::SeqCst);
    dx.abs() + BOGUS_ZONE_PX > half_width || dy.abs() + BOGUS_ZONE_PX > half_height
}

/// One watchdog tick (runs on the pump thread, off `WM_TIMER`). If the
/// system has seen input more recently than our hooks have — the
/// signature of a silently dropped low-level hook after a
/// `LowLevelHooksTimeout`, a sleep/wake, or a session switch — reinstall
/// both hooks.
fn watchdog_tick() {
    let mut lii = LASTINPUTINFO {
        cbSize: u32::try_from(size_of::<LASTINPUTINFO>()).unwrap_or_default(),
        ..Default::default()
    };
    // SAFETY: `lii` is a fully-initialized `LASTINPUTINFO` with its
    // `cbSize` set, exactly as `GetLastInputInfo` requires.
    if unsafe { GetLastInputInfo(&raw mut lii) }.as_bool() {
        let ours = LAST_HOOK_TICK.load(Ordering::Relaxed);
        let gap = lii.dwTime.wrapping_sub(ours);
        if gap > WATCHDOG_GRACE_MS && gap < WATCHDOG_WRAP_GUARD_MS {
            tracing::warn!(
                gap_ms = gap,
                "low-level hooks look dead (sleep/wake, session switch, or timeout); reinstalling"
            );
            reinstall_hooks();
        }
    }
}

/// Tears down the current hooks and installs fresh ones. Old-before-new,
/// so a stuck event can't be delivered twice (a sub-ms hookless gap is
/// the lesser evil, and the next tick retries a partial failure). The
/// per-key/per-position state in the thread-locals survives the swap.
fn reinstall_hooks() {
    // SAFETY: each handle in the thread-locals is a live hook this thread
    // installed; unhooking on the installing thread is required and sound.
    unsafe {
        if let Some(m) = MOUSE_HOOK.with(|c| c.borrow_mut().take()) {
            let _ = UnhookWindowsHookEx(m);
        }
        if let Some(k) = KEYBOARD_HOOK.with(|c| c.borrow_mut().take()) {
            let _ = UnhookWindowsHookEx(k);
        }
    }
    // SAFETY: same signature/`hmod` reasoning as the initial install in
    // `start`.
    let m = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), None, 0) };
    // SAFETY: as above.
    let k = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), None, 0) };
    let ok = m.is_ok() && k.is_ok();
    MOUSE_HOOK.with(|c| *c.borrow_mut() = m.ok());
    KEYBOARD_HOOK.with(|c| *c.borrow_mut() = k.ok());
    // SAFETY: no preconditions.
    LAST_HOOK_TICK.store(unsafe { GetTickCount() }, Ordering::Relaxed);
    if ok {
        tracing::info!("low-level hooks reinstalled");
    } else {
        tracing::error!("hook reinstall failed; retrying on the next watchdog tick");
    }
}

/// Records that a hook callback just fired, for [`watchdog_tick`].
fn stamp_hook_activity() {
    // SAFETY: `GetTickCount` has no preconditions.
    LAST_HOOK_TICK.store(unsafe { GetTickCount() }, Ordering::Relaxed);
}

/// # Safety
/// Called by the OS per the `WH_MOUSE_LL` contract: `ncode`/`wparam`/
/// `lparam` are whatever the system passes to a low-level mouse hook
/// procedure. We only dereference `lparam` as `*const MSLLHOOKSTRUCT` when
/// `ncode == HC_ACTION`, which MSDN documents as the condition under which
/// that pointer is valid.
unsafe extern "system" fn mouse_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if ncode == HC_ACTION.cast_signed() {
        stamp_hook_activity();
        // SAFETY: see function-level SAFETY comment above.
        let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        // The hook's wParam carries a WM_* message id, always small enough
        // to fit u32 even though it's widened to usize on 64-bit targets.
        let msg = u32::try_from(wparam.0).unwrap_or(u32::MAX);
        let event = match msg {
            WM_MOUSEMOVE => {
                // Deferred to the pump thread as `MOUSE_MOVE_MSG` — the
                // real work (delta computation, the anchor-warp fence) needs
                // the pump thread's own message queue and can't happen in
                // this callback, which must return in well under 1ms.
                let (wx, wy) = pack_point(info.pt.x, info.pt.y);
                // SAFETY: low-level hooks always run on the thread that
                // installed them, so `GetCurrentThreadId` here is the pump
                // thread; posting a plain integer payload to its own queue.
                let _ = unsafe { PostThreadMessageW(GetCurrentThreadId(), MOUSE_MOVE_MSG, wx, wy) };
                None
            }
            WM_LBUTTONDOWN => Some(InputEvent::MouseDown {
                button: MouseButton::Left,
            }),
            WM_LBUTTONUP => Some(InputEvent::MouseUp {
                button: MouseButton::Left,
            }),
            WM_RBUTTONDOWN => Some(InputEvent::MouseDown {
                button: MouseButton::Right,
            }),
            WM_RBUTTONUP => Some(InputEvent::MouseUp {
                button: MouseButton::Right,
            }),
            WM_MBUTTONDOWN => Some(InputEvent::MouseDown {
                button: MouseButton::Middle,
            }),
            WM_MBUTTONUP => Some(InputEvent::MouseUp {
                button: MouseButton::Middle,
            }),
            WM_XBUTTONDOWN => Some(InputEvent::MouseDown {
                button: xbutton(info.mouseData),
            }),
            WM_XBUTTONUP => Some(InputEvent::MouseUp {
                button: xbutton(info.mouseData),
            }),
            WM_MOUSEWHEEL => Some(InputEvent::Scroll {
                dx: 0,
                dy: wheel_delta(info.mouseData),
            }),
            WM_MOUSEHWHEEL => Some(InputEvent::Scroll {
                dx: wheel_delta(info.mouseData),
                dy: 0,
            }),
            _ => None,
        };
        if let Some(event) = event {
            forward(event);
        }
    }

    if ncode == HC_ACTION.cast_signed() && SUPPRESS.load(Ordering::SeqCst) {
        // Non-zero return swallows the event: it never reaches the rest of
        // the hook chain or the target window. This is what makes the
        // local cursor "disappear" during a remote handoff.
        return LRESULT(1);
    }
    // SAFETY: forwarding to the next hook in the chain with the exact
    // parameters we were given is always sound; the OS ignores the first
    // argument for low-level hooks.
    unsafe { CallNextHookEx(None, ncode, wparam, lparam) }
}

/// Resolves a `KBDLLHOOKSTRUCT` into our normalized `KeyCode`, handling the
/// left/right disambiguation `vk_to_keycode` alone can't do — see the
/// module docs on `keycodes.rs`.
fn resolve_keycode(info: &KBDLLHOOKSTRUCT) -> KeyCode {
    let extended = (info.flags.0 & LLKHF_EXTENDED.0) != 0;
    // VK codes are always 8-bit despite `vkCode`'s u32 field type.
    let vk = u16::try_from(info.vkCode).unwrap_or(0);

    if vk == VK_CONTROL.0 {
        return if extended {
            KeyCode::RightCtrl
        } else {
            KeyCode::LeftCtrl
        };
    }
    if vk == VK_MENU.0 {
        return if extended {
            KeyCode::RightAlt
        } else {
            KeyCode::LeftAlt
        };
    }
    if vk == VK_SHIFT.0 {
        // The extended flag is never set for either physical Shift key, so
        // this is the one modifier that has to be disambiguated by scan
        // code instead: 0x36 is right Shift, everything else is left.
        return if info.scanCode == 0x36 {
            KeyCode::RightShift
        } else {
            KeyCode::LeftShift
        };
    }
    if vk == VK_RETURN.0 && extended {
        return KeyCode::NumpadEnter;
    }

    vk_to_keycode(vk)
}

/// # Safety
/// Called by the OS per the `WH_KEYBOARD_LL` contract; see `mouse_proc`'s
/// SAFETY comment — the same reasoning applies to `KBDLLHOOKSTRUCT` here.
unsafe extern "system" fn keyboard_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if ncode == HC_ACTION.cast_signed() {
        stamp_hook_activity();
        // SAFETY: see function-level SAFETY comment above.
        let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        // The hook's wParam carries a WM_* message id, always small enough
        // to fit u32 even though it's widened to usize on 64-bit targets.
        let msg = u32::try_from(wparam.0).unwrap_or(u32::MAX);
        let code = resolve_keycode(info);

        match msg {
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                let repeat = HELD_KEYS.with(|cell| !cell.borrow_mut().insert(code));
                forward(InputEvent::KeyDown { code, repeat });
            }
            WM_KEYUP | WM_SYSKEYUP => {
                HELD_KEYS.with(|cell| {
                    cell.borrow_mut().remove(&code);
                });
                forward(InputEvent::KeyUp { code });
            }
            _ => {}
        }
    }

    if ncode == HC_ACTION.cast_signed() && SUPPRESS.load(Ordering::SeqCst) {
        return LRESULT(1);
    }
    // SAFETY: same reasoning as the equivalent call in `mouse_proc`.
    unsafe { CallNextHookEx(None, ncode, wparam, lparam) }
}
