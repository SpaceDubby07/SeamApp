//! macOS clipboard watching/setting via `NSPasteboard` (Tier 7.4, M7).
//!
//! `NSPasteboard` has no change-notification API, so — per Tier 7.4 of the
//! build guide — this polls its `changeCount` (a cheap integer read) on a
//! dedicated background thread every [`POLL_INTERVAL`] and only fetches
//! actual content when that count has moved. This is unlike Windows'
//! `clipboard.rs`, which gets a real `WM_CLIPBOARDUPDATE` notification and
//! polls nothing.
//!
//! Deliberately does NOT try to distinguish our own `set_text`/`set_image`
//! writes from an externally-caused clipboard change — every observed
//! change is reported unconditionally, whoever caused it. Recognizing "this
//! is just our own write echoing back" is handled once, at the session
//! layer (`Session::is_echo_of_what_we_just_applied`), rather than
//! duplicated in every platform backend.

use std::thread::JoinHandle;
use std::time::Duration;

use objc2::rc::{Retained, autoreleasepool};
use objc2_app_kit::{NSPasteboard, NSPasteboardTypePNG, NSPasteboardTypeString};
use objc2_foundation::{NSData, NSInteger, NSString};
use tokio::sync::mpsc::UnboundedSender;

use seam_core::error::PlatformError;
use seam_core::protocol::ClipboardEvent;
use seam_core::traits::ClipboardProvider;

/// How often to poll `NSPasteboard`'s `changeCount` (Tier 7.4: "it's a
/// cheap integer read").
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// MIME type reported for image clipboard content — the only image
/// pasteboard type this module reads/writes.
const IMAGE_MIME: &str = "image/png";

/// macOS implementation of [`seam_core::traits::ClipboardProvider`].
pub struct Clipboard {
    /// The polling thread `watch` spawns. Never joined: `ClipboardProvider`
    /// has no `stop()` (unlike `InputCapture`), so this simply runs for the
    /// life of the process — kept here only so the handle isn't dropped
    /// (and detached) the moment `watch` returns.
    thread: Option<JoinHandle<()>>,
}

impl Clipboard {
    /// Creates an inactive clipboard provider. Call `watch` to start
    /// polling.
    #[must_use]
    pub fn new() -> Self {
        Self { thread: None }
    }
}

impl Default for Clipboard {
    fn default() -> Self {
        Self::new()
    }
}

impl ClipboardProvider for Clipboard {
    fn watch(&mut self, sink: UnboundedSender<ClipboardEvent>) -> Result<(), PlatformError> {
        let handle = std::thread::Builder::new()
            .name("seam-clipboard-watch".into())
            .spawn(move || {
                // SAFETY: `NSPasteboardTypePNG`/`NSPasteboardTypeString`
                // (read inside `read_current`, called from this thread
                // only) are static Objective-C constants provided by
                // AppKit — valid and immutable for the life of the
                // process, safe to read from any thread.
                let pasteboard = NSPasteboard::generalPasteboard();
                let mut last_seen_change_count: Option<NSInteger> = None;

                loop {
                    let change_count = pasteboard.changeCount();
                    if last_seen_change_count != Some(change_count) {
                        last_seen_change_count = Some(change_count);
                        // Wrapped per-iteration: `dataForType`/
                        // `stringForType` return autoreleased objects, and
                        // this thread never runs its own run loop to drain
                        // a pool on its own — without one, those temporary
                        // objects would leak for the life of the process
                        // instead of just this poll (the app is meant to
                        // run for days at a stretch — Tier 12.4).
                        let event = autoreleasepool(|_| read_current(&pasteboard));
                        if let Some(event) = event
                            && sink.send(event).is_err()
                        {
                            // The session dropped its receiver (shutting
                            // down) — nothing left to report to.
                            return;
                        }
                    }
                    std::thread::sleep(POLL_INTERVAL);
                }
            })
            .map_err(|e| PlatformError::HookRegistrationFailed(e.to_string()))?;
        self.thread = Some(handle);
        Ok(())
    }

    fn set_text(&mut self, text: &str) -> Result<(), PlatformError> {
        let pasteboard = NSPasteboard::generalPasteboard();
        pasteboard.clearContents();
        let ns_string = NSString::from_str(text);
        // SAFETY: `NSPasteboardTypeString` is a static Objective-C constant
        // provided by AppKit, valid for the life of the process.
        let pasteboard_type = unsafe { NSPasteboardTypeString };
        if pasteboard.setString_forType(&ns_string, pasteboard_type) {
            Ok(())
        } else {
            Err(PlatformError::Other(
                "NSPasteboard rejected the text write".to_string(),
            ))
        }
    }

    fn set_image(&mut self, png_bytes: &[u8]) -> Result<(), PlatformError> {
        let pasteboard = NSPasteboard::generalPasteboard();
        pasteboard.clearContents();
        let data = NSData::with_bytes(png_bytes);
        // SAFETY: `NSPasteboardTypePNG` is a static Objective-C constant
        // provided by AppKit, valid for the life of the process.
        let pasteboard_type = unsafe { NSPasteboardTypePNG };
        if pasteboard.setData_forType(Some(&data), pasteboard_type) {
            Ok(())
        } else {
            Err(PlatformError::Other(
                "NSPasteboard rejected the image write".to_string(),
            ))
        }
    }
}

/// Reads whatever the pasteboard currently holds, preferring a PNG image
/// representation over plain text, and reporting nothing for content this
/// trait doesn't model (files, RTF, etc.) — matching `ClipboardProvider`'s
/// documented contract.
fn read_current(pasteboard: &Retained<NSPasteboard>) -> Option<ClipboardEvent> {
    // SAFETY: see the SAFETY comment on this same static in `watch`.
    let png_type = unsafe { NSPasteboardTypePNG };
    if let Some(data) = pasteboard.dataForType(png_type) {
        return Some(ClipboardEvent::Image {
            mime: IMAGE_MIME.to_string(),
            data: data.to_vec(),
        });
    }

    // SAFETY: see the SAFETY comment on this same static in `watch`.
    let string_type = unsafe { NSPasteboardTypeString };
    if let Some(text) = pasteboard.stringForType(string_type) {
        return Some(ClipboardEvent::Text(text.to_string()));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::Clipboard;
    use objc2_app_kit::NSPasteboard;
    use seam_core::traits::ClipboardProvider;

    /// Round-trips through the REAL system pasteboard (there's no
    /// sandboxed stand-in for `NSPasteboard::generalPasteboard()`) —
    /// running this test clobbers whatever's currently on the clipboard.
    /// `watch`'s poll loop is timing-based and not worth driving from a
    /// unit test; this only exercises `set_text`/`set_image` directly
    /// against the pasteboard `read_current` also reads from.
    #[test]
    fn set_text_then_set_image_round_trip_through_the_real_pasteboard() {
        let mut clipboard = Clipboard::new();

        clipboard
            .set_text("seam clipboard smoke test")
            .expect("set_text");
        // SAFETY: `NSPasteboardTypeString` is a static Objective-C constant
        // provided by AppKit, valid for the life of the process.
        let string_type = unsafe { objc2_app_kit::NSPasteboardTypeString };
        let pasteboard = NSPasteboard::generalPasteboard();
        assert_eq!(
            pasteboard.stringForType(string_type).map(|s| s.to_string()),
            Some("seam clipboard smoke test".to_string())
        );

        let png_bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        clipboard.set_image(&png_bytes).expect("set_image");
        // SAFETY: `NSPasteboardTypePNG` is a static Objective-C constant
        // provided by AppKit, valid for the life of the process.
        let png_type = unsafe { objc2_app_kit::NSPasteboardTypePNG };
        assert_eq!(
            pasteboard.dataForType(png_type).map(|d| d.to_vec()),
            Some(png_bytes)
        );
    }
}
