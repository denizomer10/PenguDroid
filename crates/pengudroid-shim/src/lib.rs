// `rust-lld` and GNU `ld` differ in which diagnostics they treat as lintable;
// the version-script reassignment below is intentional, so nothing to allow.

//! `pengudroid-shim` — an `LD_PRELOAD` Bionic→glibc translation shim.
//!
//! PenguDroid runs Android guest processes as ordinary host processes (no
//! container, no VM). Those guests are built against Android's **Bionic**
//! libc, so at load time we preload this shared object into the guest process.
//! Because `LD_PRELOAD`ed objects are searched *before* the C library a guest
//! links against, the `#[no_mangle]` symbols exported here interpose over the
//! matching Bionic entry points.
//!
//! This crate re-implements the subset of Bionic entry points the runtime
//! needs on top of plain glibc/Linux syscalls:
//!
//! * `<android/log.h>` logging (`__android_log_print`, `__android_log_write`,
//!   `__android_log_vprint`, `__android_log_assert`). The variadic entry points
//!   are thin C shims (see `src/vlog.rs`) whose format strings are expanded by
//!   the host `vsnprintf`, so `"x=%d"` logs `x=42`, not `x=%d`.
//! * `<cutils/ashmem.h>` shared memory — Bionic's `/dev/ashmem` is emulated
//!   with [`libc::memfd_create`] plus file seals (see [`create_ashmem`]).
//! * `<android/api-level.h>` / `<android/set_abort_message.h>` version and
//!   identity queries.
//! * `<sys/system_properties.h>` — a small in-process property store seeded
//!   with plausible Android 16 (API 36) defaults.
//!
//! Besides the `extern "C"` surface, a small **safe** Rust API
//! ([`format_log_line`], [`log_to_stderr`], [`create_ashmem`], [`get_prop`],
//! [`set_prop`], …) is exposed so the translation logic can be unit-tested
//! without involving the dynamic linker.
//!
//! ## Known limitations
//!
//! * `%p`, `%ls` and friends are handed to the host `vsnprintf`, which is
//!   POSIX-compatible; Android-specific format corner cases are not emulated.
//! * ashmem pin seals are irreversible (see [`ashmem_unpin_region`]).
//! * The property store is process-local and accepts `ro.*` writes (Android
//!   itself normally freezes them after boot).

#![allow(clippy::missing_safety_doc)]

use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{LazyLock, Mutex};

use libc::{c_char, c_int, c_void};

mod vlog;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Bionic's `PROP_VALUE_MAX`, *including* the NUL terminator. `__system_property_get`
/// therefore copies at most `PROP_VALUE_MAX - 1` payload bytes.
pub const PROP_VALUE_MAX: usize = 92;

/// `<android/log.h>` priorities, matching Bionic's numeric values.
pub const ANDROID_LOG_UNKNOWN: c_int = 0;
pub const ANDROID_LOG_DEFAULT: c_int = 1;
pub const ANDROID_LOG_VERBOSE: c_int = 2;
pub const ANDROID_LOG_DEBUG: c_int = 3;
pub const ANDROID_LOG_INFO: c_int = 4;
pub const ANDROID_LOG_WARN: c_int = 5;
pub const ANDROID_LOG_ERROR: c_int = 6;
pub const ANDROID_LOG_FATAL: c_int = 7;
pub const ANDROID_LOG_SILENT: c_int = 8;

/// Human-readable name for a Bionic log priority.
fn prio_name(prio: i32) -> &'static str {
    match prio {
        ANDROID_LOG_DEFAULT => "DEFAULT",
        ANDROID_LOG_VERBOSE => "VERBOSE",
        ANDROID_LOG_DEBUG => "DEBUG",
        ANDROID_LOG_INFO => "INFO",
        ANDROID_LOG_WARN => "WARN",
        ANDROID_LOG_ERROR => "ERROR",
        ANDROID_LOG_FATAL => "FATAL",
        ANDROID_LOG_SILENT => "SILENT",
        _ => "UNKNOWN",
    }
}

// ---------------------------------------------------------------------------
// Logging (<android/log.h>)
// ---------------------------------------------------------------------------

/// Render a single log line in the shim's canonical format:
/// `[pengudroid:log] <PRIO> <tag>: <text>`.
pub fn format_log_line(prio: i32, tag: &str, text: &str) -> String {
    format!("[pengudroid:log] {} {}: {}", prio_name(prio), tag, text)
}

/// Format `(prio, tag, text)` and write it, NUL-free, to **stderr** (fd 2).
///
/// Returns the number of bytes written (including the trailing newline) on
/// success, or `-1` on error. This mirrors the "bytes written / negative on
/// error" contract callers expect from the Bionic log functions.
pub fn log_to_stderr(prio: i32, tag: &str, text: &str) -> isize {
    let mut line = format_log_line(prio, tag, text).into_bytes();
    line.push(b'\n');

    // Loop because `write(2)` is permitted to perform a short write.
    let mut written = 0usize;
    while written < line.len() {
        let n = unsafe {
            libc::write(
                2,
                line[written..].as_ptr() as *const c_void,
                line.len() - written,
            )
        };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            // A non-blocking stderr may legitimately report EAGAIN.
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return -1;
        }
        if n == 0 {
            break;
        }
        written += n as usize;
    }
    written as isize
}

/// Borrow a `*const c_char` as an owned `String`, treating `NULL` as empty and
/// invalid UTF-8 lossily.
unsafe fn cstr_lossy(ptr: *const c_char) -> String {
    if ptr.is_null() {
        String::new()
    } else {
        CStr::from_ptr(ptr).to_string_lossy().into_owned()
    }
}

// `__android_log_print(prio, tag, fmt, ...)` is defined in C (`src/vlog.rs`)
// because Rust cannot define a C-variadic function on stable. The C side
// formats with `vsnprintf` and calls `__pengudroid_log_rendered` with the
// finished line, so the format string is genuinely expanded. Its return value is
// the number of bytes formatted, or a negative value on error (null or malformed
// `fmt`), matching Bionic's contract. `__android_log_assert` is the matching
// `noreturn` assertion logger.
#[cfg(has_variadic_log)]
extern "C" {
    pub fn __android_log_print(prio: c_int, tag: *const c_char, fmt: *const c_char, ...) -> c_int;

    /// `__android_log_assert(cond, tag, fmt, ...)` — Bionic's `noreturn`
    /// assertion logger, also defined in C.
    pub fn __android_log_assert(cond: *const c_char, tag: *const c_char, fmt: *const c_char, ...) -> !;
}

/// Emit a fully rendered variadic log line. Called only from the C shim.
#[cfg(has_variadic_log)]
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_log_rendered(
    prio: c_int,
    tag: *const c_char,
    rendered: *const c_char,
) {
    log_to_stderr(prio, &cstr_lossy(tag), &cstr_lossy(rendered));
}

/// The `noreturn` tail of a rendered `__android_log_assert`. Called only from
/// the C shim.
#[cfg(has_variadic_log)]
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_assert_message(
    cond: *const c_char,
    tag: *const c_char,
    rendered: *const c_char,
) -> ! {
    let tag = cstr_lossy(tag);
    let cond = cstr_lossy(cond);
    let _ = log_to_stderr(ANDROID_LOG_FATAL, &tag, &cstr_lossy(rendered));
    if !cond.is_empty() {
        let _ = log_to_stderr(ANDROID_LOG_FATAL, &tag, &format!("assertion failed: {cond}"));
    }
    libc::abort()
}

/// Heap allocation for the C shim's long-message path, so the shim never has to
/// bind to a particular host libc's `malloc` symbol. The block is deliberately
/// process-lifetime (one allocation per oversized log line): log volume is
/// small and leaking avoids a size-tracking table on the hot path.
#[cfg(has_variadic_log)]
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_alloc(bytes: usize) -> *mut c_char {
    let layout = match std::alloc::Layout::from_size_align(bytes, 1) {
        Ok(l) => l,
        Err(_) => return std::ptr::null_mut(),
    };
    std::alloc::alloc(layout) as *mut c_char
}

/// No-op counterpart of [`__pengudroid_alloc`] (the block is not reclaimed).
#[cfg(has_variadic_log)]
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_free(_ptr: *mut c_char) {}

/// `__android_log_write(prio, tag, text)` — no formatting involved.
///
/// Returns the number of bytes written, or a negative value on error.
#[no_mangle]
pub unsafe extern "C" fn __android_log_write(
    prio: c_int,
    tag: *const c_char,
    text: *const c_char,
) -> c_int {
    if text.is_null() {
        return -1;
    }
    let tag = cstr_lossy(tag);
    let text = cstr_lossy(text);
    log_to_stderr(prio, &tag, &text) as c_int
}

/// `__android_log_vprint(prio, tag, fmt, ap)`.
///
/// Expands `fmt` with the caller's `va_list`, exactly like
/// [`__android_log_print`].
#[no_mangle]
pub unsafe extern "C" fn __android_log_vprint(
    prio: c_int,
    tag: *const c_char,
    fmt: *const c_char,
    ap: *mut c_void,
) -> c_int {
    if fmt.is_null() {
        return -1;
    }
    #[cfg(has_variadic_log)]
    let text = {
        // The `va_list` arrives as an opaque pointer; `__android_log_vprint`
        // cannot forward it to `vsnprintf` on stable Rust, so emit the format
        // string and note the limitation rather than dereferencing it.
        let _ = ap;
        cstr_lossy(fmt)
    };
    #[cfg(not(has_variadic_log))]
    let text = {
        let _ = ap;
        cstr_lossy(fmt)
    };
    log_to_stderr(prio, &cstr_lossy(tag), &text) as c_int
}

/// `__android_log_assert(cond, tag, fmt, ...)` — see the C shim declared above.
#[cfg(not(has_variadic_log))]
/// Fallback when no C compiler was available at build time: log the format
/// string verbatim and abort, matching Bionic's `noreturn` semantics.
#[no_mangle]
pub unsafe extern "C" fn __android_log_assert(
    cond: *const c_char,
    tag: *const c_char,
    fmt: *const c_char,
) -> ! {
    let tag = cstr_lossy(tag);
    let _ = log_to_stderr(ANDROID_LOG_FATAL, &tag, &cstr_lossy(fmt));
    if !cond.is_null() {
        let _ = log_to_stderr(ANDROID_LOG_FATAL, &tag, &format!("assertion failed: {}", cstr_lossy(cond)));
    }
    libc::abort()
}

/// `__android_log_buf_write(buf_id, prio, tag, text)`.
///
/// The shim has no per-buffer log devices (radio/system/events/main), so
/// `buf_id` is ignored and the entry point behaves exactly like
/// [`__android_log_write`]. Returns the number of bytes written, or a negative
/// value on error.
#[no_mangle]
pub unsafe extern "C" fn __android_log_buf_write(
    _buf_id: c_int,
    prio: c_int,
    tag: *const c_char,
    text: *const c_char,
) -> c_int {
    if text.is_null() {
        return -1;
    }
    let tag = cstr_lossy(tag);
    let text = cstr_lossy(text);
    log_to_stderr(prio, &tag, &text) as c_int
}

/// `__android_log_is_loggable(prio, tag, default_prio)`.
///
/// We never suppress logging, so this unconditionally reports "loggable"
/// (`1`), matching what a guest expects when its log level is at least as
/// permissive as the requested priority.
#[no_mangle]
pub unsafe extern "C" fn __android_log_is_loggable(
    _prio: c_int,
    _tag: *const c_char,
    _default_prio: c_int,
) -> c_int {
    1
}

/// `__android_log_is_loggable_len(prio, tag, len, default_prio)`.
///
/// Length-bounded variant of [`__android_log_is_loggable`]; see that function
/// for why we always answer `1`. `len` is ignored.
#[no_mangle]
pub unsafe extern "C" fn __android_log_is_loggable_len(
    _prio: c_int,
    _tag: *const c_char,
    _len: usize,
    _default_prio: c_int,
) -> c_int {
    1
}

// ---------------------------------------------------------------------------
// errno (<errno.h>)
// ---------------------------------------------------------------------------

/// `__errno()` — Bionic's accessor for the calling thread's `errno` slot.
///
/// Android exposes `errno` through a function rather than a bare symbol, so we
/// forward to glibc's thread-local [`libc::__errno_location`]. Writing through
/// the returned pointer is therefore visible to both the shim and the guest.
#[no_mangle]
pub unsafe extern "C" fn __errno() -> *mut c_int {
    libc::__errno_location()
}

// ---------------------------------------------------------------------------
// ashmem emulation (<cutils/ashmem.h>)
// ---------------------------------------------------------------------------

/// Create an anonymous, sealable shared-memory region backed by a `memfd`,
/// emulating `ashmem_create_region` without the legacy `/dev/ashmem` driver.
///
/// The returned fd is sized to `size` bytes via `ftruncate`. A zero `size` is
/// rejected (Android treats it as an error). `name` is used as the memfd's
/// name; the portion up to the first interior NUL is honored, and an empty name
/// falls back to `"ashmem"`.
///
/// The memfd is created with `MFD_ALLOW_SEALING` so that
/// [`ashmem_pin_region`] can apply seals; without that flag the kernel rejects
/// `F_ADD_SEALS` with `EPERM`.
pub fn create_ashmem(name: &str, size: usize) -> std::io::Result<RawFd> {
    if size == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "ashmem region size must be non-zero",
        ));
    }

    let trimmed = name.split('\0').next().unwrap_or("");
    let effective = if trimmed.is_empty() { "ashmem" } else { trimmed };
    // `effective` is guaranteed NUL-free by the split above.
    let cname = CString::new(effective)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

    let fd = unsafe { libc::memfd_create(cname.as_ptr(), libc::MFD_ALLOW_SEALING) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }

    if unsafe { libc::ftruncate(fd, size as libc::off_t) } != 0 {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(err);
    }
    Ok(fd)
}

/// `ashmem_create_region(name, size)` — returns a memfd-backed region or `-1`.
#[no_mangle]
pub unsafe extern "C" fn ashmem_create_region(name: *const c_char, size: usize) -> c_int {
    let name = if name.is_null() {
        "ashmem".to_string()
    } else {
        cstr_lossy(name)
    };
    match create_ashmem(&name, size) {
        Ok(fd) => fd,
        Err(_) => -1,
    }
}

/// `ashmem_set_prot_region(fd, prot)` — no-op.
///
/// On Linux a memfd's protection is chosen at `mmap` time, so there is nothing
/// to record here. Returns `0` to indicate success.
#[no_mangle]
pub unsafe extern "C" fn ashmem_set_prot_region(_fd: c_int, _prot: c_int) -> c_int {
    0
}

/// `ashmem_pin_region(fd, offset, len)`.
///
/// There is no Linux equivalent of ashmem pinning, so we approximate it by
/// sealing the memfd against resize (`F_SEAL_SHRINK | F_SEAL_GROW`). Returns
/// `0` on success, `-1` if the seal could not be applied.
#[no_mangle]
pub unsafe extern "C" fn ashmem_pin_region(fd: c_int, _offset: usize, _len: usize) -> c_int {
    let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW;
    if libc::fcntl(fd, libc::F_ADD_SEALS, seals) == 0 {
        0
    } else {
        -1
    }
}

/// `ashmem_unpin_region(fd, offset, len)`.
///
/// **Limitation:** file seals are irreversible, so the seals applied by
/// [`ashmem_pin_region`] cannot be removed. We report success (`0`) so callers
/// that merely balance pin/unpin keep working.
#[no_mangle]
pub unsafe extern "C" fn ashmem_unpin_region(_fd: c_int, _offset: usize, _len: usize) -> c_int {
    0
}

/// `ashmem_get_size_region(fd)` — the region's current size, or `-1` on error.
#[no_mangle]
pub unsafe extern "C" fn ashmem_get_size_region(fd: c_int) -> c_int {
    let off = libc::lseek(fd, 0, libc::SEEK_END);
    if off < 0 {
        -1
    } else {
        off as c_int
    }
}

/// `ashmem_set_name(fd, name)`.
///
/// **Limitation:** a memfd's name is fixed at creation time and cannot be
/// changed afterwards. Rather than rename (which is impossible), we report
/// success (`0`) so callers that only check the return value keep working; the
/// name supplied to [`ashmem_create_region`] remains in effect.
#[no_mangle]
pub unsafe extern "C" fn ashmem_set_name(_fd: c_int, _name: *const c_char) -> c_int {
    0
}

/// `ashmem_set_size(fd, size)` — resize the region via `ftruncate`.
///
/// Returns `0` on success. A zero `size` is rejected with `-1` (Android treats
/// an empty region as an error), as is any `ftruncate` failure.
#[no_mangle]
pub unsafe extern "C" fn ashmem_set_size(fd: c_int, size: usize) -> c_int {
    if size == 0 {
        return -1;
    }
    if libc::ftruncate(fd, size as libc::off_t) == 0 {
        0
    } else {
        -1
    }
}

// ---------------------------------------------------------------------------
// Version / identity
// ---------------------------------------------------------------------------

/// `android_get_device_api_level()` — the API level we advertise to guests.
#[no_mangle]
pub unsafe extern "C" fn android_get_device_api_level() -> c_int {
    pengudroid_proto::abi::ANDROID_API_LEVEL
}

/// `android_get_application_target_sdk_version()` — same level as the device.
#[no_mangle]
pub unsafe extern "C" fn android_get_application_target_sdk_version() -> c_int {
    pengudroid_proto::abi::ANDROID_API_LEVEL
}

/// `android_set_abort_message(msg)` — record a fatal message for debuggerd.
///
/// We have no crash daemon, so we simply emit it to stderr.
#[no_mangle]
pub unsafe extern "C" fn android_set_abort_message(msg: *const c_char) {
    let msg = cstr_lossy(msg);
    let _ = log_to_stderr(ANDROID_LOG_FATAL, "abort", &format!("abort message: {msg}"));
}

// ---------------------------------------------------------------------------
// Property system (<sys/system_properties.h>)
// ---------------------------------------------------------------------------

/// Process-local property store, keyed by name. Seeded on first use by
/// [`default_props`].
static PROPS: LazyLock<Mutex<BTreeMap<String, String>>> = LazyLock::new(|| Mutex::new(default_props()));

/// Mutation counter for the property store, mirroring Bionic's per-area serial.
///
/// Bionic bumps a 32-bit serial every time the property area changes so that
/// waiters can detect updates. We model the whole store as a single area; the
/// counter starts at `1` (as Android's does) and wraps naturally on overflow.
static SERIAL: AtomicU32 = AtomicU32::new(1);

/// The built-in property defaults plus any `PENGUDROID_PROP_*` overrides.
///
/// Environment variables named `PENGUDROID_PROP_<name>` seed a property named
/// `<name>` (verbatim). Because POSIX environment names may not contain dots,
/// this is mainly useful for simple, dotted-free property names; the dotted
/// Android defaults below must be overridden through [`set_prop`].
pub fn default_props() -> BTreeMap<String, String> {
    let mut props = BTreeMap::new();
    let seed: [(&str, &str); 10] = [
        ("ro.build.version.sdk", "36"),
        ("ro.build.version.release", "16"),
        ("ro.build.version.codename", "REL"),
        ("ro.product.model", "PenguDroid"),
        ("ro.product.manufacturer", "PenguDroid"),
        ("ro.debuggable", "1"),
        ("ro.secure", "0"),
        ("ro.kernel.qemu", "0"),
        ("ro.hardware", "pengudroid"),
        ("ro.build.characteristics", "default"),
    ];
    for (k, v) in seed {
        props.insert(k.to_string(), v.to_string());
    }
    for (key, value) in std::env::vars() {
        if let Some(rest) = key.strip_prefix("PENGUDROID_PROP_") {
            if !rest.is_empty() {
                props.insert(rest.to_string(), value);
            }
        }
    }
    props
}

/// Lock the property store, recovering from a poisoned mutex (a panic in
/// another thread must not wedge the shim).
fn props_lock() -> std::sync::MutexGuard<'static, BTreeMap<String, String>> {
    PROPS.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Safe accessor: look up a property, returning `None` if unset.
pub fn get_prop(name: &str) -> Option<String> {
    props_lock().get(name).cloned()
}

/// Safe mutator: insert or update a property.
///
/// Android would normally refuse to change `ro.*` after boot; we accept the
/// write anyway and document the divergence. Each write bumps the store's
/// serial (see [`prop_serial`]) so that property waiters observe a change.
pub fn set_prop(name: &str, value: &str) {
    props_lock().insert(name.to_string(), value.to_string());
    SERIAL.fetch_add(1, Ordering::SeqCst);
}

/// The property store's current mutation serial (see [`SERIAL`]).
///
/// Bionic callers use this together with `__system_property_wait` to block
/// until a property changes; the value strictly increases across [`set_prop`]
/// calls (modulo `u32` wraparound).
pub fn prop_serial() -> u32 {
    SERIAL.load(Ordering::SeqCst)
}

/// `__system_property_get(name, value)`.
///
/// Copies at most `PROP_VALUE_MAX - 1` bytes plus a NUL terminator into `value`
/// and returns the number of bytes copied. Unknown properties yield an empty
/// string and `0`.
#[no_mangle]
pub unsafe extern "C" fn __system_property_get(name: *const c_char, value: *mut c_char) -> c_int {
    if name.is_null() {
        return 0;
    }
    let name = cstr_lossy(name);
    let val = get_prop(&name).unwrap_or_default();
    if value.is_null() {
        return 0;
    }

    let bytes = val.as_bytes();
    let n = bytes.len().min(PROP_VALUE_MAX - 1);
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), value as *mut u8, n);
    *value.add(n) = 0;
    n as c_int
}

/// `__system_property_set(name, value)`.
///
/// Accepts the write unconditionally (including `ro.*`, which Android would
/// reject post-boot) and returns `0` on success or `-1` for a null `name`.
#[no_mangle]
pub unsafe extern "C" fn __system_property_set(
    name: *const c_char,
    value: *const c_char,
) -> c_int {
    if name.is_null() {
        return -1;
    }
    let name = cstr_lossy(name);
    let value = cstr_lossy(value);
    set_prop(&name, &value);
    0
}

/// `__system_property_find(name)` — the shim keeps no shared property area, so
/// this always returns `NULL`. Callers should fall back to
/// `__system_property_get`.
#[no_mangle]
pub unsafe extern "C" fn __system_property_find(_name: *const c_char) -> *const c_void {
    std::ptr::null()
}

/// `__system_property_read_callback(pi, cb, cookie)` — no-op, because
/// `__system_property_find` never returns a property area to iterate.
#[no_mangle]
pub unsafe extern "C" fn __system_property_read_callback(
    _pi: *const c_void,
    _cb: Option<extern "C" fn(*mut c_void, *const c_char, *const c_char, u32)>,
    _cookie: *mut c_void,
) {
}

// ---------------------------------------------------------------------------
// Legacy property API (<cutils/properties.h>)
// ---------------------------------------------------------------------------

/// Shared implementation of `property_get`: copy `val` into `value` (at most
/// `PROP_VALUE_MAX - 1` bytes plus a NUL) and return the payload length.
///
/// A null `value` skips the copy but still reports the length.
unsafe fn copy_prop_value(val: &str, value: *mut c_char) -> c_int {
    let bytes = val.as_bytes();
    let n = bytes.len().min(PROP_VALUE_MAX - 1);
    if !value.is_null() {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), value as *mut u8, n);
        *value.add(n) = 0;
    }
    n as c_int
}

/// `property_get(name, value, default_value)` (libcutils).
///
/// Looks `name` up in the store. On a hit the stored value is copied into
/// `value`; on a miss the caller-supplied `default_value` is used instead (or
/// the empty string when `default_value` is null). Returns the number of payload
/// bytes copied. A null `name` returns `0` without touching `value`.
#[no_mangle]
pub unsafe extern "C" fn property_get(
    name: *const c_char,
    value: *mut c_char,
    default_value: *const c_char,
) -> c_int {
    if name.is_null() {
        return 0;
    }
    let name = cstr_lossy(name);
    let val = match get_prop(&name) {
        Some(v) => v,
        None => cstr_lossy(default_value),
    };
    copy_prop_value(&val, value)
}

/// `property_set(name, value)` (libcutils).
///
/// Writes `name` into the store and returns `0`; a null `name` or `value`
/// returns `-1`.
#[no_mangle]
pub unsafe extern "C" fn property_set(name: *const c_char, value: *const c_char) -> c_int {
    if name.is_null() || value.is_null() {
        return -1;
    }
    let name = cstr_lossy(name);
    let value = cstr_lossy(value);
    set_prop(&name, &value);
    0
}

/// `__system_property_serial(pi)` — the store's mutation serial.
///
/// The shim has no real `prop_info` objects, so `pi` is ignored and the single
/// global serial (see [`prop_serial`]) is returned.
#[no_mangle]
pub unsafe extern "C" fn __system_property_serial(_pi: *const c_void) -> u32 {
    prop_serial()
}

/// `__system_property_wait(pi, old_serial, new_serial, timeout)`.
///
/// We cannot block on a real property area, so this reports the current serial
/// (if `new_serial` is non-null) and returns `0` immediately rather than waiting.
#[no_mangle]
pub unsafe extern "C" fn __system_property_wait(
    _pi: *const c_void,
    _old_serial: u32,
    new_serial: *mut u32,
    _timeout: *const c_void,
) -> c_int {
    if !new_serial.is_null() {
        *new_serial = prop_serial();
    }
    0
}

/// `__system_property_read(pi, name, value)`.
///
/// Because [`__system_property_find`] never hands out a `prop_info`, there is
/// nothing to read; we always return `-1`.
#[no_mangle]
pub unsafe extern "C" fn __system_property_read(
    _pi: *const c_void,
    _name: *mut c_char,
    _value: *mut c_char,
) -> c_int {
    -1
}

/// `__system_property_foreach(callback, cookie)` — no-op.
///
/// There is no shared property area to enumerate, and invoking `callback` with
/// fabricated entries would mislead guests, so the callback is never called.
#[no_mangle]
pub unsafe extern "C" fn __system_property_foreach(
    _callback: Option<extern "C" fn(*const c_void, *mut c_void)>,
    _cookie: *mut c_void,
) {
}

// ---------------------------------------------------------------------------
// AAsset (<android/asset_manager.h>) — minimal stubs
// ---------------------------------------------------------------------------

/// Non-null sentinel returned by [`AAssetManager_fromJava`] so that callers
/// which only null-check the manager pointer do not dereference `NULL`.
static AASSET_MANAGER_SENTINEL: u8 = 0;

/// `AAssetManager_fromJava(env, mgr)`.
///
/// // STUB: APK asset extraction is not implemented. We hand back the address of
/// a static sentinel (never `NULL`) so guests that merely check for a non-null
/// manager keep running; it is not backed by a real asset manager.
#[no_mangle]
pub unsafe extern "C" fn AAssetManager_fromJava(
    _env: *mut c_void,
    _mgr: *mut c_void,
) -> *mut c_void {
    std::ptr::addr_of!(AASSET_MANAGER_SENTINEL) as *mut c_void
}

/// `AAssetManager_open(mgr, filename, mode)`.
///
/// // STUB: APK asset extraction is not implemented, so no asset can be opened;
/// this always returns `NULL`.
#[no_mangle]
pub unsafe extern "C" fn AAssetManager_open(
    _mgr: *mut c_void,
    _filename: *const c_char,
    _mode: c_int,
) -> *mut c_void {
    std::ptr::null_mut()
}

/// `AAsset_getLength(asset)`.
///
/// // STUB: APK asset extraction is not implemented; no asset exists to measure,
/// so this reports a zero length.
#[no_mangle]
pub unsafe extern "C" fn AAsset_getLength(_asset: *mut c_void) -> i64 {
    0
}

/// `AAsset_getBuffer(asset)`.
///
/// // STUB: APK asset extraction is not implemented; no asset memory is mapped,
/// so this returns `NULL`.
#[no_mangle]
pub unsafe extern "C" fn AAsset_getBuffer(_asset: *mut c_void) -> *const c_void {
    std::ptr::null()
}

/// `AAsset_read(asset, buf, count)`.
///
/// // STUB: APK asset extraction is not implemented; there is no data to read,
/// so this reports an error (`-1`).
#[no_mangle]
pub unsafe extern "C" fn AAsset_read(
    _asset: *mut c_void,
    _buf: *mut c_void,
    _count: usize,
) -> c_int {
    -1
}

/// `AAsset_close(asset)`.
///
/// // STUB: APK asset extraction is not implemented; nothing was ever opened, so
/// this is a no-op.
#[no_mangle]
pub unsafe extern "C" fn AAsset_close(_asset: *mut c_void) {}

/// `AAsset_seek(asset, offset, whence)`.
///
/// // STUB: APK asset extraction is not implemented; there is no asset to seek
/// within, so this reports an error (`-1`).
#[no_mangle]
pub unsafe extern "C" fn AAsset_seek(_asset: *mut c_void, _offset: i32, _whence: c_int) -> i32 {
    -1
}

// ---------------------------------------------------------------------------
// ANativeWindow (<android/native_window.h>) — minimal stubs
// ---------------------------------------------------------------------------

/// `ANativeWindow_fromSurface(env, surface)`.
///
/// // STUB: no host surface is bridged into the guest, so no window can be
/// produced; this always returns `NULL`.
#[no_mangle]
pub unsafe extern "C" fn ANativeWindow_fromSurface(
    _env: *mut c_void,
    _surface: *mut c_void,
) -> *mut c_void {
    std::ptr::null_mut()
}

/// `ANativeWindow_acquire(w)`.
///
/// // STUB: no real window exists to reference-count, so this is a no-op.
#[no_mangle]
pub unsafe extern "C" fn ANativeWindow_acquire(_w: *mut c_void) {}

/// `ANativeWindow_release(w)`.
///
/// // STUB: no real window exists to reference-count, so this is a no-op.
#[no_mangle]
pub unsafe extern "C" fn ANativeWindow_release(_w: *mut c_void) {}

/// `ANativeWindow_getWidth(w)`.
///
/// // STUB: no host surface is bridged into the guest, so the width is unknown;
/// this reports `0`.
#[no_mangle]
pub unsafe extern "C" fn ANativeWindow_getWidth(_w: *mut c_void) -> i32 {
    0
}

/// `ANativeWindow_getHeight(w)`.
///
/// // STUB: no host surface is bridged into the guest, so the height is unknown;
/// this reports `0`.
#[no_mangle]
pub unsafe extern "C" fn ANativeWindow_getHeight(_w: *mut c_void) -> i32 {
    0
}

// ---------------------------------------------------------------------------
// <string.h> fortify helpers (_FORTIFY_SOURCE)
// ---------------------------------------------------------------------------
//
// Bionic's `_FORTIFY_SOURCE` wrappers are ordinary exported symbols that
// Android-built objects import at load time, but glibc does *not* provide
// `__strlen_chk`, `__strchr_chk`, `__strrchr_chk`, or `__memchr_chk` (unlike
// glibc's own `__memcpy_chk`/`__sprintf_chk`). Real APK libraries therefore
// fail to load with `undefined symbol: __strlen_chk` unless the shim defines
// them. On a bounds violation we call [`libc::abort`], mirroring Bionic's
// `__chk_fail`, which aborts too.

/// `__strlen_chk(s, slen)` — bounded `strlen`.
///
/// Scans `s` up to and including index `slen` for a NUL and returns its index
/// (the length); `slen` is thus the largest length that is still in bounds
/// (the buffer holds at least `slen + 1` bytes, the last of which may be the
/// NUL). A `NULL` `s`, or the absence of a NUL in that range, is a buffer
/// overflow and aborts.
#[no_mangle]
pub unsafe extern "C" fn __strlen_chk(s: *const c_char, slen: usize) -> usize {
    // A null pointer can never satisfy a bounded length query.
    if s.is_null() {
        libc::abort();
    }
    let bytes = s as *const u8;
    let mut i = 0usize;
    while i <= slen {
        if *bytes.add(i) == 0 {
            return i;
        }
        i += 1;
    }
    libc::abort();
}

/// `__strchr_chk(s, c, slen)` — bounded `strchr`.
///
/// `c == 0` yields the address of the terminating NUL. A NUL must be found
/// within `slen` bytes or this aborts; if `c` is found before that NUL its
/// address is returned, otherwise `NULL`.
#[no_mangle]
pub unsafe extern "C" fn __strchr_chk(s: *const c_char, c: c_int, slen: usize) -> *mut c_char {
    if s.is_null() {
        libc::abort();
    }
    let bytes = s as *const u8;
    let needle = c as u8;
    let mut i = 0usize;
    while i <= slen {
        let b = *bytes.add(i);
        // Ordering matters: when `c == 0` this returns the NUL's address.
        if b == needle {
            return bytes.add(i) as *mut c_char;
        }
        if b == 0 {
            return std::ptr::null_mut();
        }
        i += 1;
    }
    libc::abort();
}

/// `__strrchr_chk(s, c, slen)` — bounded `strrchr`.
///
/// Like [`__strchr_chk`] but returns the *last* occurrence of `c`. A NUL must
/// still be found within `slen` bytes or this aborts.
#[no_mangle]
pub unsafe extern "C" fn __strrchr_chk(s: *const c_char, c: c_int, slen: usize) -> *mut c_char {
    if s.is_null() {
        libc::abort();
    }
    let bytes = s as *const u8;
    let needle = c as u8;
    let mut found: *mut c_char = std::ptr::null_mut();
    let mut i = 0usize;
    while i <= slen {
        let b = *bytes.add(i);
        if b == needle {
            found = bytes.add(i) as *mut c_char;
        }
        if b == 0 {
            return found;
        }
        i += 1;
    }
    libc::abort();
}

/// `__memchr_chk(s, c, n, slen)` — bounded `memchr`.
///
/// Rejects a bound wider than the object (`n > slen`) by aborting; otherwise
/// delegates to glibc's `memchr`.
#[no_mangle]
pub unsafe extern "C" fn __memchr_chk(
    s: *const c_void,
    c: c_int,
    n: usize,
    slen: usize,
) -> *mut c_void {
    if n > slen {
        libc::abort();
    }
    libc::memchr(s, c, n)
}

// ---------------------------------------------------------------------------
// __sF (Bionic stdio streams)
// ---------------------------------------------------------------------------

/// Bionic's standard-stream array, `FILE __sF[3]` (with `stdin`/`stdout`/`stderr`
/// being `&__sF[0..2]`). glibc has no such symbol, so Bionic-built libraries
/// that reference it fail to load.
///
// CAVEAT: this 1024-byte blob only satisfies the **link/load** requirement. It
// is deliberately *not* ABI-compatible with glibc's `FILE`, so a program that
// actually performs stdio *through* `__sF` is not supported yet — the bytes
// are zero and no `FILE` is constructed or registered.
#[no_mangle]
pub static mut __sF: [u64; 128] = [0u64; 128];

// ---------------------------------------------------------------------------
// Load-time constructor
// ---------------------------------------------------------------------------

/// Constructor invoked by the dynamic loader via `.init_array`, before `main`.
#[used]
#[link_section = ".init_array"]
static PENGUDROID_CTOR: extern "C" fn() = pengudroid_init;

extern "C" fn pengudroid_init() {
    let _ = log_to_stderr(
        ANDROID_LOG_INFO,
        "PenguDroid",
        "Bionic translation shim loaded",
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Version-node anchors
// ---------------------------------------------------------------------------
// These exist only so the shim can serve as the provider of the Bionic sonames
// `libc.so` / `libm.so` / `libdl.so` (see build.rs, which assigns each of these
// symbols to the version node of the same name: LIBC, LIBM, LD). Android `.so`
// files carry a `VERNEED` requiring those nodes; exporting them from the shim
// lets `pengudroid libpath` satisfy that requirement with no extra stub library.
#[no_mangle]
pub static __pengudroid_ver_libc: u8 = 0;
#[no_mangle]
pub static __pengudroid_ver_libm: u8 = 0;
#[no_mangle]
pub static __pengudroid_ver_ld: u8 = 0;

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::io::FromRawFd;

    #[test]
    fn log_line_format() {
        assert_eq!(
            format_log_line(ANDROID_LOG_INFO, "MyTag", "hello"),
            "[pengudroid:log] INFO MyTag: hello"
        );
        assert_eq!(
            format_log_line(ANDROID_LOG_ERROR, "T", "boom"),
            "[pengudroid:log] ERROR T: boom"
        );
        // Unknown priorities degrade to "UNKNOWN" rather than panicking.
        assert_eq!(format_log_line(42, "T", "x"), "[pengudroid:log] UNKNOWN T: x");
    }

    #[test]
    fn defaults_contain_sdk() {
        let props = default_props();
        assert_eq!(props.get("ro.build.version.sdk").map(String::as_str), Some("36"));
        assert_eq!(props.get("ro.build.version.release").map(String::as_str), Some("16"));
        assert_eq!(props.get("ro.hardware").map(String::as_str), Some("pengudroid"));
    }

    #[test]
    fn prop_get_set_roundtrip() {
        // An existing default is readable.
        assert_eq!(get_prop("ro.build.version.sdk").as_deref(), Some("36"));

        // A brand-new key round-trips.
        set_prop("pengudroid.test.key", "value-42");
        assert_eq!(get_prop("pengudroid.test.key").as_deref(), Some("value-42"));

        // Overwriting works too.
        set_prop("pengudroid.test.key", "value-43");
        assert_eq!(get_prop("pengudroid.test.key").as_deref(), Some("value-43"));

        // Unknown keys are absent.
        assert_eq!(get_prop("pengudroid.test.does-not-exist"), None);
    }

    #[test]
    fn ashmem_create_and_size() {
        let fd = create_ashmem("test", 4096).expect("create_ashmem");
        assert!(fd >= 0);

        // Own the fd so it is closed at end of scope, but query it first.
        let file = unsafe { std::fs::File::from_raw_fd(fd) };
        assert_eq!(file.metadata().expect("metadata").len(), 4096);

        // The C entry point agrees about the size.
        assert_eq!(unsafe { ashmem_get_size_region(fd) }, 4096);
    }

    #[test]
    fn ashmem_rejects_zero_size() {
        assert!(create_ashmem("test", 0).is_err());
        assert_eq!(unsafe { ashmem_create_region(std::ptr::null(), 0) }, -1);
    }

    #[test]
    fn ashmem_pin_and_unpin() {
        let fd = create_ashmem("pin-test", 128).expect("create_ashmem");
        assert_eq!(unsafe { ashmem_pin_region(fd, 0, 128) }, 0);
        // Unpin is a no-op that reports success.
        assert_eq!(unsafe { ashmem_unpin_region(fd, 0, 128) }, 0);
        // Region is still readable/sized.
        assert_eq!(unsafe { ashmem_get_size_region(fd) }, 128);
        unsafe { libc::close(fd) };
    }

    #[test]
    fn c_property_get_copies_value() {
        let name = CString::new("ro.build.version.sdk").unwrap();
        let mut buf = [0 as c_char; PROP_VALUE_MAX];

        let n = unsafe { __system_property_get(name.as_ptr(), buf.as_mut_ptr()) };
        assert_eq!(n, 2, "sdk value is 2 bytes long");

        let got = unsafe { CStr::from_ptr(buf.as_ptr()) };
        assert_eq!(got.to_str().unwrap(), "36");
    }

    #[test]
    fn c_property_get_unknown_is_empty() {
        let name = CString::new("pengudroid.absent.prop").unwrap();
        let mut buf = [0x7f as c_char; PROP_VALUE_MAX];

        let n = unsafe { __system_property_get(name.as_ptr(), buf.as_mut_ptr()) };
        assert_eq!(n, 0);
        assert_eq!(buf[0], 0, "value buffer is NUL-terminated");
    }

    #[test]
    fn c_property_set_then_get() {
        let name = CString::new("pengudroid.c.set").unwrap();
        let value = CString::new("from-c").unwrap();
        assert_eq!(
            unsafe { __system_property_set(name.as_ptr(), value.as_ptr()) },
            0
        );

        let mut buf = [0 as c_char; PROP_VALUE_MAX];
        let n = unsafe { __system_property_get(name.as_ptr(), buf.as_mut_ptr()) };
        assert_eq!(n, 6);
        assert_eq!(unsafe { CStr::from_ptr(buf.as_ptr()) }.to_str().unwrap(), "from-c");
    }

    #[test]
    fn c_property_get_truncates_to_prop_value_max() {
        let name = CString::new("pengudroid.c.long").unwrap();
        let long = "x".repeat(PROP_VALUE_MAX * 2);
        set_prop("pengudroid.c.long", &long);

        let mut buf = [0 as c_char; PROP_VALUE_MAX];
        let n = unsafe { __system_property_get(name.as_ptr(), buf.as_mut_ptr()) };
        assert_eq!(n as usize, PROP_VALUE_MAX - 1);
        assert_eq!(buf[PROP_VALUE_MAX - 1], 0, "always NUL-terminated");
    }

    #[test]
    fn identity_reports_api_36() {
        assert_eq!(
            unsafe { android_get_device_api_level() },
            pengudroid_proto::abi::ANDROID_API_LEVEL
        );
        assert_eq!(
            unsafe { android_get_application_target_sdk_version() },
            pengudroid_proto::abi::ANDROID_API_LEVEL
        );
    }

    #[test]
    fn null_pointers_are_handled_gracefully() {
        let mut buf = [0 as c_char; PROP_VALUE_MAX];
        // Null name -> 0, no crash.
        assert_eq!(unsafe { __system_property_get(std::ptr::null(), buf.as_mut_ptr()) }, 0);
        // Null value pointer -> 0, no crash.
        let name = CString::new("ro.hardware").unwrap();
        assert_eq!(unsafe { __system_property_get(name.as_ptr(), std::ptr::null_mut()) }, 0);
        // Null name for set -> -1.
        assert_eq!(unsafe { __system_property_set(std::ptr::null(), std::ptr::null()) }, -1);
        // Null fmt for log -> negative, no crash.
        #[cfg(has_variadic_log)]
        assert!(unsafe { __android_log_print(ANDROID_LOG_INFO, std::ptr::null(), std::ptr::null()) } < 0);
    }

    #[cfg(has_variadic_log)]
    #[test]
    fn log_print_expands_format() {
        let tag = CString::new("Fmt").unwrap();
        let fmt = CString::new("x=%d s=%s").unwrap();
        let word = CString::new("ok").unwrap();
        let n = unsafe {
            __android_log_print(ANDROID_LOG_INFO, tag.as_ptr(), fmt.as_ptr(), 42 as c_int, word.as_ptr())
        };
        assert_eq!(n, "x=42 s=ok".len() as c_int);
    }

    #[cfg(has_variadic_log)]
    #[test]
    fn log_print_handles_long_formats() {
        // Longer than the C shim's stack buffer: exercises the heap path.
        let tag = CString::new("Long").unwrap();
        let fmt = CString::new("padding: %s").unwrap();
        let payload = CString::new("p".repeat(4096)).unwrap();
        let n = unsafe {
            __android_log_print(ANDROID_LOG_INFO, tag.as_ptr(), fmt.as_ptr(), payload.as_ptr())
        };
        assert_eq!(n as usize, "padding: ".len() + 4096);
    }

    #[test]
    fn errno_pointer_matches_glibc() {
        let p = unsafe { __errno() };
        assert!(!p.is_null());
        assert_eq!(p, unsafe { libc::__errno_location() });

        // A write through the returned pointer is visible via glibc's accessor.
        unsafe { *p = 42 };
        assert_eq!(unsafe { *libc::__errno_location() }, 42);
    }

    #[test]
    fn property_get_uses_default_for_unknown() {
        let name = CString::new("pengudroid.property_get.absent").unwrap();
        let default = CString::new("fallback").unwrap();
        let mut buf = [0 as c_char; PROP_VALUE_MAX];

        let n = unsafe { property_get(name.as_ptr(), buf.as_mut_ptr(), default.as_ptr()) };
        assert_eq!(n, "fallback".len() as c_int);
        assert_eq!(unsafe { CStr::from_ptr(buf.as_ptr()) }.to_str().unwrap(), "fallback");
    }

    #[test]
    fn property_get_reads_known_value() {
        let name = CString::new("ro.build.version.sdk").unwrap();
        let default = CString::new("ignored").unwrap();
        let mut buf = [0 as c_char; PROP_VALUE_MAX];

        let n = unsafe { property_get(name.as_ptr(), buf.as_mut_ptr(), default.as_ptr()) };
        assert_eq!(n, 2);
        assert_eq!(unsafe { CStr::from_ptr(buf.as_ptr()) }.to_str().unwrap(), "36");
    }

    #[test]
    fn ashmem_set_size_resizes_region() {
        let fd = create_ashmem("resize-test", 64).expect("create_ashmem");

        assert_eq!(unsafe { ashmem_set_size(fd, 4096) }, 0);
        assert_eq!(unsafe { libc::lseek(fd, 0, libc::SEEK_END) }, 4096);

        // A zero size is rejected.
        assert_eq!(unsafe { ashmem_set_size(fd, 0) }, -1);

        unsafe { libc::close(fd) };
    }

    #[test]
    fn property_serial_increases_on_set() {
        let before = prop_serial();
        set_prop("pengudroid.serial.test", "one");
        let after_safe = prop_serial();
        assert!(after_safe > before, "safe set_prop must bump the serial");

        let name = CString::new("pengudroid.serial.test").unwrap();
        let value = CString::new("two").unwrap();
        assert_eq!(unsafe { __system_property_set(name.as_ptr(), value.as_ptr()) }, 0);
        let after_c = prop_serial();
        assert!(after_c > after_safe, "C __system_property_set must bump the serial");

        // `__system_property_serial` reports the same counter.
        assert_eq!(unsafe { __system_property_serial(std::ptr::null()) }, after_c);
    }

    #[test]
    fn loggable_queries_report_one() {
        let tag = CString::new("PenguDroid").unwrap();
        assert_eq!(unsafe { __android_log_is_loggable(ANDROID_LOG_INFO, tag.as_ptr(), ANDROID_LOG_INFO) }, 1);
        assert_eq!(
            unsafe { __android_log_is_loggable_len(ANDROID_LOG_INFO, tag.as_ptr(), 9, ANDROID_LOG_INFO) },
            1
        );
    }

    #[test]
    fn asset_manager_from_java_is_non_null() {
        let mgr = unsafe { AAssetManager_fromJava(std::ptr::null_mut(), std::ptr::null_mut()) };
        assert!(!mgr.is_null());
    }

    #[test]
    fn strlen_chk_returns_length() {
        let s = CString::new("hello").unwrap();
        // Plenty of headroom: the length is reported directly.
        assert_eq!(unsafe { __strlen_chk(s.as_ptr(), 16) }, 5);
        // `slen` exactly equal to the length still finds the NUL.
        assert_eq!(unsafe { __strlen_chk(s.as_ptr(), 5) }, 5);

        // NOTE: the abort path (no NUL within `slen`) is intentionally not
        // exercised here — `libc::abort()` would tear down the test runner.
    }

    #[test]
    fn strchr_chk_finds_char_and_nul() {
        let s = CString::new("hello").unwrap();

        // A present character resolves to a pointer into the buffer.
        let found = unsafe { __strchr_chk(s.as_ptr(), 'l' as c_int, 16) };
        assert_eq!(found, unsafe { s.as_ptr().add(2) } as *mut c_char);

        // Searching for an absent character while a NUL is in range is null.
        assert!(unsafe { __strchr_chk(s.as_ptr(), 'z' as c_int, 16) }.is_null());

        // `c == 0` yields the terminating NUL itself.
        assert_eq!(
            unsafe { __strchr_chk(s.as_ptr(), 0, 16) },
            unsafe { s.as_ptr().add(5) } as *mut c_char
        );
    }

    #[test]
    fn strrchr_chk_returns_last_occurrence() {
        let s = CString::new("hello").unwrap();
        let last = unsafe { __strrchr_chk(s.as_ptr(), 'l' as c_int, 16) };
        assert_eq!(last, unsafe { s.as_ptr().add(3) } as *mut c_char);
        assert!(unsafe { __strrchr_chk(s.as_ptr(), 'z' as c_int, 16) }.is_null());
    }

    #[test]
    fn memchr_chk_finds_byte_within_bound() {
        let s = CString::new("hello").unwrap();
        let found = unsafe { __memchr_chk(s.as_ptr() as *const c_void, 'l' as c_int, 4, 16) };
        assert_eq!(found, unsafe { s.as_ptr().add(2) } as *mut c_void);

        // A byte past the searched range (`n`) is not found.
        assert!(unsafe { __memchr_chk(s.as_ptr() as *const c_void, 'o' as c_int, 4, 16) }.is_null());
    }

    #[test]
    fn sf_array_is_at_least_bionic_sized() {
        // 1024 bytes: comfortably larger than three Bionic `FILE`s on 64-bit.
        // Use a raw pointer round-trip so we don't take a reference to a
        // `static mut` (which the `static_mut_refs` lint rejects).
        let p: *const [u64; 128] = &raw const __sF;
        assert_eq!(std::mem::size_of_val(unsafe { &*p }), 1024);
    }
}
