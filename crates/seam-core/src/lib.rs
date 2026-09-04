//! Portable core logic for Seam: the handoff state machine, screen topology,
//! wire protocol, networking, transfer engine, and the platform trait
//! boundary.
//!
//! This crate must compile and pass its full test suite on *any* platform,
//! with zero `#[cfg]` attributes. Every OS-specific capability is expressed
//! as a trait here (see `traits.rs`, landing in M1) and implemented in
//! `seam-platform`. If a `#[cfg(windows)]` or `#[cfg(target_os = "macos")]`
//! ever seems necessary in this crate, the abstraction belongs behind a
//! trait instead.
//!
//! Empty for now — modules land starting with M1 (`traits.rs`) and M2
//! (`state.rs`, `topology.rs`). See `documentation/kvm-app-build-guide.md`.

#![warn(missing_docs)]
#![warn(clippy::pedantic)]
