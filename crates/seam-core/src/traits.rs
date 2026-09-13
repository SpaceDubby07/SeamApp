//! Platform abstraction boundary.
//!
//! The one OS-specific capability the app needs — reading and writing the
//! system clipboard — is expressed here as a trait. `seam-core` depends
//! only on this trait and never on a concrete OS API. `seam-platform`
//! provides one implementation per supported OS.
//!
//! This is what makes the core testable: tests substitute a mock
//! implementation and exercise clipboard sync with no real OS clipboard
//! involved.

use tokio::sync::mpsc::UnboundedSender;

use crate::error::PlatformError;
use crate::protocol::ClipboardEvent;

/// Watches and sets the system clipboard.
pub trait ClipboardProvider: Send + 'static {
    /// Starts watching. Implementations MUST emit one event immediately
    /// with whatever the clipboard currently holds, if it's text or an
    /// image (nothing is emitted if the clipboard is empty or holds
    /// content this trait doesn't model, e.g. files) — this is what seeds
    /// the on-connect clipboard sync without needing a separate "read
    /// current content" method. Every local change thereafter emits
    /// another event the same way.
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

#[cfg(test)]
mod tests {
    //! Proves the trait boundary is actually usable without any real OS API:
    //! a mock `ClipboardProvider` feeds synthetic events through the same
    //! channel a real watcher would use.

    use super::ClipboardProvider;
    use crate::error::PlatformError;
    use crate::protocol::ClipboardEvent;
    use tokio::sync::mpsc::UnboundedSender;

    /// Mirrors what a real implementation must do: `watch` fires once
    /// immediately with whatever "current content" it's holding, seeding
    /// the on-connect sync described in the trait docs.
    struct MockClipboard {
        current: Option<ClipboardEvent>,
    }

    impl ClipboardProvider for MockClipboard {
        fn watch(&mut self, sink: UnboundedSender<ClipboardEvent>) -> Result<(), PlatformError> {
            if let Some(event) = self.current.clone() {
                sink.send(event).expect("receiver still open");
            }
            Ok(())
        }

        fn set_text(&mut self, _text: &str) -> Result<(), PlatformError> {
            Ok(())
        }

        fn set_image(&mut self, _png_bytes: &[u8]) -> Result<(), PlatformError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn clipboard_provider_watch_emits_current_content_immediately() {
        let mut clipboard = MockClipboard {
            current: Some(ClipboardEvent::Text("hello".to_string())),
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        clipboard.watch(tx).expect("mock never fails");

        assert_eq!(
            rx.recv().await,
            Some(ClipboardEvent::Text("hello".to_string()))
        );
    }

    #[test]
    fn clipboard_provider_mock_constructs() {
        let mut clipboard = MockClipboard { current: None };
        clipboard.set_text("hello").expect("mock never fails");
    }
}
