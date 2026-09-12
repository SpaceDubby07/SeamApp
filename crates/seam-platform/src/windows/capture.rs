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
//!
//! # Keyboard detection is Raw Input, not `WH_KEYBOARD_LL`
//! Mouse capture/suppression is entirely hook-based, same as ever. Keyboard
//! is split: `WH_KEYBOARD_LL` (`keyboard_proc`) is kept only for the one
//! thing a hook can do that Raw Input can't — blocking local delivery while
//! `SUPPRESS` is set — but actual `KeyDown`/`KeyUp` detection comes from
//! Raw Input (`register_raw_keyboard`/`handle_raw_input`) via a hidden
//! message-only window this module also owns. See `register_raw_keyboard`'s
//! doc comment for why: a real two-machine test showed the low-level hook
//! reliably seeing modifier keys but never a single regular letter, on
//! both driving directions, which points at another globally-installed
//! hook earlier in the chain (common in gaming/RGB keyboard software)
//! swallowing regular keys before ours ever sees them — a class of problem
//! Raw Input's separate, hook-chain-independent delivery path sidesteps.

use std::cell::RefCell;
use std::collections::HashSet;
use std::mem::size_of;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::mpsc as std_mpsc;
use std::thread::JoinHandle;

use tokio::sync::mpsc::UnboundedSender;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetLastInputInfo, LASTINPUTINFO, VK_CONTROL, VK_MENU, VK_RETURN, VK_SHIFT,
};
use windows::Win32::UI::Input::{
    GetRawInputData, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RID_INPUT,
    RIDEV_INPUTSINK, RIM_TYPEKEYBOARD, RegisterRawInputDevices,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CW_USEDEFAULT, CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow,
    DispatchMessageW, GetMessageW, GetSystemMetrics, HC_ACTION, HHOOK, HWND_MESSAGE,
    KBDLLHOOKSTRUCT, KillTimer, LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT, PostThreadMessageW,
    RI_KEY_BREAK, RI_KEY_E0, RI_KEY_E1, RegisterClassExW, SM_CXSCREEN, SM_CYSCREEN, SetCursorPos,
    SetTimer, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL,
    WH_MOUSE_LL, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_INPUT, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_QUIT,
    WM_RBUTTONDOWN, WM_RBUTTONUP, WM_TIMER, WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSEXW,
};
use windows::core::{PCWSTR, w};

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

/// Window class for the hidden message-only window `WM_INPUT` is delivered
/// to (see `create_message_window`) — distinct from `clipboard.rs`'s own
/// class, since each `RegisterClassExW` name is a process-global template
/// tied to one `WNDPROC`.
const WINDOW_CLASS_NAME: PCWSTR = w!("SeamInputCapture");

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

    // Raw Input's `RAWKEYBOARD` carries no "is this a repeat" bit either,
    // so we track currently-held keys ourselves to derive it — same reason
    // as before, just fed from `handle_raw_input` now instead of
    // `keyboard_proc`.
    static HELD_KEYS: RefCell<HashSet<KeyCode>> = RefCell::new(HashSet::new());

    // The message-only window created for `WM_INPUT` delivery (see
    // `create_message_window`), so `stop`'s teardown can destroy it.
    static RAW_INPUT_HWND: RefCell<Option<HWND>> = const { RefCell::new(None) };

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
                        // Diagnostic: confirms both hooks actually installed
                        // (as opposed to a keyboard-specific block by AV/EDR
                        // software that a bare Ok(HHOOK) from
                        // SetWindowsHookExW wouldn't otherwise reveal at
                        // keypress time).
                        tracing::info!("low-level mouse + keyboard hooks installed");

                        // Raw Input keyboard registration is a soft
                        // dependency: mouse capture (and keyboard
                        // suppression, via the hook above) must not fail
                        // just because this couldn't be set up. On failure
                        // we log and carry on — regular-key detection
                        // degrades to whatever `keyboard_proc` alone can
                        // see (modifiers, per the investigation in
                        // `register_raw_keyboard`'s docs), rather than
                        // losing mouse capture too.
                        match create_message_window() {
                            Ok(hwnd) => {
                                if let Err(reason) = register_raw_keyboard(hwnd) {
                                    tracing::warn!(
                                        reason,
                                        "raw input keyboard registration failed; falling back to \
                                         WH_KEYBOARD_LL alone"
                                    );
                                    // SAFETY: `hwnd` was just created above
                                    // and nothing else references it yet.
                                    unsafe {
                                        let _ = DestroyWindow(hwnd);
                                    }
                                } else {
                                    RAW_INPUT_HWND.with(|c| *c.borrow_mut() = Some(hwnd));
                                }
                            }
                            Err(reason) => {
                                tracing::warn!(
                                    reason,
                                    "creating the raw input message window failed; falling back \
                                     to WH_KEYBOARD_LL alone"
                                );
                            }
                        }
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
                    if let Some(hwnd) = RAW_INPUT_HWND.with(|c| c.borrow_mut().take()) {
                        let _ = DestroyWindow(hwnd);
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

/// Registers the message-window class exactly once per process — mirrors
/// `clipboard.rs`'s `ensure_class_registered` (same rationale: a window
/// class is a process-global template, and re-registering the same name on
/// a reconnect fails with `ERROR_CLASS_ALREADY_EXISTS`).
fn ensure_class_registered() -> Result<(), String> {
    static REGISTERED: OnceLock<Result<(), String>> = OnceLock::new();
    REGISTERED
        .get_or_init(|| {
            // SAFETY: `GetModuleHandleW(None)` returns this process's own
            // module handle, valid to register a class against.
            let hinstance = unsafe { GetModuleHandleW(None) }
                .map_err(|e| format!("GetModuleHandleW failed: {e}"))?;
            let wc = WNDCLASSEXW {
                cbSize: u32::try_from(size_of::<WNDCLASSEXW>()).unwrap_or_default(),
                lpfnWndProc: Some(wndproc),
                hInstance: hinstance.into(),
                lpszClassName: WINDOW_CLASS_NAME,
                ..Default::default()
            };
            // SAFETY: `wc` is a fully initialized `WNDCLASSEXW`; registering
            // a window class this way is always sound.
            if unsafe { RegisterClassExW(&raw const wc) } == 0 {
                return Err("RegisterClassExW failed".to_string());
            }
            Ok(())
        })
        .clone()
}

/// Creates a hidden, message-only window (`HWND_MESSAGE` parent) purely to
/// receive `WM_INPUT` — never shown, no visible content. Must be created on
/// the capture pump thread: `RegisterRawInputDevices` delivers `WM_INPUT`
/// to whichever thread's message queue owns `hwndTarget`, and that thread
/// is the only one whose `GetMessageW` loop will ever see it.
fn create_message_window() -> Result<HWND, String> {
    ensure_class_registered()?;

    // SAFETY: `GetModuleHandleW(None)` returns a handle to this process's
    // own module.
    let hinstance =
        unsafe { GetModuleHandleW(None) }.map_err(|e| format!("GetModuleHandleW failed: {e}"))?;

    // SAFETY: creating a message-only window with the class registered by
    // `ensure_class_registered`; `HWND_MESSAGE` as the parent and no window
    // style is the documented combination for a window that never becomes
    // visible and needs no message loop beyond delivering messages to us.
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            WINDOW_CLASS_NAME,
            WINDOW_CLASS_NAME,
            WINDOW_STYLE::default(),
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            Some(HWND_MESSAGE),
            None,
            Some(hinstance.into()),
            None,
        )
    }
    .map_err(|e| format!("CreateWindowExW failed: {e}"))?;

    Ok(hwnd)
}

/// Registers this process for raw keyboard input, delivered to `hwnd` as
/// `WM_INPUT`. `RIDEV_INPUTSINK` is what makes delivery work even while our
/// window has no focus and isn't foreground — the normal case, since we're
/// capturing global input while some other window is active.
///
/// # Why: `WH_KEYBOARD_LL` alone isn't reliable for this
/// A real two-machine test showed `WH_KEYBOARD_LL` reliably seeing
/// modifier keys (including this machine's own `SendInput`-injected ones —
/// see `keyboard_proc`'s history) but NEVER a single regular letter/number
/// key, on both directions of driving, across multiple sessions. Low-level
/// hooks are cooperative: any other globally-installed hook earlier in the
/// chain can swallow an event and stop it from ever reaching ours, and
/// gaming/RGB keyboard software commonly installs exactly this kind of
/// hook to watch for macro keys — plausibly explaining an asymmetry where
/// modifiers (rarely bound to macros) pass through untouched while regular
/// keys don't. Raw Input reads from the HID input queue via a separate
/// registration mechanism that Windows guarantees delivery for regardless
/// of what any other process's hook chain does with the same event
/// afterward, so it's used here as the actual detection source for
/// `KeyDown`/`KeyUp`. `WH_KEYBOARD_LL` (`keyboard_proc`) is kept, but only
/// for what a hook can do that Raw Input can't: suppressing local delivery
/// while `SUPPRESS` is set.
fn register_raw_keyboard(hwnd: HWND) -> Result<(), String> {
    const HID_USAGE_PAGE_GENERIC: u16 = 0x01;
    const HID_USAGE_GENERIC_KEYBOARD: u16 = 0x06;
    let device = RAWINPUTDEVICE {
        usUsagePage: HID_USAGE_PAGE_GENERIC,
        usUsage: HID_USAGE_GENERIC_KEYBOARD,
        dwFlags: RIDEV_INPUTSINK,
        hwndTarget: hwnd,
    };
    // SAFETY: `device` is a single, fully-initialized `RAWINPUTDEVICE`
    // targeting `hwnd`, which the caller guarantees is valid and owned by
    // this thread.
    unsafe {
        RegisterRawInputDevices(
            &[device],
            u32::try_from(size_of::<RAWINPUTDEVICE>()).unwrap_or_default(),
        )
    }
    .map_err(|e| format!("RegisterRawInputDevices failed: {e}"))
}

/// # Safety
/// Called by the OS per the standard `WNDPROC` contract for the window
/// class registered in `create_message_window`.
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_INPUT {
        handle_raw_input(lparam);
        // Fall through to `DefWindowProcW` regardless: Microsoft's own
        // docs for `WM_INPUT` say to call it even after handling the
        // message yourself, for cleanup.
    }
    // SAFETY: forwarding to the default window procedure with the exact
    // parameters we were given is always sound.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Reads one `WM_INPUT` payload and, if it's a real (non-synthetic)
/// keyboard event, resolves and forwards it as `InputEvent::KeyDown`/
/// `KeyUp` — this is the actual detection path; see `register_raw_keyboard`
/// for why `keyboard_proc`/`WH_KEYBOARD_LL` no longer does this.
fn handle_raw_input(lparam: LPARAM) {
    // SAFETY: `lparam` is the `WM_INPUT` payload handed to us by `wndproc`;
    // reinterpreting its bit pattern as `HRAWINPUT` matches what
    // `GetRawInputData` expects for that message.
    let hrawinput = HRAWINPUT(lparam.0 as *mut std::ffi::c_void);
    let header_size = u32::try_from(size_of::<RAWINPUTHEADER>()).unwrap_or(0);

    // First call: query the required buffer size (the documented two-step
    // `GetRawInputData` pattern — a `RAWINPUT` is variable-sized).
    let mut size: u32 = 0;
    // SAFETY: `pdata: None` means "just tell us the size"; `size` is a
    // valid, exclusively-owned `u32` for the OS to write into.
    let query = unsafe { GetRawInputData(hrawinput, RID_INPUT, None, &raw mut size, header_size) };
    if query != 0 || size == 0 {
        return;
    }

    let mut buf = vec![0u8; size as usize];
    // SAFETY: `buf` is sized exactly to what the query above reported;
    // `size` is re-passed as an in/out capacity, matching the documented
    // second-call contract.
    let read = unsafe {
        GetRawInputData(
            hrawinput,
            RID_INPUT,
            Some(buf.as_mut_ptr().cast()),
            &raw mut size,
            header_size,
        )
    };
    // `GetRawInputData` returns `u32::MAX` (cast from `(UINT)-1`) on
    // failure, or the number of bytes written on success.
    if read == u32::MAX || read as usize != buf.len() {
        return;
    }

    // SAFETY: `buf` holds a fully-populated `RAWINPUT` per the successful
    // read above — its declared size came from the OS itself.
    let raw = unsafe { &*buf.as_ptr().cast::<RAWINPUT>() };
    if raw.header.dwType != RIM_TYPEKEYBOARD.0 {
        return;
    }
    // A null device handle marks input Windows synthesized (e.g. our own
    // `SendInput` calls) rather than a real physical device — the Raw
    // Input equivalent of `LLKHF_INJECTED`/`LLMHF_INJECTED`. Without this,
    // `Sink::release_all_modifiers`'s sweep would echo right back in here
    // too, the same self-feedback bug already fixed for `keyboard_proc`
    // and the mouse hooks.
    if raw.header.hDevice.0.is_null() {
        return;
    }

    // SAFETY: `dwType` was just confirmed `RIM_TYPEKEYBOARD` above, so
    // `.keyboard` is the active union member.
    let kb = unsafe { raw.data.keyboard };
    let up = kb.Flags & RI_KEY_BREAK_U16 != 0;
    let extended = kb.Flags & (RI_KEY_E0_U16 | RI_KEY_E1_U16) != 0;
    let code = resolve_keycode_raw(kb.VKey, kb.MakeCode, extended);

    if up {
        HELD_KEYS.with(|cell| {
            cell.borrow_mut().remove(&code);
        });
        forward(InputEvent::KeyUp { code });
    } else {
        let repeat = HELD_KEYS.with(|cell| !cell.borrow_mut().insert(code));
        forward(InputEvent::KeyDown { code, repeat });
    }
}

// `RAWKEYBOARD::Flags` is `u16`; the `RI_KEY_*` constants windows-rs
// exposes are `u32` (matching the Win32 header, which defines them as
// plain `#define`s with no fixed width). Narrowed once here rather than
// converting at each use.
const RI_KEY_BREAK_U16: u16 = RI_KEY_BREAK as u16;
const RI_KEY_E0_U16: u16 = RI_KEY_E0 as u16;
const RI_KEY_E1_U16: u16 = RI_KEY_E1 as u16;

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
        // `LLMHF_INJECTED` marks an event as coming from `SendInput` rather
        // than real hardware — e.g. our own button/wheel injection while
        // `BeingDriven`. Skip the whole dispatch below for these: without
        // this, injecting a click on this machine gets immediately
        // re-captured by this same hook and fed back into
        // `process_capture_event` as if it were fresh local input (the same
        // bug class just found and fixed in `keyboard_proc`). Real
        // `WM_MOUSEMOVE` warp-echo filtering is handled separately by the
        // PRE_WARP/POST_WARP fence, not this flag, but the flag still
        // matters here for injected buttons/wheel, which the fence doesn't
        // cover.
        let injected = (info.flags & LLMHF_INJECTED) != 0;
        let event = if injected {
            None
        } else {
            match msg {
                WM_MOUSEMOVE => {
                    // Deferred to the pump thread as `MOUSE_MOVE_MSG` — the
                    // real work (delta computation, the anchor-warp fence) needs
                    // the pump thread's own message queue and can't happen in
                    // this callback, which must return in well under 1ms.
                    let (wx, wy) = pack_point(info.pt.x, info.pt.y);
                    // SAFETY: low-level hooks always run on the thread that
                    // installed them, so `GetCurrentThreadId` here is the pump
                    // thread; posting a plain integer payload to its own queue.
                    let _ =
                        unsafe { PostThreadMessageW(GetCurrentThreadId(), MOUSE_MOVE_MSG, wx, wy) };
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
            }
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

/// Resolves a `(VKey, scan code, extended)` triple into our normalized
/// `KeyCode`, handling the left/right disambiguation `vk_to_keycode` alone
/// can't do — see the module docs on `keycodes.rs`. Takes primitives
/// rather than a specific OS struct so both `handle_raw_input`'s
/// `RAWKEYBOARD` and (for reference) `WH_KEYBOARD_LL`'s `KBDLLHOOKSTRUCT`
/// shape can feed it identically; only `handle_raw_input` calls it now.
fn resolve_keycode_raw(vk: u16, scan_code: u16, extended: bool) -> KeyCode {
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
        return if scan_code == 0x36 {
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

/// `WH_KEYBOARD_LL` no longer does key detection — see
/// `register_raw_keyboard`'s docs for why (a real two-machine test showed
/// this hook reliably seeing modifier keys but never a single regular
/// letter, on both driving directions, across multiple sessions; Raw Input
/// is now the actual source `handle_raw_input` forwards from). What a hook
/// can do that Raw Input can't is BLOCK local delivery, so this stays
/// installed purely for the `SUPPRESS` gate below, plus a liveness stamp
/// for the watchdog and a diagnostic proving whether this specific key
/// reached the hook chain at all.
///
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
        // Diagnostic only now: confirms whether the LL hook chain saw this
        // key at all, independent of Raw Input's own (separate) delivery.
        let msg = u32::try_from(wparam.0).unwrap_or(u32::MAX);
        tracing::debug!(vk = info.vkCode, msg, "keyboard_proc fired");
    }

    if ncode == HC_ACTION.cast_signed() && SUPPRESS.load(Ordering::SeqCst) {
        return LRESULT(1);
    }
    // SAFETY: same reasoning as the equivalent call in `mouse_proc`.
    unsafe { CallNextHookEx(None, ncode, wparam, lparam) }
}
