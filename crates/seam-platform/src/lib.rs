//! Platform implementations, selected at compile time.
//!
//! `seam-core` depends only on the one trait it defines
//! (`ClipboardProvider`) and never on a concrete OS API. This crate
//! provides one implementation per supported OS; which concrete type gets
//! used is decided by `cfg` here and nowhere else, so `seam-app` never
//! needs to write a `#[cfg]` of its own.

#![warn(missing_docs)]
#![warn(clippy::pedantic)]
#![warn(clippy::undocumented_unsafe_blocks)]

#[cfg(target_os = "macos")]
pub mod macos;
mod platform;
#[cfg(windows)]
pub mod windows;

pub use platform::{Platform, current_platform};
