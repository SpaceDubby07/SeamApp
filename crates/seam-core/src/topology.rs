//! Node identity.
//!
//! A UUID rather than e.g. a hostname or an index, so it survives renames
//! and never collides across machines.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Stable identity for one node (machine), generated once on first run and
/// persisted in config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(pub Uuid);

impl NodeId {
    /// Generates a new, random node identity.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for NodeId {
    fn default() -> Self {
        Self::new()
    }
}
