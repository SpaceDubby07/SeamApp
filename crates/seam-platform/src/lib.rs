//! Platform implementations, selected at compile time.
//!
//! `seam-core` depends only on the traits it defines and never on a concrete
//! OS API. This crate provides one implementation per supported OS; which
//! concrete types get used is decided by `cfg` here and nowhere else, so
//! `seam-app` never needs to write a `#[cfg]` of its own.
//!
//! Windows `InputCapture`/`InputSink`/`ScreenInfo` are implemented as of
//! M1; macOS stays an empty stub until M5. See
//! `documentation/kvm-app-build-guide.md`.

#![warn(missing_docs)]
#![warn(clippy::pedantic)]
#![warn(clippy::undocumented_unsafe_blocks)]

#[cfg(target_os = "macos")]
pub mod macos;
mod platform;
#[cfg(windows)]
pub mod windows;

pub use platform::{Platform, current_platform, has_input_permission, request_input_permission};
