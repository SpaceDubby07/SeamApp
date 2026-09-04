//! macOS Accessibility permission gate (Tier 11.1 of the build guide).
//!
//! Without Accessibility permission, `CGEventTapCreate` returns null and
//! macOS gives NO error and NO prompt — the app just silently does
//! nothing. This is what lets the app detect that state and guide the
//! user, rather than looking broken.

use seam_core::error::PlatformError;
use seam_core::traits::PermissionGate;

use super::cg_ffi::AXIsProcessTrusted;

/// macOS implementation of [`seam_core::traits::PermissionGate`].
pub struct Permissions;

impl Permissions {
    /// Creates a permission gate. Nothing to set up ahead of time — every
    /// query re-checks live.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for Permissions {
    fn default() -> Self {
        Self::new()
    }
}

impl PermissionGate for Permissions {
    fn has_input_permission(&self) -> bool {
        // SAFETY: `AXIsProcessTrusted` takes no arguments and has no
        // preconditions.
        unsafe { AXIsProcessTrusted() }
    }

    fn request_input_permission(&self) -> Result<(), PlatformError> {
        // Opens the Accessibility settings pane directly (Tier 11.1 step
        // 4). Skips `AXIsProcessTrustedWithOptions`'
        // `kAXTrustedCheckOptionPrompt` system-prompt trigger (step 3) —
        // that needs a CFDictionary options bag, more FFI surface than is
        // worth the risk for a first pass when opening the pane gets the
        // user to the same place. A guided in-app prompt is a reasonable
        // enhancement once real permission UI (M11) exists to host it.
        //
        // Also worth knowing: on modern macOS, some event tap
        // configurations additionally require Input Monitoring
        // (Tier 11.2), a separate TCC service with no
        // `AXIsProcessTrusted`-equivalent query — it surfaces only as
        // `CGEventTapCreate` failing even with Accessibility granted. If
        // capture still won't start after granting Accessibility here,
        // check:
        //   x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent
        std::process::Command::new("open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
            .status()
            .map(|_| ())
            .map_err(|e| PlatformError::Other(e.to_string()))
    }
}
