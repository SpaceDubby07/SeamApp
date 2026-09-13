//! Error types shared by the platform trait boundary.
//!
//! `seam-platform` implementations convert their OS-specific errors
//! (`windows::core::Error`, `objc2` failures, etc.) into this shared,
//! matchable type before returning across the trait boundary — `seam-core`
//! never depends on OS-specific error types directly.

/// An error from a platform trait implementation.
#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    /// The OS refused to register (or later revoked) a clipboard watcher.
    #[error("failed to register OS clipboard watcher: {0}")]
    HookRegistrationFailed(String),

    /// Any other platform failure that doesn't fit a more specific variant.
    #[error("platform operation failed: {0}")]
    Other(String),
}
