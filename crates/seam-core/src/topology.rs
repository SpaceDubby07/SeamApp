//! Screen layout: displays, bounds, and (starting M2) edge-crossing math.
//!
//! Only the data types needed by the `ScreenInfo` trait land here for M1.
//! `compute_entry_point` and the rest of the edge-detection logic (Tier 7.2)
//! land in M2 alongside the handoff state machine that consumes them.

use serde::{Deserialize, Serialize};

/// A point in local virtual-desktop pixel coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Point {
    /// X coordinate in pixels.
    pub x: i32,
    /// Y coordinate in pixels.
    pub y: i32,
}

/// An axis-aligned rectangle in local virtual-desktop pixel coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    /// X coordinate of the top-left corner.
    pub x: i32,
    /// Y coordinate of the top-left corner.
    pub y: i32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// Stable identifier for one physical display, scoped to the local machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DisplayId(pub u32);

/// One physical display attached to the local machine.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Display {
    /// Identifier for this display, stable for the current session.
    pub id: DisplayId,
    /// Bounds of this display within the local virtual desktop.
    pub bounds: Rect,
    /// DPI scale factor (e.g. `2.0` on a Retina display).
    pub scale_factor: f64,
    /// Whether this is the OS-designated primary display.
    pub is_primary: bool,
}
