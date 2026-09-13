//! Portable core logic for Seam: node identity, wire protocol, networking
//! (discovery, pairing, TLS), the file-transfer engine, clipboard sync, and
//! config persistence.
//!
//! This crate must compile and pass its full test suite on *any* platform,
//! with zero `#[cfg]` attributes. The one OS-specific capability the app
//! needs — the system clipboard — is expressed as a trait in `traits` and
//! implemented in `seam-platform`.
//!
//! `session.rs` wires the control channel (`net`), the transfer engine
//! (`transfer`), and clipboard sync together into a runnable session.

#![warn(missing_docs)]
#![warn(clippy::pedantic)]

pub mod config;
pub mod error;
pub mod net;
pub mod protocol;
pub mod session;
pub mod topology;
pub mod traits;
pub mod transfer;

pub use error::PlatformError;
