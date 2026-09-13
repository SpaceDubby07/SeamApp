//! Cross-platform factory: the one place `#[cfg]` selects which OS's
//! `Clipboard` gets used, so callers like `seam-app` never need a `#[cfg]`
//! of their own.

use seam_core::traits::ClipboardProvider;

/// The concrete platform implementation for the OS this was compiled on,
/// as the trait object [`seam_core::session::Session::new`] takes.
pub struct Platform {
    /// Clipboard watch/set.
    pub clipboard: Box<dyn ClipboardProvider>,
}

/// Builds the platform implementation for whichever OS this was compiled
/// on.
#[must_use]
pub fn current_platform() -> Platform {
    #[cfg(target_os = "macos")]
    {
        Platform {
            clipboard: Box::new(crate::macos::Clipboard::new()),
        }
    }
    #[cfg(windows)]
    {
        Platform {
            clipboard: Box::new(crate::windows::Clipboard::new()),
        }
    }
}
