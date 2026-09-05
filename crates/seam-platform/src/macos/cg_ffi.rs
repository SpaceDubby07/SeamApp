//! Raw `extern "C"` bindings for the slice of CoreGraphics/CoreFoundation/
//! `ApplicationServices` used by `capture.rs`, `inject.rs`, `screens.rs`, and
//! `permissions.rs`.
//!
//! These are hand-written against the stable C ABI rather than pulled from
//! a wrapper crate — the Quartz Event Services / Core Foundation C
//! interfaces have been unchanged for well over a decade, which makes them
//! easier to get right from memory than a newer Rust binding crate's exact
//! wrapper shape. Only the subset actually used elsewhere in `macos/` is
//! declared here; nothing is `pub` outside this crate.

#![allow(non_camel_case_types, non_upper_case_globals, dead_code)]

use std::ffi::c_void;

pub type CGFloat = f64;
pub type CFIndex = isize;
pub type CFTypeRef = *const c_void;
pub type CFAllocatorRef = *const c_void;
pub type CFStringRef = *const c_void;
pub type CFRunLoopRef = *mut c_void;
pub type CFRunLoopSourceRef = *mut c_void;
pub type CFMachPortRef = *mut c_void;
pub type CGEventRef = *mut c_void;
pub type CGEventTapProxy = *mut c_void;
pub type CGEventSourceRef = *const c_void;
pub type CGDirectDisplayID = u32;

/// Layout must exactly match Apple's `CGPoint`: two `CGFloat`s (`f64`), no
/// padding.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CGPoint {
    pub x: CGFloat,
    pub y: CGFloat,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CGSize {
    pub width: CGFloat,
    pub height: CGFloat,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CGRect {
    pub origin: CGPoint,
    pub size: CGSize,
}

// ─────────────────────────── CGEventType ───────────────────────────
pub const K_CG_EVENT_LEFT_MOUSE_DOWN: u32 = 1;
pub const K_CG_EVENT_LEFT_MOUSE_UP: u32 = 2;
pub const K_CG_EVENT_RIGHT_MOUSE_DOWN: u32 = 3;
pub const K_CG_EVENT_RIGHT_MOUSE_UP: u32 = 4;
pub const K_CG_EVENT_MOUSE_MOVED: u32 = 5;
pub const K_CG_EVENT_LEFT_MOUSE_DRAGGED: u32 = 6;
pub const K_CG_EVENT_RIGHT_MOUSE_DRAGGED: u32 = 7;
pub const K_CG_EVENT_KEY_DOWN: u32 = 10;
pub const K_CG_EVENT_KEY_UP: u32 = 11;
pub const K_CG_EVENT_FLAGS_CHANGED: u32 = 12;
pub const K_CG_EVENT_SCROLL_WHEEL: u32 = 22;
pub const K_CG_EVENT_OTHER_MOUSE_DOWN: u32 = 25;
pub const K_CG_EVENT_OTHER_MOUSE_UP: u32 = 26;
pub const K_CG_EVENT_OTHER_MOUSE_DRAGGED: u32 = 27;
pub const K_CG_EVENT_TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
pub const K_CG_EVENT_TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;

// ─────────────────────── CGEventTap location/placement/options ───────────────────────
pub const K_CG_HID_EVENT_TAP: u32 = 0;
pub const K_CG_HEAD_INSERT_EVENT_TAP: u32 = 0;
pub const K_CG_EVENT_TAP_OPTION_DEFAULT: u32 = 0;

// ─────────────────────────── CGEventFlags bits ───────────────────────────
pub const K_CG_EVENT_FLAG_MASK_ALPHA_SHIFT: u64 = 0x0001_0000;
pub const K_CG_EVENT_FLAG_MASK_SHIFT: u64 = 0x0002_0000;
pub const K_CG_EVENT_FLAG_MASK_CONTROL: u64 = 0x0004_0000;
pub const K_CG_EVENT_FLAG_MASK_ALTERNATE: u64 = 0x0008_0000;
pub const K_CG_EVENT_FLAG_MASK_COMMAND: u64 = 0x0010_0000;

// ─────────────────────────── CGEventField ───────────────────────────
pub const K_CG_MOUSE_EVENT_BUTTON_NUMBER: u32 = 3;
pub const K_CG_MOUSE_EVENT_DELTA_X: u32 = 4;
pub const K_CG_MOUSE_EVENT_DELTA_Y: u32 = 5;
pub const K_CG_KEYBOARD_EVENT_AUTOREPEAT: u32 = 8;
pub const K_CG_KEYBOARD_EVENT_KEYCODE: u32 = 9;
pub const K_CG_SCROLL_WHEEL_EVENT_DELTA_AXIS_1: u32 = 11; // vertical
pub const K_CG_SCROLL_WHEEL_EVENT_DELTA_AXIS_2: u32 = 12; // horizontal

// ─────────────────────────── CGMouseButton ───────────────────────────
pub const K_CG_MOUSE_BUTTON_LEFT: u32 = 0;
pub const K_CG_MOUSE_BUTTON_RIGHT: u32 = 1;
pub const K_CG_MOUSE_BUTTON_CENTER: u32 = 2;

pub type CGEventTapCallBack = unsafe extern "C" fn(
    proxy: CGEventTapProxy,
    event_type: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    // ── Event tap lifecycle ──
    pub fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: CGEventTapCallBack,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    pub fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);

    // ── Reading event data ──
    pub fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
    pub fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    pub fn CGEventGetFlags(event: CGEventRef) -> u64;
    pub fn CGEventGetType(event: CGEventRef) -> u32;

    // ── Creating and posting synthetic events ──
    pub fn CGEventCreateMouseEvent(
        source: CGEventSourceRef,
        mouse_type: u32,
        mouse_cursor_position: CGPoint,
        mouse_button: u32,
    ) -> CGEventRef;
    pub fn CGEventCreateKeyboardEvent(
        source: CGEventSourceRef,
        virtual_key: u16,
        key_down: bool,
    ) -> CGEventRef;
    pub fn CGEventCreateScrollWheelEvent(
        source: CGEventSourceRef,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        ...
    ) -> CGEventRef;
    pub fn CGEventSetFlags(event: CGEventRef, flags: u64);
    pub fn CGEventPost(tap: u32, event: CGEventRef);
    pub fn CGWarpMouseCursorPosition(new_cursor_position: CGPoint) -> i32;
    pub fn CFRelease(cf: CFTypeRef);

    // ── Display enumeration ──
    pub fn CGGetActiveDisplayList(
        max_displays: u32,
        active_displays: *mut CGDirectDisplayID,
        display_count: *mut u32,
    ) -> i32;
    pub fn CGMainDisplayID() -> CGDirectDisplayID;
    pub fn CGDisplayBounds(display: CGDirectDisplayID) -> CGRect;
    pub fn CGDisplayPixelsWide(display: CGDirectDisplayID) -> usize;
    pub fn CGDisplayPixelsHigh(display: CGDirectDisplayID) -> usize;

    // ── Accessibility permission (Tier 11.1) ──
    pub fn AXIsProcessTrusted() -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    pub fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    pub fn CFMachPortCreateRunLoopSource(
        allocator: CFAllocatorRef,
        port: CFMachPortRef,
        order: CFIndex,
    ) -> CFRunLoopSourceRef;
    pub fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    pub fn CFRunLoopRun();
    pub fn CFRunLoopStop(rl: CFRunLoopRef);
    pub fn CFMachPortInvalidate(port: CFMachPortRef);

    pub static kCFRunLoopCommonModes: CFStringRef;
}
