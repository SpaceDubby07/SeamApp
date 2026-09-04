//! Windows display enumeration and per-monitor DPI.

use std::mem::size_of;

use seam_core::topology::{Display, DisplayId, Rect};
use seam_core::traits::ScreenInfo;

use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;
use windows::core::BOOL;

/// Windows implementation of [`seam_core::traits::ScreenInfo`].
pub struct Screens;

impl Screens {
    /// Creates a screen-info source. Nothing to set up ahead of time — each
    /// query re-enumerates live, so it can't go stale across a monitor
    /// hotplug.
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
        union_bounds(&enumerate_displays())
    }

    fn scale_factor(&self, display_id: DisplayId) -> f64 {
        enumerate_displays()
            .into_iter()
            .find(|d| d.id == display_id)
            .map_or(1.0, |d| d.scale_factor)
    }
}

fn enumerate_displays() -> Vec<Display> {
    let mut displays: Vec<Display> = Vec::new();
    let lparam = LPARAM(std::ptr::addr_of_mut!(displays) as isize);
    // SAFETY: `monitor_enum_proc` matches the `MONITORENUMPROC` signature.
    // `lparam` points at `displays`, which outlives this whole synchronous
    // enumeration call — `EnumDisplayMonitors` doesn't return until every
    // callback invocation has completed.
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(monitor_enum_proc), lparam);
    }
    displays
}

/// # Safety
/// Called synchronously by `EnumDisplayMonitors`, once per attached
/// monitor, for the duration of the call in `enumerate_displays` — `lparam`
/// is valid for that entire window.
unsafe extern "system" fn monitor_enum_proc(
    monitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    // SAFETY: see function-level SAFETY comment above.
    let displays = unsafe { &mut *(lparam.0 as *mut Vec<Display>) };

    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize =
        u32::try_from(size_of::<MONITORINFOEXW>()).expect("MONITORINFOEXW size fits in u32");
    // SAFETY: `info.monitorInfo.cbSize` is set as `GetMonitorInfoW`
    // requires (it uses this to distinguish `MONITORINFO` from the larger
    // `MONITORINFOEXW`), and `monitor` is the handle the OS just supplied
    // via this enum callback.
    let ok = unsafe { GetMonitorInfoW(monitor, std::ptr::addr_of_mut!(info).cast()) };
    if ok.as_bool() {
        let mut dpi_x = 96u32;
        let mut dpi_y = 96u32;
        // SAFETY: `monitor` is valid for the duration of this callback;
        // `dpi_x`/`dpi_y` are valid, exclusively-owned out-params.
        let _ =
            unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &raw mut dpi_x, &raw mut dpi_y) };

        let rc = info.monitorInfo.rcMonitor;
        displays.push(Display {
            // HMONITOR handles are stable only for the current session,
            // which matches `DisplayId`'s documented contract — this is an
            // opaque-handle-as-id truncation, not a real quantity.
            id: DisplayId(monitor.0 as u32),
            bounds: Rect {
                x: rc.left,
                y: rc.top,
                width: (rc.right - rc.left).cast_unsigned(),
                height: (rc.bottom - rc.top).cast_unsigned(),
            },
            scale_factor: f64::from(dpi_x) / 96.0,
            is_primary: (info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY) != 0,
        });
    }

    BOOL(1) // Non-zero: keep enumerating.
}

/// The bounding box of every display's `bounds`, i.e. the whole virtual
/// desktop. Displays are typically packed with no gaps, but this makes no
/// such assumption — it just takes the outer extent.
fn union_bounds(displays: &[Display]) -> Rect {
    let Some(first) = displays.first() else {
        return Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };
    };

    let mut min_x = first.bounds.x;
    let mut min_y = first.bounds.y;
    let mut max_x = first.bounds.x + first.bounds.width.cast_signed();
    let mut max_y = first.bounds.y + first.bounds.height.cast_signed();

    for d in &displays[1..] {
        min_x = min_x.min(d.bounds.x);
        min_y = min_y.min(d.bounds.y);
        max_x = max_x.max(d.bounds.x + d.bounds.width.cast_signed());
        max_y = max_y.max(d.bounds.y + d.bounds.height.cast_signed());
    }

    Rect {
        x: min_x,
        y: min_y,
        width: (max_x - min_x).cast_unsigned(),
        height: (max_y - min_y).cast_unsigned(),
    }
}
