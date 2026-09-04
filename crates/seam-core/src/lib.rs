//! Portable core logic for Seam: the handoff state machine, screen topology,
//! wire protocol, networking, transfer engine, and the platform trait
//! boundary.
//!
//! This crate must compile and pass its full test suite on *any* platform,
//! with zero `#[cfg]` attributes. Every OS-specific capability is expressed
//! as a trait in `traits` and implemented in `seam-platform`. If a
//! `#[cfg(windows)]` or `#[cfg(target_os = "macos")]` ever seems necessary
//! in this crate, the abstraction belongs behind a trait instead.
//!
//! `state.rs` (the handoff state machine) and the rest of `topology.rs`'s
//! edge math land in M2; the full wire protocol lands in M3. See
//! `documentation/kvm-app-build-guide.md`.

#![warn(missing_docs)]
#![warn(clippy::pedantic)]

pub mod error;
pub mod protocol;
pub mod topology;
pub mod traits;

pub use error::PlatformError;
