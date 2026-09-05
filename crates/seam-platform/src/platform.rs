//! Cross-platform factory (Tier 5.3): the one place `#[cfg]` selects which
//! OS's `Capture`/`Sink`/`Clipboard`/`Screens` gets used, so callers like
//! `seam-app` never need a `#[cfg]` of their own.

use seam_core::error::PlatformError;
use seam_core::traits::{ClipboardProvider, InputCapture, InputSink, ScreenInfo};

/// The concrete platform implementations for the OS this was compiled on,
/// as the trait objects [`seam_core::session::Session::new`] takes.
pub struct Platform {
    /// Global input capture.
    pub capture: Box<dyn InputCapture>,
    /// Synthetic input injection.
    pub sink: Box<dyn InputSink>,
    /// Clipboard watch/set.
    pub clipboard: Box<dyn ClipboardProvider>,
    /// Display enumeration.
    pub screens: Box<dyn ScreenInfo>,
}

/// Builds the platform implementations for whichever OS this was compiled
/// on.
#[must_use]
pub fn current_platform() -> Platform {
    #[cfg(target_os = "macos")]
    {
        Platform {
            capture: Box::new(crate::macos::Capture::new()),
            sink: Box::new(crate::macos::Sink::new()),
            clipboard: Box::new(crate::macos::Clipboard::new()),
            screens: Box::new(crate::macos::Screens::new()),
        }
    }
    #[cfg(windows)]
    {
        Platform {
            capture: Box::new(crate::windows::Capture::new()),
            sink: Box::new(crate::windows::Sink::new()),
            clipboard: Box::new(crate::windows::Clipboard::new()),
            screens: Box::new(crate::windows::Screens::new()),
        }
    }
}

/// Whether this OS has granted whatever permission its input capture
/// needs. Always `true` on Windows — no such gate exists there (Tier
/// 5.3's `PermissionGate` is macOS-only for now); on macOS, reflects
/// Accessibility (Tier 11.1).
#[must_use]
pub fn has_input_permission() -> bool {
    #[cfg(target_os = "macos")]
    {
        use seam_core::traits::PermissionGate;
        crate::macos::Permissions::new().has_input_permission()
    }
    #[cfg(windows)]
    {
        true
    }
}

/// Requests whatever input-capture permission this OS needs — opens the
/// Accessibility pane in System Settings on macOS. A no-op on Windows.
///
/// # Errors
/// Returns an error if the OS-level request itself fails (not if the user
/// declines — there's no synchronous signal for that; re-check with
/// [`has_input_permission`]).
pub fn request_input_permission() -> Result<(), PlatformError> {
    #[cfg(target_os = "macos")]
    {
        use seam_core::traits::PermissionGate;
        crate::macos::Permissions::new().request_input_permission()
    }
    #[cfg(windows)]
    {
        Ok(())
    }
}
