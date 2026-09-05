//! Windows clipboard watching/setting.
//!
//! `AddClipboardFormatListener` + `WM_CLIPBOARDUPDATE` on a dedicated
//! message-only window, mirroring `capture.rs`'s dedicated-pump-thread
//! pattern: event-driven, no polling. A message-only window is required
//! here because `AddClipboardFormatListener` attaches to an `HWND` and
//! Windows delivers `WM_CLIPBOARDUPDATE` to that window's procedure — there
//! is no listener API that isn't backed by a window.
//!
//! Deliberately does NOT try to detect or suppress this process's own
//! clipboard writes as an "echo" — `WM_CLIPBOARDUPDATE` fires for every
//! change regardless of who made it, and every one of those changes is
//! reported here unfiltered. Echo-loop prevention (recognizing "this is our
//! own write bouncing back") is handled once, platform-independently, at
//! the session layer (`seam_core::session::Session`).

use std::sync::OnceLock;
use std::sync::mpsc as std_mpsc;
use std::thread::JoinHandle;

use tokio::sync::mpsc::UnboundedSender;
use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
    IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW,
    RemoveClipboardFormatListener, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    HWND_MESSAGE, MSG, RegisterClassExW, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_CLIPBOARDUPDATE, WNDCLASSEXW,
};
use windows::core::{PCWSTR, w};

use seam_core::error::PlatformError;
use seam_core::protocol::ClipboardEvent;
use seam_core::traits::ClipboardProvider;

thread_local! {
    // The window procedure runs on the thread that created the
    // message-only window (Windows dispatches its messages synchronously
    // on that thread), so this only needs to be visible there.
    static SINK: std::cell::RefCell<Option<UnboundedSender<ClipboardEvent>>> =
        const { std::cell::RefCell::new(None) };
}

const WINDOW_CLASS_NAME: PCWSTR = w!("SeamClipboardWatcher");

/// Windows implementation of [`seam_core::traits::ClipboardProvider`].
pub struct Clipboard {
    thread: Option<JoinHandle<()>>,
    thread_id: Option<u32>,
}

impl Clipboard {
    /// Creates an inactive clipboard watcher. Call `watch` to actually
    /// start it.
    #[must_use]
    pub fn new() -> Self {
        Self {
            thread: None,
            thread_id: None,
        }
    }
}

impl Default for Clipboard {
    fn default() -> Self {
        Self::new()
    }
}

impl ClipboardProvider for Clipboard {
    fn watch(&mut self, sink: UnboundedSender<ClipboardEvent>) -> Result<(), PlatformError> {
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<u32, String>>();

        let handle = std::thread::Builder::new()
            .name("seam-clipboard-watch".into())
            .spawn(move || {
                SINK.with(|cell| *cell.borrow_mut() = Some(sink));

                let hwnd = match create_message_window() {
                    Ok(hwnd) => hwnd,
                    Err(reason) => {
                        let _ = ready_tx.send(Err(reason));
                        SINK.with(|cell| *cell.borrow_mut() = None);
                        return;
                    }
                };

                // SAFETY: `hwnd` was just created above and is valid for
                // the duration of this call.
                if let Err(e) = unsafe { AddClipboardFormatListener(hwnd) } {
                    // SAFETY: `hwnd` is ours, freshly created, with no
                    // listener attached yet.
                    unsafe {
                        let _ = DestroyWindow(hwnd);
                    }
                    let _ = ready_tx.send(Err(format!("AddClipboardFormatListener failed: {e}")));
                    SINK.with(|cell| *cell.borrow_mut() = None);
                    return;
                }

                // SAFETY: `GetCurrentThreadId` has no preconditions.
                let thread_id = unsafe { GetCurrentThreadId() };
                let _ = ready_tx.send(Ok(thread_id));

                // Fire the "current content" baseline event the
                // `ClipboardProvider::watch` contract promises, now that
                // the listener is armed (Tier 7.4's on-connect sync).
                report_current_clipboard();

                // Message pump. WM_CLIPBOARDUPDATE is only delivered while
                // this thread is pumping messages for `hwnd` — this loop IS
                // the watch, not just bookkeeping. `GetMessageW` blocks
                // until a message (including our own WM_QUIT, if this
                // thread is ever asked to stop) arrives.
                let mut msg = MSG::default();
                // SAFETY: `msg` is a valid, exclusively-owned MSG the OS
                // fills in; `None, 0, 0` means "any message for this
                // thread", matching `capture.rs`'s pump loop.
                while unsafe { GetMessageW(&raw mut msg, None, 0, 0) }.as_bool() {
                    // SAFETY: `msg` was just populated by GetMessageW above.
                    unsafe {
                        let _ = TranslateMessage(&raw const msg);
                        DispatchMessageW(&raw const msg);
                    }
                }

                // SAFETY: `hwnd` is still valid and the listener was
                // successfully added above.
                unsafe {
                    let _ = RemoveClipboardFormatListener(hwnd);
                    let _ = DestroyWindow(hwnd);
                }
                SINK.with(|cell| *cell.borrow_mut() = None);
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
                    "clipboard watch thread exited before signaling readiness".to_string(),
                ))
            }
        }
    }

    fn set_text(&mut self, text: &str) -> Result<(), PlatformError> {
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        wide.push(0);
        // SAFETY: reinterpreting a `Vec<u16>` we just built as its
        // constituent bytes for the length-prefixed write below — valid
        // for `wide.len() * 2` bytes since `u16` has no padding.
        let bytes =
            unsafe { std::slice::from_raw_parts(wide.as_ptr().cast::<u8>(), wide.len() * 2) };
        write_clipboard(u32::from(CF_UNICODETEXT.0), bytes)
    }

    fn set_image(&mut self, png_bytes: &[u8]) -> Result<(), PlatformError> {
        write_clipboard(png_clipboard_format(), png_bytes)
    }
}

/// Creates a hidden, message-only window (`HWND_MESSAGE` parent) purely to
/// receive `WM_CLIPBOARDUPDATE` — it's never shown and has no visible
/// content.
fn create_message_window() -> Result<HWND, String> {
    // SAFETY: `GetModuleHandleW(None)` returns a handle to this process's
    // own module, valid for registering a window class against.
    let hinstance =
        unsafe { GetModuleHandleW(None) }.map_err(|e| format!("GetModuleHandleW failed: {e}"))?;

    let wc = WNDCLASSEXW {
        cbSize: u32::try_from(size_of::<WNDCLASSEXW>()).unwrap_or_default(),
        lpfnWndProc: Some(wndproc),
        hInstance: hinstance.into(),
        lpszClassName: WINDOW_CLASS_NAME,
        ..Default::default()
    };
    // SAFETY: `wc` is a fully initialized `WNDCLASSEXW`; registering a
    // window class this way is always sound, and a duplicate-class error
    // (e.g. a second `Clipboard` in the same process) is reported through
    // the return value rather than being unsound.
    if unsafe { RegisterClassExW(&raw const wc) } == 0 {
        return Err("RegisterClassExW failed".to_string());
    }

    // SAFETY: creating a message-only window with the class just
    // registered above; `HWND_MESSAGE` as the parent and no window style
    // is the documented combination for a window that never becomes
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

/// # Safety
/// Called by the OS per the standard `WNDPROC` contract for the window
/// class registered in `create_message_window`.
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_CLIPBOARDUPDATE {
        report_current_clipboard();
        return LRESULT(0);
    }
    // SAFETY: forwarding an unhandled message to the default window
    // procedure with the exact parameters we were given is always sound.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Reads whatever the clipboard currently holds and forwards it through
/// the thread-local `SINK`, if it's a format we understand and a watcher
/// is currently listening. Silently does nothing otherwise (empty
/// clipboard, unsupported format, or a momentarily-locked clipboard).
fn report_current_clipboard() {
    let Some(event) = read_clipboard_event() else {
        return;
    };
    SINK.with(|cell| {
        if let Some(sink) = cell.borrow().as_ref() {
            let _ = sink.send(event);
        }
    });
}

/// Opens the clipboard, checks for a format we understand (PNG image
/// first, then Unicode text), and reads it into a [`ClipboardEvent`].
fn read_clipboard_event() -> Option<ClipboardEvent> {
    // SAFETY: `OpenClipboard(None)` opens the clipboard on behalf of this
    // thread with no associated window, which is valid for read-only
    // access — we only ever read here, never mutate.
    if unsafe { OpenClipboard(None) }.is_err() {
        tracing::debug!("OpenClipboard failed while reading current clipboard content");
        return None;
    }

    let png_format = png_clipboard_format();
    // SAFETY: the clipboard is open per the check above.
    let result = if unsafe { IsClipboardFormatAvailable(png_format) }.is_ok() {
        read_global_bytes(png_format).map(|data| ClipboardEvent::Image {
            mime: "image/png".to_string(),
            data,
        })
    // SAFETY: the clipboard is open per the check above.
    } else if unsafe { IsClipboardFormatAvailable(u32::from(CF_UNICODETEXT.0)) }.is_ok() {
        read_clipboard_text().map(ClipboardEvent::Text)
    } else {
        None
    };

    // SAFETY: matches the successful `OpenClipboard` above.
    let _ = unsafe { CloseClipboard() };
    result
}

/// Reads the raw bytes behind clipboard format `format` (the caller must
/// already have the clipboard open).
fn read_global_bytes(format: u32) -> Option<Vec<u8>> {
    // SAFETY: caller's invariant is that the clipboard is open. The
    // returned handle is owned by the clipboard, not by us — we only lock
    // it for reading, never free it.
    let handle = unsafe { GetClipboardData(format) }.ok()?;
    let hglobal = HGLOBAL(handle.0);
    // SAFETY: `hglobal` came from the successful `GetClipboardData` call
    // above.
    let size = unsafe { GlobalSize(hglobal) };
    if size == 0 {
        return None;
    }
    // SAFETY: same handle, valid for the duration of this lock.
    let ptr = unsafe { GlobalLock(hglobal) };
    if ptr.is_null() {
        return None;
    }
    // SAFETY: `ptr` is valid for `size` bytes per `GlobalLock`'s contract,
    // and we hold the lock for the duration of this read.
    let bytes = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), size) }.to_vec();
    // SAFETY: unlocking the handle we just locked above.
    let _ = unsafe { GlobalUnlock(hglobal) };
    Some(bytes)
}

/// Reads `CF_UNICODETEXT` (a null-terminated UTF-16LE buffer) as a `String`
/// (the caller must already have the clipboard open).
fn read_clipboard_text() -> Option<String> {
    // SAFETY: caller's invariant is that the clipboard is open; same
    // ownership contract as `read_global_bytes`.
    let handle = unsafe { GetClipboardData(u32::from(CF_UNICODETEXT.0)) }.ok()?;
    let hglobal = HGLOBAL(handle.0);
    // SAFETY: `hglobal` came from the successful `GetClipboardData` call
    // above.
    let ptr = unsafe { GlobalLock(hglobal) };
    if ptr.is_null() {
        return None;
    }
    // SAFETY: `CF_UNICODETEXT` data is documented to be a null-terminated
    // UTF-16LE string; `PCWSTR::to_string` reads up to the first null
    // terminator, and `ptr` is valid at least that far per the format's
    // own contract.
    let text = unsafe { PCWSTR(ptr.cast()).to_string() }.ok();
    // SAFETY: unlocking the handle we just locked above.
    let _ = unsafe { GlobalUnlock(hglobal) };
    text
}

/// Writes `bytes` to the clipboard under `format`, replacing any existing
/// content.
fn write_clipboard(format: u32, bytes: &[u8]) -> Result<(), PlatformError> {
    // SAFETY: `OpenClipboard(None)` — we own no window, which is valid for
    // a background writer.
    if unsafe { OpenClipboard(None) }.is_err() {
        return Err(PlatformError::Other(
            "OpenClipboard failed while writing".to_string(),
        ));
    }

    let result = (|| {
        // SAFETY: the clipboard is open per the check above; emptying it
        // is required before `SetClipboardData` per the Win32 contract.
        unsafe { EmptyClipboard() }.map_err(|e| PlatformError::Other(e.to_string()))?;
        let hglobal = alloc_global(bytes)?;
        // SAFETY: `hglobal` was just allocated and filled by `alloc_global`
        // and the clipboard is open. On success, ownership of `hglobal`
        // transfers to the clipboard — we must NOT free it ourselves,
        // which is why there's no corresponding `GlobalFree` here.
        unsafe { SetClipboardData(format, Some(HANDLE(hglobal.0))) }
            .map_err(|e| PlatformError::Other(e.to_string()))?;
        Ok(())
    })();

    // SAFETY: matches the successful `OpenClipboard` above.
    let _ = unsafe { CloseClipboard() };
    result
}

/// Allocates a movable global block sized for `bytes` and copies them in.
/// `GMEM_MOVEABLE` is `SetClipboardData`'s documented required allocation
/// flag.
fn alloc_global(bytes: &[u8]) -> Result<HGLOBAL, PlatformError> {
    // SAFETY: `GMEM_MOVEABLE` per `SetClipboardData`'s documented
    // requirement; the size requested is exactly the payload we're about
    // to copy in below.
    let hglobal = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len()) }
        .map_err(|e| PlatformError::Other(e.to_string()))?;
    // SAFETY: `hglobal` was just allocated with `bytes.len()` bytes above.
    let ptr = unsafe { GlobalLock(hglobal) };
    if ptr.is_null() {
        return Err(PlatformError::Other("GlobalLock failed".to_string()));
    }
    // SAFETY: `ptr` is valid for `bytes.len()` bytes per the allocation
    // above, and we hold the only lock on it, so a non-overlapping copy of
    // exactly that many bytes is sound.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.cast::<u8>(), bytes.len());
    }
    // SAFETY: unlocking the handle we just locked above.
    let _ = unsafe { GlobalUnlock(hglobal) };
    Ok(hglobal)
}

/// The registered "PNG" clipboard format id, used for both reading and
/// writing images. Cached after first registration rather than
/// re-registering on every clipboard access — `RegisterClipboardFormatW`
/// is idempotent (it returns the same id for an already-registered name),
/// but there's no reason to pay the call cost every time.
fn png_clipboard_format() -> u32 {
    static FORMAT: OnceLock<u32> = OnceLock::new();
    *FORMAT.get_or_init(|| {
        // SAFETY: `w!("PNG")` is a null-terminated wide string literal
        // valid for the duration of this call.
        unsafe { RegisterClipboardFormatW(w!("PNG")) }
    })
}
