//! Platform implementations, selected at compile time.
//!
//! `seam-core` depends only on the traits it defines and never on a concrete
//! OS API. This crate provides one implementation per supported OS; which
//! concrete types get used is decided by `cfg` here and nowhere else, so
//! `seam-app` never needs to write a `#[cfg]` of its own.
//!
//! Empty stubs for now — Windows capture/inject lands in M1, macOS in M5.
//! See `documentation/kvm-app-build-guide.md`.

#![warn(missing_docs)]
#![warn(clippy::pedantic)]
#![warn(clippy::undocumented_unsafe_blocks)]

#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;
