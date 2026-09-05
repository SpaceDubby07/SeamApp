//! Networking: the control channel (M3), the bulk channel (M7, carrying
//! clipboard images now and file chunks from M10 on), and — landing in
//! later milestones — mDNS discovery (M9), pairing (M8), and TLS (M8). See
//! Tier 5/6 of the build guide.

pub mod bulk;
pub mod control;
