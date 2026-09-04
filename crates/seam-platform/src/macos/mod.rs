//! macOS platform implementation (`CGEventTap`/`CGEventPost` via direct
//! `extern "C"` FFI — see `cg_ffi.rs` for why not `objc2-core-graphics`).
//!
//! `Capture`/`Sink`/`Screens`/`Permissions` are implemented as of M5.
//! `ClipboardProvider` lands with M7. See Tier 13 of
//! `documentation/kvm-app-build-guide.md`.

mod capture;
mod cg_ffi;
mod inject;
mod keycodes;
mod permissions;
mod screens;

pub use capture::Capture;
pub use inject::Sink;
pub use permissions::Permissions;
pub use screens::Screens;
