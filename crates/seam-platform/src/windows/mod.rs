//! Windows platform implementation (`SetWindowsHookEx`/`SendInput` via the
//! `windows` crate).
//!
//! `Capture`/`Sink`/`Screens` (M1) are implemented; `ClipboardProvider` and
//! `PermissionGate` (a no-op on Windows) land with M7 and the cross-platform
//! `current_platform()` factory (Tier 5.3). See Tier 13 of
//! `documentation/kvm-app-build-guide.md`.

mod capture;
mod inject;
mod keycodes;
mod screens;

pub use capture::Capture;
pub use inject::Sink;
pub use screens::Screens;
