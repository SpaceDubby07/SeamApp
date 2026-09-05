//! macOS platform implementation (`CGEventTap`/`CGEventPost` via direct
//! `extern "C"` FFI — see `cg_ffi.rs` for why not `objc2-core-graphics`).
//!
//! `Capture`/`Sink`/`Screens`/`Permissions` are implemented as of M5.
//! `ClipboardProvider` is implemented as of M7, via `NSPasteboard` through
//! the `objc2`/`objc2-app-kit` crates — see `clipboard.rs` for why that
//! module doesn't follow the raw-FFI approach the rest of this backend
//! uses. See Tier 13 of `documentation/kvm-app-build-guide.md`.

mod capture;
mod cg_ffi;
mod clipboard;
mod inject;
mod keycodes;
mod permissions;
mod screens;

pub use capture::Capture;
pub use clipboard::Clipboard;
pub use inject::Sink;
pub use permissions::Permissions;
pub use screens::Screens;
