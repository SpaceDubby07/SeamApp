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
//! The handoff state machine (`state.rs`), screen topology
//! (`topology.rs`), and the wire protocol + control channel (`protocol`,
//! `net`) are implemented as of M3. See
//! `documentation/kvm-app-build-guide.md`.

#![warn(missing_docs)]
#![warn(clippy::pedantic)]

pub mod error;
pub mod net;
pub mod protocol;
pub mod state;
pub mod topology;
pub mod traits;

pub use error::PlatformError;
