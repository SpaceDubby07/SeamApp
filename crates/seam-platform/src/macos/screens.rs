//! macOS display enumeration via CoreGraphics.
//!
//! Uses `CGDisplayBounds` rather than `NSScreen` deliberately — see the
//! Cargo.toml comment on why: `NSScreen`'s coordinate space has its origin
//! at the bottom-left with Y increasing upward, while `CGEventTap`/
//! `CGWarpMouseCursorPosition` (and so `capture.rs`/`inject.rs`) use a
//! top-left origin with Y increasing downward. `CGDisplayBounds` already
//! reports bounds in that same Quartz space, sidestepping the mismatch.

use seam_core::topology::{Display, DisplayId, Rect, union_of_display_bounds};
use seam_core::traits::ScreenInfo;

use super::cg_ffi::{
    CGDirectDisplayID, CGDisplayBounds, CGDisplayPixelsWide, CGGetActiveDisplayList,
    CGMainDisplayID, CGRect,
};

const MAX_DISPLAYS: u32 = 16;

/// macOS implementation of [`seam_core::traits::ScreenInfo`].
pub struct Screens;

impl Screens {
    /// Creates a screen-info source. Nothing to set up ahead of time —
    /// each query re-enumerates live, so it can't go stale across a
    /// monitor hotplug.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for Screens {
    fn default() -> Self {
        Self::new()
    }
}

impl ScreenInfo for Screens {
    fn displays(&self) -> Vec<Display> {
        enumerate_displays()
    }

    fn virtual_bounds(&self) -> Rect {
        union_of_display_bounds(&enumerate_displays())
    }

    fn scale_factor(&self, display_id: DisplayId) -> f64 {
        enumerate_displays()
            .into_iter()
            .find(|d| d.id == display_id)
            .map_or(1.0, |d| d.scale_factor)
    }
}

fn enumerate_displays() -> Vec<Display> {
    let mut ids = [0u32; MAX_DISPLAYS as usize];
    let mut count: u32 = 0;
    // SAFETY: `ids` is a valid buffer of `MAX_DISPLAYS` elements and
    // `count` a valid out-param; both live for the duration of this call.
    let err = unsafe { CGGetActiveDisplayList(MAX_DISPLAYS, ids.as_mut_ptr(), &raw mut count) };
    if err != 0 {
        return Vec::new();
    }

    // SAFETY: `CGMainDisplayID` has no preconditions.
    let main_id = unsafe { CGMainDisplayID() };

    ids[..count as usize]
        .iter()
        .map(|&id| display_for(id, id == main_id))
        .collect()
}

fn display_for(id: CGDirectDisplayID, is_primary: bool) -> Display {
    // SAFETY: `id` came from `CGGetActiveDisplayList`, which only returns
    // currently-active display IDs.
    let CGRect { origin, size } = unsafe { CGDisplayBounds(id) };
    // SAFETY: same as above.
    let pixels_wide = unsafe { CGDisplayPixelsWide(id) };

    // `CGDisplayBounds` reports POINTS (logical units), not raw pixels —
    // dividing pixel width by point width recovers the backing scale
    // factor (2.0 on Retina) without an NSScreen round trip.
    #[allow(clippy::cast_precision_loss)]
    let scale_factor = if size.width > 0.0 {
        pixels_wide as f64 / size.width
    } else {
        1.0
    };

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Display {
        id: DisplayId(id),
        bounds: Rect {
            x: origin.x.round() as i32,
            y: origin.y.round() as i32,
            width: size.width.round() as u32,
            height: size.height.round() as u32,
        },
        scale_factor,
        is_primary,
    }
}
