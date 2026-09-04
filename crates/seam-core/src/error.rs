//! Error types shared by the platform trait boundary.
//!
//! `seam-platform` implementations convert their OS-specific errors
//! (`windows::core::Error`, `objc2` failures, etc.) into this shared,
//! matchable type before returning across the trait boundary — `seam-core`
//! never depends on OS-specific error types directly.

/// An error from a platform trait implementation.
#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    /// The OS refused to register (or later revoked) an input hook or event
    /// tap. On Windows this usually means the callback was too slow; on
    /// macOS it usually means missing Accessibility/Input Monitoring
    /// permission.
    #[error("failed to register OS input hook: {0}")]
    HookRegistrationFailed(String),

    /// A synthetic input event was rejected by the OS injection API.
    #[error("input injection was rejected by the OS")]
    InjectionRejected,

    /// An OS-level permission (Accessibility, Input Monitoring) is missing.
    #[error("permission denied: {0}")]
    PermissionDenied(String),

    /// Any other platform failure that doesn't fit a more specific variant.
    #[error("platform operation failed: {0}")]
    Other(String),
}
