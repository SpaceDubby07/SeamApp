//! Platform abstraction boundary.
//!
//! Every OS-specific capability the app needs is expressed here as a trait.
//! `seam-core` depends only on these traits and never on a concrete OS API.
//! `seam-platform` provides one implementation per supported OS.
//!
//! This is what makes the core testable: tests substitute mock
//! implementations and exercise the full handoff state machine with no real
//! hardware involved.

use tokio::sync::mpsc::UnboundedSender;

use crate::error::PlatformError;
use crate::protocol::{ClipboardEvent, InputEvent};
use crate::topology::{Display, DisplayId};

/// Captures global input events, optionally suppressing them from reaching
/// the local OS.
///
/// Implementations run the OS hook on a dedicated thread and forward events
/// through the channel supplied to `start`. The callback path must never
/// block — on Windows a slow low-level hook is silently unregistered by the
/// OS after the timeout, and on macOS a slow event tap gets disabled.
pub trait InputCapture: Send + 'static {
    /// Begins capturing. Events are pushed to `sink` as they arrive.
    ///
    /// # Errors
    /// Returns an error if the OS refuses to register the hook — most
    /// commonly a missing macOS Accessibility/Input Monitoring permission.
    fn start(&mut self, sink: UnboundedSender<InputEvent>) -> Result<(), PlatformError>;

    /// Stops capturing and releases the OS hook.
    ///
    /// # Errors
    /// Returns an error if the OS hook could not be cleanly released.
    fn stop(&mut self) -> Result<(), PlatformError>;

    /// When `true`, captured events are consumed and do NOT reach local
    /// apps. This is what makes the local cursor "disappear" during
    /// handoff.
    ///
    /// # Errors
    /// Returns an error if the OS rejects the suppression change.
    fn set_suppression(&mut self, suppress: bool) -> Result<(), PlatformError>;

    /// True if the OS revoked our hook (macOS does this after sleep, and
    /// after an event tap times out). The supervisor polls this to trigger
    /// re-arm.
    #[must_use]
    fn is_healthy(&self) -> bool;
}

/// Injects synthetic input into the local OS.
pub trait InputSink: Send + 'static {
    /// Injects a single input event as if it came from local hardware.
    ///
    /// # Errors
    /// Returns an error if the OS rejects the synthetic event.
    fn inject(&mut self, event: &InputEvent) -> Result<(), PlatformError>;

    /// Warps the cursor to absolute screen coordinates. Used on handoff
    /// entry.
    ///
    /// # Errors
    /// Returns an error if the OS rejects the cursor warp.
    fn warp_cursor(&mut self, x: i32, y: i32) -> Result<(), PlatformError>;

    /// Forces all modifier keys to the released state. Called on handoff
    /// exit and on disconnect — this is the fix for stuck-modifier bugs.
    ///
    /// # Errors
    /// Returns an error if the OS rejects one of the synthetic key-up
    /// events.
    fn release_all_modifiers(&mut self) -> Result<(), PlatformError>;
}

/// Watches and sets the system clipboard.
pub trait ClipboardProvider: Send + 'static {
    /// Emits an event whenever local clipboard content changes.
    ///
    /// # Errors
    /// Returns an error if the OS clipboard watcher could not be started.
    fn watch(&mut self, sink: UnboundedSender<ClipboardEvent>) -> Result<(), PlatformError>;

    /// Sets the local clipboard to plain text.
    ///
    /// # Errors
    /// Returns an error if the OS rejects the write.
    fn set_text(&mut self, text: &str) -> Result<(), PlatformError>;

    /// Sets the local clipboard to a PNG image.
    ///
    /// # Errors
    /// Returns an error if the OS rejects the write.
    fn set_image(&mut self, png_bytes: &[u8]) -> Result<(), PlatformError>;
}

/// Reports the local display configuration.
pub trait ScreenInfo: Send + 'static {
    /// All displays attached to this machine, in OS virtual-desktop
    /// coordinates.
    #[must_use]
    fn displays(&self) -> Vec<Display>;

    /// The bounding box of the entire virtual desktop. Edge detection uses
    /// this.
    #[must_use]
    fn virtual_bounds(&self) -> crate::topology::Rect;

    /// DPI scale factor for a given display. Needed to make cursor position
    /// translate correctly between a Retina Mac and a 1080p Windows
    /// monitor.
    #[must_use]
    fn scale_factor(&self, display_id: DisplayId) -> f64;
}

/// OS-level permission gates (macOS Accessibility; no-op on Windows).
pub trait PermissionGate: Send + 'static {
    /// Whether the app currently has the OS permission it needs to capture
    /// and inject input.
    #[must_use]
    fn has_input_permission(&self) -> bool;

    /// Opens the relevant system settings pane. Returns immediately — the
    /// caller is expected to poll `has_input_permission` afterward.
    ///
    /// # Errors
    /// Returns an error if the settings pane could not be opened.
    fn request_input_permission(&self) -> Result<(), PlatformError>;
}

#[cfg(test)]
mod tests {
    //! Proves the trait boundary is actually usable without any real OS API:
    //! a mock `InputCapture` feeds synthetic events through the same
    //! channel a real hook would use, and a mock `InputSink` records what it
    //! was asked to inject. This is the M1 demo ("log every event, inject a
    //! synthetic click and see it land") with the "OS" swapped for a mock —
    //! the same pattern `seam-platform`'s real Windows/macOS
    //! implementations plug into.

    use super::{
        ClipboardEvent, ClipboardProvider, Display, DisplayId, InputCapture, InputEvent, InputSink,
        ScreenInfo,
    };
    use crate::error::PlatformError;
    use crate::protocol::{KeyCode, MouseButton};
    use tokio::sync::mpsc::UnboundedSender;

    struct MockCapture;

    impl InputCapture for MockCapture {
        fn start(&mut self, sink: UnboundedSender<InputEvent>) -> Result<(), PlatformError> {
            // A real hook would forward events as the OS delivers them; the
            // mock just delivers a fixed sequence immediately.
            sink.send(InputEvent::MouseMoveAbs { x: 100, y: 200 })
                .expect("receiver still open");
            sink.send(InputEvent::KeyDown {
                code: KeyCode::A,
                repeat: false,
            })
            .expect("receiver still open");
            Ok(())
        }

        fn stop(&mut self) -> Result<(), PlatformError> {
            Ok(())
        }

        fn set_suppression(&mut self, _suppress: bool) -> Result<(), PlatformError> {
            Ok(())
        }

        fn is_healthy(&self) -> bool {
            true
        }
    }

    #[derive(Default)]
    struct MockSink {
        injected: Vec<InputEvent>,
    }

    impl InputSink for MockSink {
        fn inject(&mut self, event: &InputEvent) -> Result<(), PlatformError> {
            self.injected.push(*event);
            Ok(())
        }

        fn warp_cursor(&mut self, _x: i32, _y: i32) -> Result<(), PlatformError> {
            Ok(())
        }

        fn release_all_modifiers(&mut self) -> Result<(), PlatformError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn captured_events_are_observable_through_the_channel() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut capture = MockCapture;

        capture.start(tx).expect("mock capture never fails");

        assert_eq!(
            rx.recv().await,
            Some(InputEvent::MouseMoveAbs { x: 100, y: 200 })
        );
        assert_eq!(
            rx.recv().await,
            Some(InputEvent::KeyDown {
                code: KeyCode::A,
                repeat: false
            })
        );
    }

    #[test]
    fn injected_click_lands_on_the_sink() {
        let mut sink = MockSink::default();

        sink.inject(&InputEvent::MouseDown {
            button: MouseButton::Left,
        })
        .expect("mock sink never fails");
        sink.inject(&InputEvent::MouseUp {
            button: MouseButton::Left,
        })
        .expect("mock sink never fails");

        assert_eq!(
            sink.injected,
            vec![
                InputEvent::MouseDown {
                    button: MouseButton::Left
                },
                InputEvent::MouseUp {
                    button: MouseButton::Left
                },
            ]
        );
    }

    /// Trivial mock proving `ScreenInfo`'s associated types are usable
    /// without any real OS call — a fixed single-display report.
    struct MockScreenInfo;

    impl super::ScreenInfo for MockScreenInfo {
        fn displays(&self) -> Vec<Display> {
            vec![Display {
                id: DisplayId(0),
                bounds: crate::topology::Rect {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                },
                scale_factor: 1.0,
                is_primary: true,
            }]
        }

        fn virtual_bounds(&self) -> crate::topology::Rect {
            self.displays()[0].bounds
        }

        fn scale_factor(&self, _display_id: DisplayId) -> f64 {
            1.0
        }
    }

    #[test]
    fn screen_info_reports_the_mock_display() {
        let screens = MockScreenInfo;
        assert_eq!(screens.displays().len(), 1);
        assert!(screens.displays()[0].is_primary);
    }

    /// Compile-time check only: a `ClipboardProvider` mock must be
    /// constructible against the trait as written.
    struct MockClipboard;

    impl super::ClipboardProvider for MockClipboard {
        fn watch(&mut self, _sink: UnboundedSender<ClipboardEvent>) -> Result<(), PlatformError> {
            Ok(())
        }

        fn set_text(&mut self, _text: &str) -> Result<(), PlatformError> {
            Ok(())
        }

        fn set_image(&mut self, _png_bytes: &[u8]) -> Result<(), PlatformError> {
            Ok(())
        }
    }

    #[test]
    fn clipboard_provider_mock_constructs() {
        let mut clipboard = MockClipboard;
        clipboard.set_text("hello").expect("mock never fails");
    }
}
