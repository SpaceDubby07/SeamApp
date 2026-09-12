//! Prevents this Mac from sleeping while it's being driven remotely.
//!
//! Barrier's own fix for this exact failure mode
//! (`ArchMiscWindows::addBusyState`/`kSYSTEM`) is Windows-only — its comment
//! explains why it exists: a machine that's only receiving synthetic
//! (injected) input has no real local HID activity of its own to keep its
//! idle timer fresh, and once it sleeps, injected input can't wake it back
//! up remotely — the session is dead until someone physically touches it.
//! Barrier never ported the same protection to macOS; this module does,
//! since either machine can end up on the driven side here. `IOKit`'s Power
//! Management assertions (`IOPMAssertionCreateWithName`/`IOPMAssertionRelease`)
//! are macOS's equivalent of Windows' `SetThreadExecutionState`.

use std::ffi::{CString, c_char, c_void};
use std::sync::atomic::{AtomicU32, Ordering};

type CFStringRef = *const c_void;
type IoPmAssertionId = u32;
type IoPmAssertionLevel = u32;
type IoReturn = i32;

/// `kIOReturnSuccess` — `IOKit`'s success sentinel is plain zero, not a
/// `sys_iokit`-subsystem-coded value like most other `IOReturn`s.
const K_IO_RETURN_SUCCESS: IoReturn = 0;
/// `kIOPMAssertionLevelOn` (`IOPMLib.h`).
const K_IOPM_ASSERTION_LEVEL_ON: IoPmAssertionLevel = 255;
/// `kIOPMAssertionIDInvalid` — also doubles as "no assertion currently held"
/// for [`ASSERTION_ID`], since a real id is never zero.
const NO_ASSERTION: IoPmAssertionId = 0;
/// `kCFStringEncodingUTF8` (`CFString.h`).
const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    /// `kIOPMAssertionTypePreventUserIdleSystemSleep` (`IOPMLib.h`) — blocks
    /// idle system sleep specifically (not display sleep); matches Barrier's
    /// `kSYSTEM`, the critical half of its Windows fix (a slept machine is
    /// the unrecoverable failure mode; a dimmed display is merely annoying).
    static kIOPMAssertionTypePreventUserIdleSystemSleep: CFStringRef;
    fn IOPMAssertionCreateWithName(
        assertion_type: CFStringRef,
        assertion_level: IoPmAssertionLevel,
        assertion_name: CFStringRef,
        assertion_id: *mut IoPmAssertionId,
    ) -> IoReturn;
    fn IOPMAssertionRelease(assertion_id: IoPmAssertionId) -> IoReturn;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(
        alloc: *const c_void,
        c_str: *const c_char,
        encoding: u32,
    ) -> CFStringRef;
    fn CFRelease(cf: *const c_void);
}

/// The currently-held assertion id, or [`NO_ASSERTION`]. A process-wide
/// atomic rather than anything tied to `Sink` itself: `set_being_driven` can
/// run on whatever thread executes the session's actions, with no fixed
/// thread identity the way the capture thread's hook state has.
static ASSERTION_ID: AtomicU32 = AtomicU32::new(NO_ASSERTION);

/// Holds (or releases) the idle-sleep assertion — see the module doc for
/// why. Idempotent: calling with the same value twice in a row is a no-op.
pub fn set_being_driven(being_driven: bool) {
    let currently_held = ASSERTION_ID.load(Ordering::SeqCst) != NO_ASSERTION;
    if being_driven == currently_held {
        return;
    }

    if being_driven {
        acquire();
    } else {
        release();
    }
}

fn acquire() {
    let Ok(name) = CString::new("Seam: being driven remotely") else {
        // Never actually fails for a fixed ASCII literal with no interior
        // nul, but `set_being_driven` must not panic over this.
        return;
    };
    // SAFETY: `name` is a valid, nul-terminated C string for the duration
    // of this call; a null allocator means "use the default allocator",
    // which `CFStringCreateWithCString` documents as valid.
    let cf_name = unsafe {
        CFStringCreateWithCString(std::ptr::null(), name.as_ptr(), K_CF_STRING_ENCODING_UTF8)
    };

    let mut id: IoPmAssertionId = NO_ASSERTION;
    // SAFETY: `kIOPMAssertionTypePreventUserIdleSystemSleep` is a static
    // CFStringRef IOKit owns for the process's lifetime; `cf_name` is valid
    // (or null, which IOKit accepts as "no name") from just above; `id` is
    // a valid, exclusively-owned out-parameter.
    let result = unsafe {
        IOPMAssertionCreateWithName(
            kIOPMAssertionTypePreventUserIdleSystemSleep,
            K_IOPM_ASSERTION_LEVEL_ON,
            cf_name,
            &raw mut id,
        )
    };

    if !cf_name.is_null() {
        // SAFETY: balances the successful `CFStringCreateWithCString`
        // above — `IOPMAssertionCreateWithName` copies what it needs from
        // the name, it doesn't take ownership of ours.
        unsafe { CFRelease(cf_name) };
    }

    if result == K_IO_RETURN_SUCCESS {
        ASSERTION_ID.store(id, Ordering::SeqCst);
    } else {
        tracing::warn!(
            result,
            "IOPMAssertionCreateWithName failed; this Mac may sleep while being driven"
        );
    }
}

fn release() {
    let id = ASSERTION_ID.swap(NO_ASSERTION, Ordering::SeqCst);
    if id == NO_ASSERTION {
        return;
    }
    // SAFETY: `id` came from a successful `IOPMAssertionCreateWithName`
    // above and hasn't been released since.
    let result = unsafe { IOPMAssertionRelease(id) };
    if result != K_IO_RETURN_SUCCESS {
        tracing::warn!(result, "IOPMAssertionRelease failed");
    }
}
