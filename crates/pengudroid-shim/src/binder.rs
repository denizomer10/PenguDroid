//! A minimal `/dev/binder` device emulation.
//!
//! Android's `libbinder` talks to the kernel binder driver through a character
//! device: `open("/dev/binder")`, `ioctl(BINDER_VERSION)`, an `mmap` of the
//! transfer area, then `ioctl(BINDER_WRITE_READ)` for every transaction.
//! PenguDroid does not mount binderfs and does not need the kernel driver: the
//! daemon already runs a userspace broker. This module makes the *device
//! surface* exist so `ProcessState::ProcessState` initialises, which is the
//! first thing `ActivityThread.main` needs.
//!
//! Current scope: the control ioctls succeed and transactions are refused with
//! `EINVAL`, which `libbinder` surfaces as a `RemoteException` instead of the
//! `LOG(FATAL)` that a missing `/dev/binder` produces. Wiring transactions to
//! the daemon's broker is the next step and happens here.
//!
//! The fd is a `memfd` so the `mmap` succeeds and no kernel driver is involved.

use std::ffi::CStr;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicI32, Ordering};

use libc::{c_int, c_ulong, c_void};

/// Protocol version `libbinder` expects from Android 10 onward.
const BINDER_CURRENT_PROTOCOL_VERSION: c_int = 8;

const BINDER_WRITE_READ: c_ulong = 0xC030_6201;
const BINDER_SET_MAX_THREADS: c_ulong = 0x4004_6205;
const BINDER_SET_CONTEXT_MGR: c_ulong = 0x4004_6207;
const BINDER_VERSION: c_ulong = 0xC004_6209;
const BINDER_THREAD_EXIT: c_ulong = 0x4004_6208;

/// Size of the mapping `libbinder` requests; a `memfd` longer than this is
/// enough (the mapping is never dereferenced by libbinder itself).
const MAP_SIZE: i64 = 1 << 20;

/// The single emulated device fd, or `-1`.
static BINDER_FD: AtomicI32 = AtomicI32::new(-1);

/// Whether `path` names one of Android's binder devices.
fn is_binder_device(path: &[u8]) -> bool {
    matches!(
        path,
        b"/dev/binder" | b"/dev/binderfs/binder" | b"/dev/hwbinder" | b"/dev/vndbinder"
            | b"/dev/binderfs/hwbinder" | b"/dev/binderfs/vndbinder"
    )
}

/// Open the emulated device, returning a fd, or `None` if `path` is not a
/// binder device.
///
/// The fd is shared: `libbinder` opens the device once per process, so a
/// second open returns the same descriptor.
pub fn open_device(path: &[u8]) -> Option<RawFd> {
    if !is_binder_device(path) {
        return None;
    }
    let existing = BINDER_FD.load(Ordering::Relaxed);
    if existing >= 0 {
        return Some(existing);
    }
    let name = b"pengudroid-binder\0";
    // SAFETY: the name is NUL-terminated; the flags are the documented ones.
    let fd = unsafe { libc::memfd_create(name.as_ptr() as *const libc::c_char, libc::MFD_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    // SAFETY: `fd` is a fresh memfd.
    if unsafe { libc::ftruncate(fd, MAP_SIZE) } != 0 {
        unsafe { libc::close(fd) };
        return None;
    }
    BINDER_FD.store(fd, Ordering::Relaxed);
    Some(fd)
}

/// Whether `fd` is the emulated device.
pub fn is_device(fd: RawFd) -> bool {
    fd >= 0 && fd == BINDER_FD.load(Ordering::Relaxed)
}

/// Handle an ioctl on the emulated device.
///
/// Returns `Some(result)` when handled (the value the ioctl returns), or
/// `None` when the request is unknown, in which case the caller forwards it.
///
/// # Safety
/// `arg` must be null or a pointer valid for the request being issued.
pub unsafe fn ioctl(fd: RawFd, request: c_ulong, arg: *mut c_void) -> Option<c_int> {
    if !is_device(fd) {
        return None;
    }
    match request {
        BINDER_VERSION => {
            if arg.is_null() {
                return Some(-1);
            }
            *(arg as *mut c_int) = BINDER_CURRENT_PROTOCOL_VERSION;
            Some(0)
        }
        BINDER_SET_MAX_THREADS | BINDER_SET_CONTEXT_MGR | BINDER_THREAD_EXIT => Some(0),
        BINDER_WRITE_READ => {
            // No broker yet. Failing the call makes `libbinder` raise a
            // RemoteException; claiming success without filling the read
            // buffer would make `waitForResponse` spin forever.
            *libc::__errno_location() = libc::EINVAL;
            Some(-1)
        }
        _ => Some(0),
    }
}

/// `int __pengudroid_binder_open(const char *path)`
///
/// Called from the C `open`/`openat` shims before ordinary path handling.
/// Returns a fd for a binder device, or `-1` when the path is something else.
///
/// # Safety
/// `path` must be a valid NUL-terminated string or `NULL`.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_binder_open(path: *const libc::c_char) -> c_int {
    if path.is_null() {
        return -1;
    }
    match open_device(CStr::from_ptr(path).to_bytes()) {
        Some(fd) => fd,
        None => -1,
    }
}

/// `long __pengudroid_binder_ioctl(int fd, unsigned long request, void *arg)`
///
/// `PD_IOCTL_NOT_HANDLED` (`-4096`) tells the C shim to forward the request.
///
/// # Safety
/// `arg` must be valid for `request`.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_binder_ioctl(
    fd: c_int,
    request: c_ulong,
    arg: *mut c_void,
) -> i64 {
    match ioctl(fd, request, arg) {
        Some(result) => result as i64,
        None => -4096,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_the_binder_devices() {
        for p in [
            b"/dev/binder".as_slice(),
            b"/dev/binderfs/binder",
            b"/dev/hwbinder",
            b"/dev/vndbinder",
        ] {
            assert!(is_binder_device(p), "{p:?}");
        }
        for p in [b"/dev/ashmem".as_slice(), b"/system/binder", b"/dev/binderx"] {
            assert!(!is_binder_device(p), "{p:?}");
        }
    }

    #[test]
    fn the_version_ioctl_reports_protocol_8() {
        if open_device(b"/dev/binder").is_none() {
            return; // memfd unavailable on this host
        }
        let fd = BINDER_FD.load(Ordering::Relaxed);
        assert!(is_device(fd));
        let mut version: c_int = 0;
        let r = unsafe {
            ioctl(
                fd,
                BINDER_VERSION,
                &mut version as *mut c_int as *mut c_void,
            )
        };
        assert_eq!(r, Some(0));
        assert_eq!(version, BINDER_CURRENT_PROTOCOL_VERSION);
    }

    #[test]
    fn ioctls_on_other_fds_are_not_handled() {
        assert!(unsafe { ioctl(0, BINDER_VERSION, std::ptr::null_mut()) }.is_none());
    }
}
