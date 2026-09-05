//! Portable core logic for Seam: the handoff state machine, screen topology,
//! wire protocol, networking, transfer engine, modifier remapping, config
//! persistence, and the platform trait boundary.
//!
//! This crate must compile and pass its full test suite on *any* platform,
//! with zero `#[cfg]` attributes. Every OS-specific capability is expressed
//! as a trait in `traits` and implemented in `seam-platform`. If a
//! `#[cfg(windows)]` or `#[cfg(target_os = "macos")]` ever seems necessary
//! in this crate, the abstraction belongs behind a trait instead.
//!
//! `session.rs` wires the handoff state machine (`state.rs`), the control
//! channel (`net`), and the platform traits (`traits.rs`) together into a
//! runnable session — the M4 milestone. See
//! `documentation/kvm-app-build-guide.md`.

#![warn(missing_docs)]
#![warn(clippy::pedantic)]

pub mod config;
pub mod error;
pub mod net;
pub mod protocol;
pub mod remap;
pub mod session;
pub mod state;
pub mod topology;
pub mod traits;
pub mod transfer;

pub use error::PlatformError;
