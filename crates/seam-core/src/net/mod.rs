//! Networking: the control channel (M3), the bulk channel (M7, carrying
//! clipboard images now and file chunks from M10 on), TLS identity and
//! fingerprint pinning (M8), pairing-code derivation (M8), and mDNS
//! discovery (M9). See Tier 5/6 of the build guide.

pub mod bulk;
pub mod control;
pub mod discovery;
pub mod pairing;
pub mod tls;
