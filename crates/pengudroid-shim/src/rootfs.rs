//! A virtual Android root for guest processes.
//!
//! Android code addresses its platform by absolute mount points: `/system`,
//! `/apex`, `/vendor`, `/data`.  PenguDroid does not mount anything and does not
//! use namespaces, so those paths do not exist on the host.  When
//! `PENGUDROID_ROOT` names an extracted Android tree, the shim rewrites any
//! absolute path under a known Android mount point to `<root>/<path>` before the
//! operation reaches the kernel.
//!
//! This is what lets ART's Java-side boot class loader open
//! `/system/framework/framework.jar`: ART records the boot class path as
//! *locations* (`/system/...`, `/apex/...`) because the boot image was built
//! with those names, and `VMClassLoader` later re-opens them by name.  Keeping
//! the locations and translating the lookups is the Wine-style answer; it needs
//! no `chroot`, no mount namespace and no root privileges.
//!
//! Only *guest* mount points are rewritten.  Host paths (`/proc`, `/dev`,
//! `/etc`, the sysroot's own location, ...) are left untouched, so the same
//! process can still use the host normally.

use std::ffi::{CStr, CString};
use std::sync::OnceLock;

use libc::{c_char, c_int, c_void};

/// Android mount points that belong to the guest, not the host.
///
/// `/proc`, `/sys`, `/dev` and `/etc` are deliberately absent: those are shared
/// with the host and redirecting them would break ordinary Linux behaviour.
const GUEST_MOUNTS: &[&str] = &[
    "/apex",
    "/system",
    "/system_ext",
    "/vendor",
    "/product",
    "/odm",
    "/data",
    "/linkerconfig",
    "/vendor_dlkm",
    "/odm_dlkm",
    "/mnt",
];

/// The configured virtual root, read once.
fn root() -> Option<&'static [u8]> {
    static ROOT: OnceLock<Option<Vec<u8>>> = OnceLock::new();
    ROOT.get_or_init(|| {
        let v = std::env::var_os("PENGUDROID_ROOT")?;
        let bytes = std::os::unix::ffi::OsStrExt::as_bytes(v.as_os_str()).to_vec();
        if bytes.is_empty() || bytes == b"/" {
            None
        } else {
            Some(bytes)
        }
    })
    .as_deref()
}

/// Whether `path` names something inside the virtual Android root.
fn guest_path(path: &[u8]) -> bool {
    if !path.starts_with(b"/") {
        return false;
    }
    GUEST_MOUNTS.iter().any(|m| {
        let m = m.as_bytes();
        path == m || path.starts_with(m) && path.get(m.len()) == Some(&b'/')
    })
}

/// Map `path` into `root`, or return `None` when no rewrite applies.
///
/// Split out from [`map_bytes`] so the rule can be unit-tested without
/// mutating the process environment.
pub fn map_with_root(root: &[u8], path: &[u8]) -> Option<Vec<u8>> {
    if root.is_empty() || root == b"/" || !guest_path(path) {
        return None;
    }
    let mut out = Vec::with_capacity(root.len() + path.len());
    out.extend_from_slice(root);
    out.extend_from_slice(path);
    Some(out)
}

/// Map `path` into the virtual root, or return `None` when no rewrite applies.
pub fn map_bytes(path: &[u8]) -> Option<Vec<u8>> {
    map_with_root(root()?, path)
}

/// `char *__pengudroid_map_path(const char *path)`
///
/// Returns a heap-allocated replacement path, or `NULL` when the caller should
/// use the original pointer.  The C wrappers hand the result back through
/// [`__pengudroid_free_path`].
///
/// # Safety
/// `path` must be a valid NUL-terminated string or `NULL`.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_map_path(path: *const c_char) -> *mut c_char {
    if path.is_null() {
        return std::ptr::null_mut();
    }
    let bytes = CStr::from_ptr(path).to_bytes();
    match map_bytes(bytes).and_then(|m| CString::new(m).ok()) {
        Some(s) => s.into_raw(),
        None => std::ptr::null_mut(),
    }
}

/// Free a pointer returned by [`__pengudroid_map_path`].
///
/// # Safety
/// `p` must be `NULL` or a pointer previously returned by
/// [`__pengudroid_map_path`] and not yet freed.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_free_path(p: *mut c_char) {
    if !p.is_null() {
        drop(CString::from_raw(p));
    }
}

/// Run `f` with a mapped path when one applies, or with the original pointer.
///
/// # Safety
/// `path` must be a valid NUL-terminated string or `NULL`.
unsafe fn with_path<R>(path: *const c_char, f: impl FnOnce(*const c_char) -> R) -> R {
    if path.is_null() {
        return f(path);
    }
    let bytes = CStr::from_ptr(path).to_bytes();
    match map_bytes(bytes).and_then(|m| CString::new(m).ok()) {
        Some(mapped) => f(mapped.as_ptr()),
        None => f(path),
    }
}

/// `RTLD_NEXT` — search after this object.
const RTLD_NEXT: *mut c_void = -1isize as *mut c_void;

/// Resolve the host implementation of `name`, or `None`.
unsafe fn next<T: Copy>(name: &[u8]) -> Option<T> {
    debug_assert_eq!(std::mem::size_of::<T>(), std::mem::size_of::<*mut c_void>());
    let p = libc::dlsym(RTLD_NEXT, name.as_ptr() as *const c_char);
    if p.is_null() {
        None
    } else {
        Some(std::mem::transmute_copy(&p))
    }
}

// --- non-variadic file entry points ---------------------------------------
//
// `open`/`openat` are C-variadic and live in the generated C translation unit
// (`src/pathshim.rs`).  Everything else forwards here.

type StatFn = unsafe extern "C" fn(*const c_char, *mut c_void) -> c_int;
type StatAtFn = unsafe extern "C" fn(c_int, *const c_char, *mut c_void, c_int) -> c_int;
type AccessFn = unsafe extern "C" fn(*const c_char, c_int) -> c_int;
type AccessAtFn = unsafe extern "C" fn(c_int, *const c_char, c_int, c_int) -> c_int;
type ReadlinkFn = unsafe extern "C" fn(*const c_char, *mut c_char, usize) -> isize;
type ReadlinkAtFn = unsafe extern "C" fn(c_int, *const c_char, *mut c_char, usize) -> isize;
type OpenDirFn = unsafe extern "C" fn(*const c_char) -> *mut c_void;
type FopenFn = unsafe extern "C" fn(*const c_char, *const c_char) -> *mut c_void;
type FreopenFn = unsafe extern "C" fn(*const c_char, *const c_char, *mut c_void) -> *mut c_void;
type RealpathFn = unsafe extern "C" fn(*const c_char, *mut c_char) -> *mut c_char;
type Open2Fn = unsafe extern "C" fn(*const c_char, c_int) -> c_int;
type OpenAt2Fn = unsafe extern "C" fn(c_int, *const c_char, c_int) -> c_int;

/// `stat(path, buf)`.
#[no_mangle]
pub unsafe extern "C" fn stat(path: *const c_char, buf: *mut c_void) -> c_int {
    match next::<StatFn>(b"stat\0") {
        Some(f) => with_path(path, |p| f(p, buf)),
        None => -1,
    }
}

/// `lstat(path, buf)`.
#[no_mangle]
pub unsafe extern "C" fn lstat(path: *const c_char, buf: *mut c_void) -> c_int {
    match next::<StatFn>(b"lstat\0") {
        Some(f) => with_path(path, |p| f(p, buf)),
        None => -1,
    }
}

/// `fstatat(dirfd, path, buf, flags)`.
#[no_mangle]
pub unsafe extern "C" fn fstatat(
    dirfd: c_int,
    path: *const c_char,
    buf: *mut c_void,
    flags: c_int,
) -> c_int {
    match next::<StatAtFn>(b"fstatat\0") {
        Some(f) => with_path(path, |p| f(dirfd, p, buf, flags)),
        None => -1,
    }
}

/// `access(path, mode)`.
#[no_mangle]
pub unsafe extern "C" fn access(path: *const c_char, mode: c_int) -> c_int {
    match next::<AccessFn>(b"access\0") {
        Some(f) => with_path(path, |p| f(p, mode)),
        None => -1,
    }
}

/// `faccessat(dirfd, path, mode, flags)`.
#[no_mangle]
pub unsafe extern "C" fn faccessat(
    dirfd: c_int,
    path: *const c_char,
    mode: c_int,
    flags: c_int,
) -> c_int {
    match next::<AccessAtFn>(b"faccessat\0") {
        Some(f) => with_path(path, |p| f(dirfd, p, mode, flags)),
        None => -1,
    }
}

/// `readlink(path, buf, size)`.
#[no_mangle]
pub unsafe extern "C" fn readlink(
    path: *const c_char,
    buf: *mut c_char,
    size: usize,
) -> isize {
    match next::<ReadlinkFn>(b"readlink\0") {
        Some(f) => with_path(path, |p| f(p, buf, size)),
        None => -1,
    }
}

/// `readlinkat(dirfd, path, buf, size)`.
#[no_mangle]
pub unsafe extern "C" fn readlinkat(
    dirfd: c_int,
    path: *const c_char,
    buf: *mut c_char,
    size: usize,
) -> isize {
    match next::<ReadlinkAtFn>(b"readlinkat\0") {
        Some(f) => with_path(path, |p| f(dirfd, p, buf, size)),
        None => -1,
    }
}

/// `opendir(path)`.
#[no_mangle]
pub unsafe extern "C" fn opendir(path: *const c_char) -> *mut c_void {
    match next::<OpenDirFn>(b"opendir\0") {
        Some(f) => with_path(path, |p| f(p)),
        None => std::ptr::null_mut(),
    }
}

/// `fopen(path, mode)`.
#[no_mangle]
pub unsafe extern "C" fn fopen(path: *const c_char, mode: *const c_char) -> *mut c_void {
    match next::<FopenFn>(b"fopen\0") {
        Some(f) => with_path(path, |p| f(p, mode)),
        None => std::ptr::null_mut(),
    }
}

/// `fopen64(path, mode)`.
#[no_mangle]
pub unsafe extern "C" fn fopen64(path: *const c_char, mode: *const c_char) -> *mut c_void {
    match next::<FopenFn>(b"fopen64\0") {
        Some(f) => with_path(path, |p| f(p, mode)),
        None => std::ptr::null_mut(),
    }
}

/// `freopen(path, mode, stream)`.
#[no_mangle]
pub unsafe extern "C" fn freopen(
    path: *const c_char,
    mode: *const c_char,
    stream: *mut c_void,
) -> *mut c_void {
    match next::<FreopenFn>(b"freopen\0") {
        Some(f) => with_path(path, |p| f(p, mode, stream)),
        None => std::ptr::null_mut(),
    }
}

/// `realpath(path, resolved)`.
#[no_mangle]
pub unsafe extern "C" fn realpath(path: *const c_char, resolved: *mut c_char) -> *mut c_char {
    match next::<RealpathFn>(b"realpath\0") {
        Some(f) => with_path(path, |p| f(p, resolved)),
        None => std::ptr::null_mut(),
    }
}

/// `__open_2(path, flags)` — the `_FORTIFY_SOURCE` form without a mode.
#[no_mangle]
pub unsafe extern "C" fn __open_2(path: *const c_char, flags: c_int) -> c_int {
    if !path.is_null() {
        let bfd = crate::binder::__pengudroid_binder_open(path);
        if bfd >= 0 {
            return bfd;
        }
    }
    match next::<Open2Fn>(b"__open_2\0") {
        Some(f) => with_path(path, |p| f(p, flags)),
        None => -1,
    }
}

/// `__openat_2(dirfd, path, flags)`.
#[no_mangle]
pub unsafe extern "C" fn __openat_2(dirfd: c_int, path: *const c_char, flags: c_int) -> c_int {
    if !path.is_null() {
        let bfd = crate::binder::__pengudroid_binder_open(path);
        if bfd >= 0 {
            return bfd;
        }
    }
    match next::<OpenAt2Fn>(b"__openat_2\0") {
        Some(f) => with_path(path, |p| f(dirfd, p, flags)),
        None => -1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &[u8] = b"/opt/android";

    #[test]
    fn maps_guest_mount_points() {
        assert_eq!(
            map_with_root(ROOT, b"/system/framework/framework.jar").unwrap(),
            b"/opt/android/system/framework/framework.jar"
        );
        assert_eq!(
            map_with_root(ROOT, b"/apex/com.android.art/javalib/core-oj.jar").unwrap(),
            b"/opt/android/apex/com.android.art/javalib/core-oj.jar"
        );
        assert_eq!(
            map_with_root(ROOT, b"/data/system/users/0").unwrap(),
            b"/opt/android/data/system/users/0"
        );
    }

    #[test]
    fn maps_the_bare_mount_point() {
        assert_eq!(map_with_root(ROOT, b"/system").unwrap(), b"/opt/android/system");
    }

    #[test]
    fn leaves_host_paths_alone() {
        for path in [
            b"/proc/self/maps".as_slice(),
            b"/dev/binder",
            b"/etc/passwd",
            b"/usr/lib/libc.so",
            b"system/framework",
            b"",
        ] {
            assert!(map_with_root(ROOT, path).is_none(), "{path:?} must not be rewritten");
        }
    }

    #[test]
    fn does_not_match_a_longer_name_with_the_same_prefix() {
        // `/system_ext` is a mount of its own; `/systemxyz` is not a mount at all.
        assert!(map_with_root(ROOT, b"/systemxyz").is_none());
        assert_eq!(
            map_with_root(ROOT, b"/system_ext/lib64/x.so").unwrap(),
            b"/opt/android/system_ext/lib64/x.so"
        );
    }

    #[test]
    fn an_unset_or_root_root_disables_mapping() {
        assert!(map_with_root(b"", b"/system/x").is_none());
        assert!(map_with_root(b"/", b"/system/x").is_none());
    }
}
