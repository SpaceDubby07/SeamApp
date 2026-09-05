//! Windows platform implementation (`SetWindowsHookEx`/`SendInput` via the
//! `windows` crate).
//!
//! `Capture`/`Sink`/`Screens` (M1) and `ClipboardProvider` (M7) are
//! implemented; `PermissionGate` (a no-op on Windows) and the
//! cross-platform `current_platform()` factory (Tier 5.3) are still
//! pending. See Tier 13 of `documentation/kvm-app-build-guide.md`.

mod capture;
mod clipboard;
mod inject;
mod keycodes;
mod screens;

pub use capture::Capture;
pub use clipboard::Clipboard;
pub use inject::Sink;
pub use screens::Screens;
