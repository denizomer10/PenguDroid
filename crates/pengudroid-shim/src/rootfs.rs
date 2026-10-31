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
//!
//! Optional `PENGUDROID_ARM64_GUEST_ROOT` overlays only native-bridge runtime
//! paths. The host ART/framework stays in `PENGUDROID_ROOT`; guest linker/libc
//! must never replace the host's x86_64 libraries.

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

fn arm64_root() -> Option<&'static [u8]> {
    static ROOT: OnceLock<Option<Vec<u8>>> = OnceLock::new();
    ROOT.get_or_init(|| {
        let value = std::env::var_os("PENGUDROID_ARM64_GUEST_ROOT")?;
        let bytes = std::os::unix::ffi::OsStrExt::as_bytes(value.as_os_str());
        (bytes.starts_with(b"/") && bytes != b"/").then(|| bytes.to_vec())
    }).as_deref()
}

fn under(path: &[u8], prefix: &[u8]) -> bool {
    path == prefix || path.starts_with(prefix) && path.get(prefix.len()) == Some(&b'/')
}

/// Deliberately a narrow overlay, not a second whole `/system` tree. Redirect
/// the optional Berberis directories too, so a stale host overlay cannot win
/// Digitalis's alternative-location probe over the selected guest runtime.
fn arm64_runtime_path(path: &[u8]) -> bool {
    path == b"/system/etc/ld.config.arm64.txt" || [
        b"/system/bin/arm64".as_slice(), b"/system/lib64/arm64",
        b"/system/bin/berberis", b"/system/lib64/berberis",
    ].iter().any(|prefix| under(path, prefix))
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

fn map_with_roots(host: Option<&[u8]>, arm64: Option<&[u8]>, path: &[u8]) -> Option<Vec<u8>> {
    if arm64_runtime_path(path) {
        if let Some(arm64) = arm64 {
            // No existence-based fallback: a missing guest dependency is an
            // error, not permission to load the stock GSI's incompatible libc.
            return map_with_root(arm64, path);
        }
    }
    map_with_root(host?, path)
}

/// Map `path` into the selected runtime root, or leave host paths untouched.
pub fn map_bytes(path: &[u8]) -> Option<Vec<u8>> {
    map_with_roots(root(), arm64_root(), path)
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
    let mapped = map_bytes(bytes).and_then(|m| CString::new(m).ok());
    if trace_enabled() {
        match &mapped {
            Some(m) => eprintln!(
                "[rootfs] {} -> {}",
                String::from_utf8_lossy(bytes),
                String::from_utf8_lossy(m.as_bytes())
            ),
            None => eprintln!("[rootfs] {} (host path)", String::from_utf8_lossy(bytes)),
        }
    }
    match mapped {
        Some(mapped) => f(mapped.as_ptr()),
        None => f(path),
    }
}

/// Whether `PENGUDROID_TRACE_ROOTFS` asks for a log of every path rewrite.
fn trace_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("PENGUDROID_TRACE_ROOTFS").is_some())
}

/// Report an `open`/`openat` that failed, when `PENGUDROID_TRACE_OPEN` is set.
///
/// A guest that silently gives up on a missing resource is the hardest kind of
/// bug to see: the failure is one `errno` inside native code that logs nothing.
/// This lists each distinct path exactly once, so a full run costs a few hundred
/// lines instead of one per retry. Host paths that legitimately come and go
/// (`/proc`, `/sys`, `/dev`) are skipped.
///
/// `err` is the `errno` captured right after the syscall.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_note_open(path: *const c_char, err: c_int) {
    static ON: OnceLock<bool> = OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("PENGUDROID_TRACE_OPEN").is_some()) {
        return;
    }
    if path.is_null() {
        return;
    }
    let p = CStr::from_ptr(path).to_string_lossy().into_owned();
    for skip in ["/proc/", "/sys/", "/dev/", "/tmp/pengudroid-art-"] {
        if p.starts_with(skip) {
            return;
        }
    }
    static SEEN: OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()));
    let mut guard = seen.lock().unwrap_or_else(|e| e.into_inner());
    if !guard.insert(p.clone()) {
        return;
    }
    eprintln!("[rootfs] open failed (errno {err}): {p}");
}

/// `RTLD_NEXT` — search after this object.
const RTLD_NEXT: *mut c_void = -1isize as *mut c_void;

/// Resolve the host implementation of `name`, or `None`.
pub(crate) unsafe fn next<T: Copy>(name: &[u8]) -> Option<T> {
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
type StatVfsFn = unsafe extern "C" fn(*const c_char, *mut c_void) -> c_int;
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

// Bionic code calls the 64-bit variants (`stat64`, `lstat64`, `fstat64`,
// `fstatat64`) rather than the plain names; libcore's `File.isFile()` and
// `File.exists()` go through `stat64`. Without these the call falls through to
// glibc's own `stat64`, which never sees the virtual-root rewrite, so every
// `/system/...` path looks absent and `FontListParser` rejects every font.

/// `stat64(path, buf)`.
#[no_mangle]
pub unsafe extern "C" fn stat64(path: *const c_char, buf: *mut c_void) -> c_int {
    match next::<StatFn>(b"stat64\0") {
        Some(f) => with_path(path, |p| f(p, buf)),
        None => -1,
    }
}

/// `lstat64(path, buf)`.
#[no_mangle]
pub unsafe extern "C" fn lstat64(path: *const c_char, buf: *mut c_void) -> c_int {
    match next::<StatFn>(b"lstat64\0") {
        Some(f) => with_path(path, |p| f(p, buf)),
        None => -1,
    }
}

/// `fstatat64(dirfd, path, buf, flags)`.
#[no_mangle]
pub unsafe extern "C" fn fstatat64(
    dirfd: c_int,
    path: *const c_char,
    buf: *mut c_void,
    flags: c_int,
) -> c_int {
    match next::<StatAtFn>(b"fstatat64\0") {
        Some(f) => with_path(path, |p| f(dirfd, p, buf, flags)),
        None => -1,
    }
}

/// `fstat64(fd, buf)` — no path, so nothing to rewrite; defined so the symbol
/// resolves through the shim like its siblings.
#[no_mangle]
pub unsafe extern "C" fn fstat64(fd: c_int, buf: *mut c_void) -> c_int {
    match next::<unsafe extern "C" fn(c_int, *mut c_void) -> c_int>(b"fstat64\0") {
        Some(f) => f(fd, buf),
        None => -1,
    }
}

/// `access(path, mode)`.
///
/// The binder devices are emulated in-process, so they never exist on the host
/// filesystem. `libhidlbase`'s `defaultServiceManager` gates the whole HIDL
/// transport on `access("/dev/hwbinder", R_OK|W_OK)`, so answering `ENOENT`
/// there makes every HIDL lookup fail with "Hardware service manager is not
/// running".
#[no_mangle]
pub unsafe extern "C" fn access(path: *const c_char, mode: c_int) -> c_int {
    if is_emulated_device(path) {
        return 0;
    }
    match next::<AccessFn>(b"access\0") {
        Some(f) => with_path(path, |p| f(p, mode)),
        None => -1,
    }
}

/// Is `path` one of the devices this shim provides itself?
unsafe fn is_emulated_device(path: *const c_char) -> bool {
    if path.is_null() {
        return false;
    }
    let bytes = std::ffi::CStr::from_ptr(path).to_bytes();
    crate::binder::is_binder_device(bytes) || bytes == b"/dev/ashmem"
}

/// `faccessat(dirfd, path, mode, flags)`.
#[no_mangle]
pub unsafe extern "C" fn faccessat(
    dirfd: c_int,
    path: *const c_char,
    mode: c_int,
    flags: c_int,
) -> c_int {
    if is_emulated_device(path) {
        return 0;
    }
    match next::<AccessAtFn>(b"faccessat\0") {
        Some(f) => with_path(path, |p| f(dirfd, p, mode, flags)),
        None => -1,
    }
}

fn virtual_arm64_path<'a>(root: &[u8], path: &'a [u8]) -> Option<&'a [u8]> {
    let path = path.strip_prefix(root)?;
    arm64_runtime_path(path).then_some(path)
}

fn virtual_runtime_path(path: &[u8]) -> Option<&[u8]> {
    let arm64 = arm64_root()?;
    if let Some(path) = virtual_arm64_path(arm64, path) { return Some(path); }
    // The config reader canonicalizes dir.system=/system as well as the
    // executable. Both must remain in the same virtual coordinate space.
    let path = path.strip_prefix(root()?)?;
    guest_path(path).then_some(path)
}

/// Linker config sections are selected by executable realpath. Returning the
/// host bundle prefix here makes `/system` fail to match `dir.system`, which
/// otherwise silently selects the host x86_64 library fallback.
unsafe fn with_link_output(
    buf: *mut c_char, size: usize, mut read: impl FnMut(*mut c_char, usize) -> isize,
) -> isize {
    if arm64_root().is_none() { return read(buf, size); }
    if buf.is_null() || size == 0 { return read(buf, size); }
    let mut link = [0u8; 4096 + 64];
    let len = read(link.as_mut_ptr().cast(), link.len());
    if len < 0 { return len; }
    if len as usize == link.len() { return read(buf, size); }
    let path = &link[..len as usize];
    let path = virtual_runtime_path(path).unwrap_or(path);
    let count = size.min(path.len());
    std::ptr::copy_nonoverlapping(path.as_ptr(), buf.cast(), count);
    count as isize
}

/// `readlink(path, buf, size)`.
#[no_mangle]
pub unsafe extern "C" fn readlink(
    path: *const c_char,
    buf: *mut c_char,
    size: usize,
) -> isize {
    match next::<ReadlinkFn>(b"readlink\0") {
        Some(f) => with_path(path, |p| with_link_output(buf, size, |out, size| f(p, out, size))),
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
        Some(f) => with_path(path, |p| with_link_output(buf, size, |out, size| f(dirfd, p, out, size))),
        None => -1,
    }
}

/// `statvfs(path, buf)`.
///
/// `StatFs("/data")` is how the framework asks for the free space on the data
/// partition (`SQLiteGlobal.getDefaultPageSize` does it on every database
/// open). Without the rewrite the path does not exist on the host and the
/// framework throws "Invalid path: /data".
#[no_mangle]
pub unsafe extern "C" fn statvfs(path: *const c_char, buf: *mut c_void) -> c_int {
    match next::<StatVfsFn>(b"statvfs\0") {
        Some(f) => with_path(path, |p| f(p, buf)),
        None => -1,
    }
}

/// `statvfs64(path, buf)`.
#[no_mangle]
pub unsafe extern "C" fn statvfs64(path: *const c_char, buf: *mut c_void) -> c_int {
    match next::<StatVfsFn>(b"statvfs64\0") {
        Some(f) => with_path(path, |p| f(p, buf)),
        None => -1,
    }
}

/// `statfs(path, buf)` — the raw syscall wrapper, which `Os.statfs` uses.
#[no_mangle]
pub unsafe extern "C" fn statfs(path: *const c_char, buf: *mut c_void) -> c_int {
    match next::<StatVfsFn>(b"statfs\0") {
        Some(f) => with_path(path, |p| f(p, buf)),
        None => -1,
    }
}

/// `statfs64(path, buf)`.
#[no_mangle]
pub unsafe extern "C" fn statfs64(path: *const c_char, buf: *mut c_void) -> c_int {
    match next::<StatVfsFn>(b"statfs64\0") {
        Some(f) => with_path(path, |p| f(p, buf)),
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
        Some(f) => with_path(path, |p| {
            let out = f(p, resolved);
            if !out.is_null() {
                {
                    let host = CStr::from_ptr(out).to_bytes();
                    if let Some(virtual_path) = virtual_runtime_path(host) {
                        let len = virtual_path.len();
                        std::ptr::copy(virtual_path.as_ptr(), out.cast(), len);
                        *out.add(len) = 0;
                    }
                }
            }
            out
        }),
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
    fn arm64_overlay_does_not_replace_host_framework_or_libc() {
        for path in [
            b"/system/bin/arm64/linker64".as_slice(),
            b"/system/lib64/arm64/libc.so", b"/system/lib64/arm64/bootstrap/libdl.so",
            b"/system/etc/ld.config.arm64.txt", b"/system/bin/berberis",
            b"/system/lib64/berberis/arm64/libc.so",
        ] {
            let expected = [b"/opt/guest".as_slice(), path].concat();
            assert_eq!(map_with_roots(Some(ROOT), Some(b"/opt/guest"), path), Some(expected));
        }
        for path in [
            b"/system/lib64/libc.so".as_slice(), b"/system/bin/linker64",
            b"/system/framework/framework.jar", b"/apex/com.android.art/lib64/libart.so",
            b"/system/bin/arm64-other", b"/system/lib64/arm640/libc.so",
            b"/system/etc/ld.config.arm64.txt.bak",
        ] {
            assert_eq!(map_with_roots(Some(ROOT), Some(b"/opt/guest"), path), map_with_root(ROOT, path));
        }
        assert!(map_with_roots(Some(ROOT), Some(b"/opt/guest"), b"/opt/guest/system/lib64/arm64/libc.so").is_none());
    }

    #[test]
    fn only_arm64_bundle_realpaths_become_virtual_again() {
        assert_eq!(virtual_arm64_path(b"/opt/guest", b"/opt/guest/system/bin/arm64/app_process64"), Some(b"/system/bin/arm64/app_process64".as_slice()));
        assert_eq!(virtual_arm64_path(b"/opt/guest", b"/opt/guest/system/lib64/arm64/libc.so"), Some(b"/system/lib64/arm64/libc.so".as_slice()));
        for path in [b"/opt/guest/system/lib64/libc.so".as_slice(), b"/opt/guest-other/system/bin/arm64/linker64", b"/usr/bin/bash", b"/opt/guest"] {
            assert!(virtual_arm64_path(b"/opt/guest", path).is_none());
        }
    }

    #[test]
    fn arm64_overlay_is_optional_and_never_falls_back_for_missing_files() {
        let path = b"/system/lib64/arm64/does-not-exist.so";
        assert_eq!(map_with_roots(Some(ROOT), None, path), map_with_root(ROOT, path));
        assert_eq!(map_with_roots(None, Some(b"/opt/guest"), path), Some(b"/opt/guest/system/lib64/arm64/does-not-exist.so".to_vec()));
        assert!(map_with_roots(None, Some(b"/opt/guest"), b"/system/lib64/libart.so").is_none());
    }

    #[test]
    fn an_unset_or_root_root_disables_mapping() {
        assert!(map_with_root(b"", b"/system/x").is_none());
        assert!(map_with_root(b"/", b"/system/x").is_none());
    }
}
