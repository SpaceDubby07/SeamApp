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
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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
    KBDLLHOOKSTRUCT, KillTimer, LLKHF_EXTENDED, LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT,
    PostThreadMessageW, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, SetCursorPos, SetTimer, SetWindowsHookExW, TranslateMessage,
    UnhookWindowsHookEx, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN,
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

    // Tier 7.2: the last real (non-injected) absolute position we saw,
    // used to compute `InputEvent::MouseDelta` — `MSLLHOOKSTRUCT` carries
    // no delta field of its own (unlike macOS's `CGEventGetIntegerValueField`
    // with `kCGMouseEventDeltaX/Y`), so this is derived by diffing
    // consecutive readings instead. See `recenter_if_pinned`'s docs for
    // why this alone isn't enough once suppressed.
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
                // a message (including our own WM_QUIT from `stop()` and
                // the watchdog's `WM_TIMER`) arrives.
                let mut msg = MSG::default();
                // SAFETY: `msg` is a valid, exclusively-owned MSG the OS
                // fills in; `None, 0, 0` means "any message for this
                // thread".
                while unsafe { GetMessageW(&raw mut msg, None, 0, 0) }.as_bool() {
                    if msg.message == WM_TIMER && msg.wParam.0 == WATCHDOG_TIMER_ID {
                        watchdog_tick();
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

/// Tier 7.2: pulled back off whichever virtual-desktop edge a suppressed
/// cursor is pinned against, once `recenter_if_pinned` warps it — large
/// enough that the OS doesn't immediately re-clamp on the very next tiny
/// real movement, small enough to stay well within any real multi-monitor
/// layout.
const RECENTER_MARGIN_PX: i32 = 200;

/// Handles one `WM_MOUSEMOVE`: derives `MouseDelta` from the raw absolute
/// reading (`MSLLHOOKSTRUCT` carries no delta field of its own, unlike
/// macOS's `CGEventGetIntegerValueField` with `kCGMouseEventDeltaX/Y`),
/// and — while suppressed — proactively un-clamps the cursor from
/// whichever virtual-desktop edge it's pinned against.
///
/// # Why the recenter warp is needed at all
/// Once `RemoteActive`, our own cursor sits suppressed at the edge that
/// triggered the handoff. Windows clamps `MSLLHOOKSTRUCT.pt` to the
/// virtual desktop's bounds, so once the real cursor is pinned at x=0 (or
/// any other edge), further real hardware motion in that direction keeps
/// arriving as `WM_MOUSEMOVE` but with the EXACT SAME `pt` every time —
/// there is no lower value for the OS to report. Nudging the cursor back
/// by `RECENTER_MARGIN_PX` gives the OS room to register fresh motion
/// again, so delta keeps flowing indefinitely in the same direction
/// rather than dying the instant the edge is reached.
///
/// # Why this doesn't fight anything downstream
/// The warp is filtered from ever becoming a `MouseMoveAbs` reading at all
/// (see the `injected` check below); only the true `MouseDelta`s it keeps
/// alive are forwarded, and those are what `seam-core::session` relays to
/// the peer as continued motion while driving. Reclaim itself now lives on
/// the driven side (`ControlMessage::ReleaseBack`), so a recenter warp
/// can't be mistaken for a reclaim gesture.
fn handle_mouse_move(info: &MSLLHOOKSTRUCT) -> Option<InputEvent> {
    // `LLMHF_INJECTED` marks an event as having come from `SendInput`/
    // `SetCursorPos` rather than real hardware — exactly what
    // `recenter_if_pinned` generates when it warps us. Silently resync the
    // delta baseline to it and stop: it must never be treated as real
    // motion, or it would both double-count as a spurious `MouseDelta` and
    // look like a false reclaim gesture.
    if (info.flags & LLMHF_INJECTED) != 0 {
        LAST_REAL_POS.with(|cell| *cell.borrow_mut() = Some(info.pt));
        return None;
    }

    let previous = LAST_REAL_POS.with(|cell| cell.borrow_mut().replace(info.pt));
    if let Some(previous) = previous {
        let dx = info.pt.x - previous.x;
        let dy = info.pt.y - previous.y;
        if dx != 0 || dy != 0 {
            forward(InputEvent::MouseDelta { dx, dy });
        }
    }

    if SUPPRESS.load(Ordering::SeqCst) {
        recenter_if_pinned(info.pt);
    }

    Some(InputEvent::MouseMoveAbs {
        x: info.pt.x,
        y: info.pt.y,
    })
}

/// Warps the cursor `RECENTER_MARGIN_PX` off any virtual-desktop edge
/// `pt` is currently pinned against. See `handle_mouse_move`'s docs for
/// why this exists and why it's safe against feedback loops/false
/// reclaims.
fn recenter_if_pinned(pt: POINT) {
    // SAFETY: `GetSystemMetrics` takes a plain metric index and has no
    // preconditions.
    let (left, top, width, height) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };

    let mut target = pt;
    let mut pinned = false;
    if pt.x <= left {
        target.x = left + RECENTER_MARGIN_PX;
        pinned = true;
    } else if pt.x >= left + width - 1 {
        target.x = left + width - 1 - RECENTER_MARGIN_PX;
        pinned = true;
    }
    if pt.y <= top {
        target.y = top + RECENTER_MARGIN_PX;
        pinned = true;
    } else if pt.y >= top + height - 1 {
        target.y = top + height - 1 - RECENTER_MARGIN_PX;
        pinned = true;
    }

    if pinned {
        // SAFETY: `SetCursorPos` takes plain integer coordinates; the
        // resulting synthetic `WM_MOUSEMOVE` is what `handle_mouse_move`
        // filters via `LLMHF_INJECTED` above.
        let _ = unsafe { SetCursorPos(target.x, target.y) };
        LAST_REAL_POS.with(|cell| *cell.borrow_mut() = Some(target));
    }
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
            WM_MOUSEMOVE => handle_mouse_move(info),
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
