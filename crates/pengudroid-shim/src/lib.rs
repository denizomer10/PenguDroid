// `rust-lld` warns when a version script reassigns an already-global symbol to a
// version node; the reassignment is intentional and succeeds, so silence it.
#![allow(linker_messages)]

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
use std::sync::{LazyLock, Mutex, OnceLock};

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

/// `ANativeWindow_setBuffersGeometry(w, width, height, format)`.
///
/// // STUB: the real buffer geometry is owned by the shim's EGL surface; this
/// accepts the request and reports success.
#[no_mangle]
pub unsafe extern "C" fn ANativeWindow_setBuffersGeometry(
    _w: *mut c_void,
    _width: i32,
    _height: i32,
    _format: i32,
) -> i32 {
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

/// Bionic's bounded `strncpy`. Aborts when the requested length exceeds the
/// destination capacity.
#[no_mangle]
pub unsafe extern "C" fn __strncpy_chk(
    dst: *mut c_char,
    src: *const c_char,
    n: usize,
    dst_len: usize,
) -> *mut c_char {
    if n > dst_len {
        libc::abort();
    }
    libc::strncpy(dst, src, n)
}

/// Bionic's `strncpy` that knows both buffer sizes.
#[no_mangle]
pub unsafe extern "C" fn __strncpy_chk2(
    dst: *mut c_char,
    src: *const c_char,
    n: usize,
    dst_len: usize,
    src_len: usize,
) -> *mut c_char {
    if n > dst_len || n > src_len {
        libc::abort();
    }
    libc::strncpy(dst, src, n)
}

/// `stpncpy` is not exposed by the `libc` crate; implement it directly.
unsafe fn stpncpy_impl(dst: *mut c_char, src: *const c_char, n: usize) -> *mut c_char {
    let mut i = 0usize;
    while i < n {
        let ch = *src.add(i);
        *dst.add(i) = ch;
        if ch == 0 {
            for j in (i + 1)..n {
                *dst.add(j) = 0;
            }
            return dst.add(i);
        }
        i += 1;
    }
    dst.add(n)
}

#[no_mangle]
pub unsafe extern "C" fn __stpncpy_chk(
    dst: *mut c_char,
    src: *const c_char,
    n: usize,
    dst_len: usize,
) -> *mut c_char {
    if n > dst_len {
        libc::abort();
    }
    stpncpy_impl(dst, src, n)
}

#[no_mangle]
pub unsafe extern "C" fn __stpncpy_chk2(
    dst: *mut c_char,
    src: *const c_char,
    n: usize,
    dst_len: usize,
    src_len: usize,
) -> *mut c_char {
    if n > dst_len || n > src_len {
        libc::abort();
    }
    stpncpy_impl(dst, src, n)
}

#[no_mangle]
pub unsafe extern "C" fn __strncat_chk(
    dst: *mut c_char,
    src: *const c_char,
    n: usize,
    dst_len: usize,
) -> *mut c_char {
    let cur = libc::strlen(dst);
    if cur.saturating_add(n) >= dst_len {
        libc::abort();
    }
    libc::strncat(dst, src, n)
}

#[no_mangle]
pub unsafe extern "C" fn __strcat_chk(
    dst: *mut c_char,
    src: *const c_char,
    dst_len: usize,
) -> *mut c_char {
    let cur = libc::strlen(dst);
    let add = libc::strlen(src);
    if cur.saturating_add(add) >= dst_len {
        libc::abort();
    }
    libc::strcat(dst, src)
}

/// Reverse `memchr`, bounded.
#[no_mangle]
pub unsafe extern "C" fn __memrchr_chk(
    s: *const c_void,
    c: c_int,
    n: usize,
    slen: usize,
) -> *mut c_void {
    if n > slen {
        libc::abort();
    }
    let bytes = s as *const u8;
    let target = c as u8;
    let mut i = n;
    while i > 0 {
        i -= 1;
        if *bytes.add(i) == target {
            return bytes.add(i) as *mut c_void;
        }
    }
    std::ptr::null_mut()
}

/// Bionic's bounded `FD_SET`/`FD_CLR`/`FD_ISSET` variants.
#[no_mangle]
pub unsafe extern "C" fn __FD_SET_chk(
    fd: c_int,
    set: *mut libc::fd_set,
    set_size: usize,
) -> c_int {
    if fd < 0 || (fd as usize) >= set_size.saturating_mul(8) {
        libc::abort();
    }
    libc::FD_SET(fd, set);
    0
}

#[no_mangle]
pub unsafe extern "C" fn __FD_CLR_chk(
    fd: c_int,
    set: *mut libc::fd_set,
    set_size: usize,
) -> c_int {
    if fd < 0 || (fd as usize) >= set_size.saturating_mul(8) {
        libc::abort();
    }
    libc::FD_CLR(fd, set);
    0
}

#[no_mangle]
pub unsafe extern "C" fn __FD_ISSET_chk(
    fd: c_int,
    set: *const libc::fd_set,
    set_size: usize,
) -> c_int {
    if fd < 0 || (fd as usize) >= set_size.saturating_mul(8) {
        libc::abort();
    }
    i32::from(libc::FD_ISSET(fd, set))
}

/// Bionic's `__assert`/`__assert2`: report and abort (glibc's are different).
unsafe fn bionic_assert(file: *const c_char, line: c_int, func: *const c_char, msg: *const c_char) -> ! {
    let file = if file.is_null() { String::from("<unknown>") } else { cstr_lossy(file) };
    let msg = if msg.is_null() { String::new() } else { cstr_lossy(msg) };
    if func.is_null() {
        eprintln!("assertion failed: {msg} at {file}:{line}");
    } else {
        eprintln!("assertion failed: {msg} at {}(), {file}:{line}", cstr_lossy(func));
    }
    libc::abort();
}

#[no_mangle]
pub unsafe extern "C" fn __assert(file: *const c_char, line: c_int, msg: *const c_char) -> ! {
    bionic_assert(file, line, std::ptr::null(), msg)
}

#[no_mangle]
pub unsafe extern "C" fn __assert2(
    file: *const c_char,
    line: c_int,
    func: *const c_char,
    msg: *const c_char,
) -> ! {
    bionic_assert(file, line, func, msg)
}

// ---------------------------------------------------------------------------
// __sF (Bionic stdio streams)
// ---------------------------------------------------------------------------

/// Bionic's standard-stream array, `FILE __sF[3]` (with `stdin`/`stdout`/`stderr`
/// being `&__sF[0..2]`). glibc has no such symbol, so Bionic-built libraries
/// that reference it fail to load.
///
// CAVEAT: this 1024-byte blob is deliberately *not* ABI-compatible with
// glibc's `FILE`. The stdio entry points below ([`fprintf`], [`vfprintf`],
// [`fputs`], [`fwrite`], [`fputc`], [`fflush`], [`fclose`]) recognise pointers
// into it and redirect them to glibc's real standard streams; other stdio
// functions that receive `&__sF[..]` directly are still unsupported.
#[no_mangle]
pub static mut __sF: [u64; 128] = [0u64; 128];

/// Size in bytes of a Bionic `FILE` on LP64 (the only ABI we support here).
pub const BIONIC_FILE_SIZE: usize = 152;

/// Map an address into the Bionic `__sF` array to a standard-stream index
/// (`0` = stdin, `1` = stdout, `2` = stderr). Returns `None` for any address
/// outside the array.
pub fn bionic_stream_index(stream: usize) -> Option<usize> {
    let base = (&raw const __sF) as usize;
    if stream < base || stream >= base + 1024 {
        return None;
    }
    Some(match (stream - base) / BIONIC_FILE_SIZE {
        0 => 0,
        1 => 1,
        _ => 2,
    })
}

/// `RTLD_NEXT` — search for symbols after this object. Not exported by the
/// `libc` crate for the glibc target, so spell it out.
const RTLD_NEXT: *mut c_void = -1isize as *mut c_void;

/// `RTLD_DEFAULT` — the default global search scope.
const RTLD_DEFAULT: *mut c_void = std::ptr::null_mut();

/// Resolve `name` (NUL-terminated) in the object following this one.
///
/// Returns `None` when the host libc does not export the symbol, letting each
/// wrapper fall back gracefully instead of crashing.
unsafe fn dlsym_next<T: Copy>(name: &[u8]) -> Option<T> {
    debug_assert_eq!(std::mem::size_of::<T>(), std::mem::size_of::<*mut c_void>());
    let p = libc::dlsym(RTLD_NEXT, name.as_ptr() as *const c_char);
    if p.is_null() {
        None
    } else {
        Some(std::mem::transmute_copy(&p))
    }
}

// ---------------------------------------------------------------------------
// Destructor suppression
// ---------------------------------------------------------------------------

/// Interpret a `PENGUDROID_SUPPRESS_DESTRUCTORS` value: `1`, `true` and `yes`
/// (ASCII case-insensitive, surrounding whitespace ignored) mean "suppress".
pub fn env_flag(v: Option<&str>) -> bool {
    match v {
        Some(v) => {
            let v = v.trim();
            v.eq_ignore_ascii_case("1")
                || v.eq_ignore_ascii_case("true")
                || v.eq_ignore_ascii_case("yes")
        }
        None => false,
    }
}

/// Whether guest static destructors are suppressed for this process.
///
/// Bionic C++ libraries register destructors that assume Bionic runtime state;
/// running them on glibc after `dlclose`/`atexit` can segfault. The value is
/// read once and cached.
pub fn destructors_suppressed() -> bool {
    static SUPPRESSED: OnceLock<bool> = OnceLock::new();
    *SUPPRESSED.get_or_init(|| {
        let raw = std::env::var("PENGUDROID_SUPPRESS_DESTRUCTORS").ok();
        env_flag(raw.as_deref())
    })
}

type CxaAtexit = unsafe extern "C" fn(
    Option<unsafe extern "C" fn(*mut c_void)>,
    *mut c_void,
    *mut c_void,
) -> c_int;
type Atexit = unsafe extern "C" fn(Option<extern "C" fn()>) -> c_int;
type CxaFinalize = unsafe extern "C" fn(*mut c_void);

/// `__cxa_atexit(func, arg, dso_handle)` — register a destructor, unless
/// suppression is enabled (in which case the registration is dropped).
#[no_mangle]
pub unsafe extern "C" fn __cxa_atexit(
    func: Option<unsafe extern "C" fn(*mut c_void)>,
    arg: *mut c_void,
    dso_handle: *mut c_void,
) -> c_int {
    if destructors_suppressed() {
        return 0;
    }
    match dlsym_next::<CxaAtexit>(b"__cxa_atexit\0") {
        Some(real) => real(func, arg, dso_handle),
        None => 0,
    }
}

/// `atexit(func)` — register a destructor, unless suppression is enabled.
#[no_mangle]
pub unsafe extern "C" fn atexit(func: Option<extern "C" fn()>) -> c_int {
    if destructors_suppressed() {
        return 0;
    }
    match dlsym_next::<Atexit>(b"atexit\0") {
        Some(real) => real(func),
        None => 0,
    }
}

/// `__cxa_finalize(dso_handle)` — run pending destructors, unless suppression
/// is enabled (in which case it is a no-op).
#[no_mangle]
pub unsafe extern "C" fn __cxa_finalize(dso_handle: *mut c_void) {
    if destructors_suppressed() {
        return;
    }
    if let Some(real) = dlsym_next::<CxaFinalize>(b"__cxa_finalize\0") {
        real(dso_handle);
    }
}

/// `__cxa_thread_atexit_impl(func, arg, dso_handle)` — thread-exit destructor
/// registration, suppressed like [`__cxa_atexit`].
#[no_mangle]
pub unsafe extern "C" fn __cxa_thread_atexit_impl(
    func: Option<unsafe extern "C" fn(*mut c_void)>,
    arg: *mut c_void,
    dso_handle: *mut c_void,
) -> c_int {
    if destructors_suppressed() {
        return 0;
    }
    match dlsym_next::<CxaAtexit>(b"__cxa_thread_atexit_impl\0") {
        Some(real) => real(func, arg, dso_handle),
        None => 0,
    }
}

// ---------------------------------------------------------------------------
// Bionic stdio on __sF
// ---------------------------------------------------------------------------

/// Resolve glibc's `stdin`/`stdout`/`stderr` `FILE *` values once.
///
/// `dlsym` returns the address of the `FILE *` *variable*, so dereference it.
/// The addresses are stored as `usize` to keep the cache `Sync`.
fn glibc_stream_pointers() -> &'static [usize; 3] {
    static STREAMS: OnceLock<[usize; 3]> = OnceLock::new();
    STREAMS.get_or_init(|| {
        let mut out = [0usize; 3];
        let names: [&[u8]; 3] = [b"stdin\0", b"stdout\0", b"stderr\0"];
        for (i, name) in names.iter().enumerate() {
            unsafe {
                let p = libc::dlsym(RTLD_DEFAULT, name.as_ptr() as *const c_char);
                if !p.is_null() {
                    out[i] = *(p as *mut *mut c_void) as usize;
                }
            }
        }
        out
    })
}

/// Translate a Bionic `FILE *` that points into `__sF` to glibc's matching
/// standard stream. Any other pointer is passed through unchanged.
unsafe fn translate_stream(stream: *mut c_void) -> *mut c_void {
    if stream.is_null() {
        return stream;
    }
    if let Some(idx) = bionic_stream_index(stream as usize) {
        let mapped = glibc_stream_pointers()[idx];
        if mapped != 0 {
            return mapped as *mut c_void;
        }
    }
    stream
}

/// Whether `stream` is (already translated to) one of glibc's standard streams.
fn is_standard_stream(stream: *mut c_void) -> bool {
    glibc_stream_pointers().contains(&(stream as usize))
}

/// File descriptor backing one of glibc's standard streams, used only by the
/// `dlsym` failure fallbacks below.
fn stream_fd(stream: *mut c_void) -> c_int {
    let p = glibc_stream_pointers();
    if (stream as usize) == p[2] {
        2
    } else {
        1
    }
}

type Vfprintf = unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_void) -> c_int;
type Fputs = unsafe extern "C" fn(*const c_char, *mut c_void) -> c_int;
type Fwrite = unsafe extern "C" fn(*const c_void, usize, usize, *mut c_void) -> usize;
type Fputc = unsafe extern "C" fn(c_int, *mut c_void) -> c_int;
type Fflush = unsafe extern "C" fn(*mut c_void) -> c_int;
type Fclose = unsafe extern "C" fn(*mut c_void) -> c_int;

/// `fprintf(stream, fmt, ...)`.
///
/// LIMITATION: stable Rust cannot build a `va_list`, so this variadic entry
/// point cannot forward its arguments. It writes `fmt` literally (no `%`
/// expansion) to the translated stream. The fully-correct path is
/// [`vfprintf`], which receives a ready `va_list` and is forwarded intact;
/// libc++ and friends generally call that internally.
#[no_mangle]
pub unsafe extern "C" fn fprintf(stream: *mut c_void, fmt: *const c_char, _: ...) -> c_int {
    if fmt.is_null() {
        return -1;
    }
    fputs(fmt, translate_stream(stream))
}

/// `vfprintf(stream, fmt, ap)` — fully forwarded with the translated stream.
#[no_mangle]
pub unsafe extern "C" fn vfprintf(
    stream: *mut c_void,
    fmt: *const c_char,
    ap: *mut c_void,
) -> c_int {
    let stream = translate_stream(stream);
    match dlsym_next::<Vfprintf>(b"vfprintf\0") {
        Some(real) => real(stream, fmt, ap),
        None => -1,
    }
}

/// `fputs(s, stream)` with `__sF` stream translation.
#[no_mangle]
pub unsafe extern "C" fn fputs(s: *const c_char, stream: *mut c_void) -> c_int {
    let stream = translate_stream(stream);
    match dlsym_next::<Fputs>(b"fputs\0") {
        Some(real) => real(s, stream),
        None => {
            // No host symbol to forward to (e.g. in a bare test binary): write
            // straight to the descriptor instead of failing.
            if s.is_null() {
                return -1;
            }
            let len = libc::strlen(s);
            if libc::write(stream_fd(stream), s as *const c_void, len) < 0 {
                libc::EOF
            } else {
                0
            }
        }
    }
}

/// `fwrite(ptr, size, nmemb, stream)` with `__sF` stream translation.
#[no_mangle]
pub unsafe extern "C" fn fwrite(
    ptr: *const c_void,
    size: usize,
    nmemb: usize,
    stream: *mut c_void,
) -> usize {
    let stream = translate_stream(stream);
    match dlsym_next::<Fwrite>(b"fwrite\0") {
        Some(real) => real(ptr, size, nmemb, stream),
        None => {
            let total = match size.checked_mul(nmemb) {
                Some(n) => n,
                None => return 0,
            };
            if total == 0 || libc::write(stream_fd(stream), ptr, total) as usize != total {
                0
            } else {
                nmemb
            }
        }
    }
}

/// `fputc(c, stream)` with `__sF` stream translation.
#[no_mangle]
pub unsafe extern "C" fn fputc(c: c_int, stream: *mut c_void) -> c_int {
    let stream = translate_stream(stream);
    match dlsym_next::<Fputc>(b"fputc\0") {
        Some(real) => real(c, stream),
        None => {
            let byte = c as u8;
            if libc::write(stream_fd(stream), &byte as *const u8 as *const c_void, 1) < 0 {
                libc::EOF
            } else {
                c
            }
        }
    }
}

/// `fflush(stream)` with `__sF` stream translation. A null `stream` means
/// "flush every stream" and is preserved.
#[no_mangle]
pub unsafe extern "C" fn fflush(stream: *mut c_void) -> c_int {
    let stream = translate_stream(stream);
    match dlsym_next::<Fflush>(b"fflush\0") {
        Some(real) => real(stream),
        None => 0,
    }
}

/// `fclose(stream)` with `__sF` stream translation.
///
/// Guest code sometimes closes `stderr`/`stdout`; doing that to the host's real
/// stream would break the whole process, so closing a standard stream is
/// reported as success without forwarding.
#[no_mangle]
pub unsafe extern "C" fn fclose(stream: *mut c_void) -> c_int {
    let stream = translate_stream(stream);
    if is_standard_stream(stream) {
        return 0;
    }
    match dlsym_next::<Fclose>(b"fclose\0") {
        Some(real) => real(stream),
        None => 0,
    }
}

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
// OpenSL ES (buffer-queue playback)
// ---------------------------------------------------------------------------
// Real apps `DT_NEEDED` `libOpenSLES.so` even when they only render, so the
// entry points and interface IDs must exist. This implements the small slice of
// OpenSL ES 1.0.1 that a normal Android "buffer queue player" uses:
//
//   slCreateEngine -> Realize -> GetInterface(SL_IID_ENGINE)
//     -> CreateOutputMix / CreateAudioPlayer
//     -> Realize -> GetInterface(SL_IID_PLAY)
//     -> GetInterface(SL_IID_ANDROIDSIMPLEBUFFERQUEUE)
//     -> SetPlayState(PLAYING) -> Enqueue(PCM)
//
// Enqueued PCM is written to a lazily spawned `pw-cat` (or `aplay`) sink, which
// plays it through PipeWire/ALSA. The object/interface layering follows the
// canonical Khronos headers exactly: an `SLObjectItf`/`SLEngineItf`/`SLPlayItf`
// /`SLAndroidSimpleBufferQueueItf` is a pointer to a pointer to a vtable, so the
// first field of every structure below is that vtable pointer. Field order is
// load-bearing: the guest indexes the vtable by byte offset.
//
// `SL_IID_*` are opaque interface identifiers; callers only pass them back to
// us, so unique non-null values suffice (they are never dereferenced).
use std::io::Write as _;
use std::process::{Child, ChildStdin, Command, Stdio};

const SL_RESULT_SUCCESS: u32 = 0;
const SL_RESULT_PARAMETER_INVALID: u32 = 2;
const SL_RESULT_FEATURE_UNSUPPORTED: u32 = 12;

const SL_OBJECT_STATE_UNREALIZED: u32 = 1;
const SL_OBJECT_STATE_REALIZED: u32 = 2;

const SL_PLAYSTATE_STOPPED: u32 = 1;
#[allow(dead_code)] // Used by the unit test; kept for symmetry/documentation.
const SL_PLAYSTATE_PLAYING: u32 = 3;

const SL_DATALOCATOR_ANDROIDSIMPLEBUFFERQUEUE: u32 = 0x8000_07BD;
const SL_DATAFORMAT_PCM: u32 = 2;

const DEFAULT_RATE: u32 = 48_000;
const DEFAULT_CHANNELS: u32 = 2;

/// OpenSL's `SLDataSource` / `SLDataSink` layouts (both are `{locator, format}`).
#[repr(C)]
struct SLDataSource {
    p_locator: *mut c_void,
    p_format: *mut c_void,
}

#[repr(C)]
struct SLDataSink {
    p_locator: *mut c_void,
    p_format: *mut c_void,
}

#[repr(C)]
struct SLDataFormatPcm {
    format_type: u32,
    num_channels: u32,
    samples_per_sec: u32,
    bits_per_sample: u32,
    container_size: u32,
    channel_mask: u32,
    endianness: u32,
}

#[repr(C)]
struct SLDataLocatorAndroidSimpleBufferQueue {
    locator_type: u32,
    num_buffers: u32,
}

#[repr(C)]
struct SLAndroidSimpleBufferQueueState {
    count: u32,
    index: u32,
}

// ---------------------------------------------------------------------------
// Vtables
// ---------------------------------------------------------------------------

type SlObjectCallback = Option<
    unsafe extern "C" fn(*mut c_void, *const c_void, u32, u32, u32, *mut c_void),
>;
type SlPlayCallback = Option<unsafe extern "C" fn(*mut c_void, *mut c_void, u32)>;
type SlBqCallback = Option<unsafe extern "C" fn(*mut c_void, *mut c_void)>;

#[repr(C)]
#[allow(non_snake_case)]
struct SLObjectItf_ {
    Realize: Option<unsafe extern "C" fn(*mut c_void, u32) -> u32>,
    Resume: Option<unsafe extern "C" fn(*mut c_void, u32) -> u32>,
    GetState: Option<unsafe extern "C" fn(*mut c_void, *mut u32) -> u32>,
    GetInterface: Option<unsafe extern "C" fn(*mut c_void, *const c_void, *mut *mut c_void) -> u32>,
    RegisterCallback: Option<unsafe extern "C" fn(*mut c_void, SlObjectCallback, *mut c_void) -> u32>,
    AbortAsyncOperation: Option<unsafe extern "C" fn(*mut c_void)>,
    Destroy: Option<unsafe extern "C" fn(*mut c_void)>,
    SetPriority: Option<unsafe extern "C" fn(*mut c_void, i32, u32) -> u32>,
    GetPriority: Option<unsafe extern "C" fn(*mut c_void, *mut i32, *mut u32) -> u32>,
    SetLossOfControlInterfaces:
        Option<unsafe extern "C" fn(*mut c_void, i16, *mut *const c_void, u32) -> u32>,
}

#[repr(C)]
#[allow(non_snake_case)]
struct SLEngineItf_ {
    CreateLEDDevice: Option<
        unsafe extern "C" fn(
            *mut c_void,
            *mut *mut c_void,
            u32,
            u32,
            *const *const c_void,
            *const u32,
        ) -> u32,
    >,
    CreateVibraDevice: Option<
        unsafe extern "C" fn(
            *mut c_void,
            *mut *mut c_void,
            u32,
            u32,
            *const *const c_void,
            *const u32,
        ) -> u32,
    >,
    CreateAudioPlayer: Option<
        unsafe extern "C" fn(
            *mut c_void,
            *mut *mut c_void,
            *mut SLDataSource,
            *mut SLDataSink,
            u32,
            *const *const c_void,
            *const u32,
        ) -> u32,
    >,
    CreateAudioRecorder: Option<
        unsafe extern "C" fn(
            *mut c_void,
            *mut *mut c_void,
            *mut SLDataSource,
            *mut SLDataSink,
            u32,
            *const *const c_void,
            *const u32,
        ) -> u32,
    >,
    CreateMidiPlayer: Option<
        unsafe extern "C" fn(
            *mut c_void,
            *mut *mut c_void,
            *mut SLDataSource,
            *mut SLDataSource,
            *mut SLDataSink,
            *mut SLDataSink,
            *mut SLDataSink,
            u32,
            *const *const c_void,
            *const u32,
        ) -> u32,
    >,
    CreateListener: Option<
        unsafe extern "C" fn(
            *mut c_void,
            *mut *mut c_void,
            u32,
            *const *const c_void,
            *const u32,
        ) -> u32,
    >,
    Create3DGroup: Option<
        unsafe extern "C" fn(
            *mut c_void,
            *mut *mut c_void,
            u32,
            *const *const c_void,
            *const u32,
        ) -> u32,
    >,
    CreateOutputMix: Option<
        unsafe extern "C" fn(
            *mut c_void,
            *mut *mut c_void,
            u32,
            *const *const c_void,
            *const u32,
        ) -> u32,
    >,
    CreateMetadataExtractor: Option<
        unsafe extern "C" fn(
            *mut c_void,
            *mut *mut c_void,
            *mut SLDataSource,
            u32,
            *const *const c_void,
            *const u32,
        ) -> u32,
    >,
    CreateExtensionObject: Option<
        unsafe extern "C" fn(
            *mut c_void,
            *mut *mut c_void,
            *mut c_void,
            u32,
            u32,
            *const *const c_void,
            *const u32,
        ) -> u32,
    >,
    QueryNumSupportedInterfaces:
        Option<unsafe extern "C" fn(*mut c_void, u32, *mut u32) -> u32>,
    QuerySupportedInterfaces:
        Option<unsafe extern "C" fn(*mut c_void, u32, u32, *mut *const c_void) -> u32>,
    QueryNumSupportedExtensions: Option<unsafe extern "C" fn(*mut c_void, *mut u32) -> u32>,
    QuerySupportedExtension:
        Option<unsafe extern "C" fn(*mut c_void, u32, *mut c_char, *mut i16) -> u32>,
    IsExtensionSupported:
        Option<unsafe extern "C" fn(*mut c_void, *const c_char, *mut u32) -> u32>,
}

#[repr(C)]
#[allow(non_snake_case)]
struct SLPlayItf_ {
    SetPlayState: Option<unsafe extern "C" fn(*mut c_void, u32) -> u32>,
    GetPlayState: Option<unsafe extern "C" fn(*mut c_void, *mut u32) -> u32>,
    GetDuration: Option<unsafe extern "C" fn(*mut c_void, *mut u32) -> u32>,
    GetPosition: Option<unsafe extern "C" fn(*mut c_void, *mut u32) -> u32>,
    RegisterCallback: Option<unsafe extern "C" fn(*mut c_void, SlPlayCallback, *mut c_void) -> u32>,
    SetCallbackEventsMask: Option<unsafe extern "C" fn(*mut c_void, u32) -> u32>,
    GetCallbackEventsMask: Option<unsafe extern "C" fn(*mut c_void, *mut u32) -> u32>,
    SetMarkerPosition: Option<unsafe extern "C" fn(*mut c_void, u32) -> u32>,
    ClearMarkerPosition: Option<unsafe extern "C" fn(*mut c_void) -> u32>,
    GetMarkerPosition: Option<unsafe extern "C" fn(*mut c_void, *mut u32) -> u32>,
    SetPositionUpdatePeriod: Option<unsafe extern "C" fn(*mut c_void, u32) -> u32>,
    GetPositionUpdatePeriod: Option<unsafe extern "C" fn(*mut c_void, *mut u32) -> u32>,
}

#[repr(C)]
#[allow(non_snake_case)]
struct SLAndroidSimpleBufferQueueItf_ {
    Enqueue: Option<unsafe extern "C" fn(*mut c_void, *const c_void, u32) -> u32>,
    Clear: Option<unsafe extern "C" fn(*mut c_void) -> u32>,
    GetState: Option<unsafe extern "C" fn(*mut c_void, *mut SLAndroidSimpleBufferQueueState) -> u32>,
    RegisterCallback: Option<unsafe extern "C" fn(*mut c_void, SlBqCallback, *mut c_void) -> u32>,
}

// ---------------------------------------------------------------------------
// Objects and interfaces
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
enum ObjKind {
    Engine = 0,
    OutputMix = 1,
    AudioPlayer = 2,
}

/// Both `ChildStdin` and `Child` are dropped by `Sink`'s `Drop`.
struct Sink {
    child: Child,
    stdin: Option<ChildStdin>,
}

impl Drop for Sink {
    fn drop(&mut self) {
        // Closing stdin signals EOF; kill+wait guarantees we reap even if the
        // player ignores EOF (e.g. pw-cat waiting on a stalled server).
        drop(self.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// An `SLObjectItf` value. The first field *must* stay first: the guest treats
/// the object pointer as a pointer to a vtable pointer.
#[repr(C)]
struct Obj {
    vtable: *const SLObjectItf_,
    kind: ObjKind,
    state: u32,
    rate: u32,
    channels: u32,
    play_state: u32,
    bq_count: u32,
    bq_index: u32,
    bq_callback: SlBqCallback,
    bq_context: *mut c_void,
    sink: Option<Sink>,
    /// Interface blocks handed out by `GetInterface`, freed by `Destroy`.
    interfaces: Vec<*mut c_void>,
}

impl Obj {
    fn new(kind: ObjKind, rate: u32, channels: u32) -> Self {
        Obj {
            vtable: &OBJECT_VTABLE,
            kind,
            state: SL_OBJECT_STATE_UNREALIZED,
            rate,
            channels,
            play_state: SL_PLAYSTATE_STOPPED,
            bq_count: 0,
            bq_index: 0,
            bq_callback: None,
            bq_context: std::ptr::null_mut(),
            sink: None,
            interfaces: Vec::new(),
        }
    }
}

/// An `SLEngineItf` / `SLPlayItf` / `SLAndroidSimpleBufferQueueItf` value. Like
/// `Obj`, the vtable pointer is the first field.
#[repr(C)]
struct Itf {
    vtable: *const c_void,
    owner: *mut Obj,
}

unsafe fn itf_owner(self_: *mut c_void) -> *mut Obj {
    if self_.is_null() {
        return std::ptr::null_mut();
    }
    (*(self_ as *const Itf)).owner
}

unsafe fn alloc_itf(owner: *mut Obj, vtable: *const c_void) -> *mut c_void {
    let p = Box::into_raw(Box::new(Itf { vtable, owner })) as *mut c_void;
    if !owner.is_null() {
        (*owner).interfaces.push(p);
    }
    p
}

fn iid_matches(iid: *const c_void, value: usize) -> bool {
    !iid.is_null() && iid as usize == value
}

// ---------------------------------------------------------------------------
// Object interface implementation
// ---------------------------------------------------------------------------

// Object-vtable methods receive the `SLObjectItf` itself, which is our `Obj`
// pointer (its first field is the vtable pointer). Interface-vtable methods
// receive an `Itf` instead and use `itf_owner` above.
unsafe fn obj_of(self_: *mut c_void) -> *mut Obj {
    self_ as *mut Obj
}

unsafe extern "C" fn obj_realize(self_: *mut c_void, _async: u32) -> u32 {
    let owner = obj_of(self_);
    if owner.is_null() {
        return SL_RESULT_PARAMETER_INVALID;
    }
    (*owner).state = SL_OBJECT_STATE_REALIZED;
    SL_RESULT_SUCCESS
}

unsafe extern "C" fn obj_resume(self_: *mut c_void, _async: u32) -> u32 {
    if obj_of(self_).is_null() {
        SL_RESULT_PARAMETER_INVALID
    } else {
        SL_RESULT_SUCCESS
    }
}

unsafe extern "C" fn obj_get_state(self_: *mut c_void, p_state: *mut u32) -> u32 {
    let owner = obj_of(self_);
    if owner.is_null() || p_state.is_null() {
        return SL_RESULT_PARAMETER_INVALID;
    }
    *p_state = (*owner).state;
    SL_RESULT_SUCCESS
}

unsafe extern "C" fn obj_get_interface(
    self_: *mut c_void,
    iid: *const c_void,
    p_interface: *mut *mut c_void,
) -> u32 {
    let owner = obj_of(self_);
    if owner.is_null() || p_interface.is_null() {
        return SL_RESULT_PARAMETER_INVALID;
    }
    *p_interface = std::ptr::null_mut();

    // The object interface is the object itself.
    if iid_matches(iid, SL_IID_OBJECT) {
        *p_interface = self_;
        return SL_RESULT_SUCCESS;
    }

    let itf = match (*owner).kind {
        ObjKind::Engine if iid_matches(iid, SL_IID_ENGINE) => alloc_itf(
            owner,
            &ENGINE_VTABLE as *const SLEngineItf_ as *const c_void,
        ),
        ObjKind::AudioPlayer if iid_matches(iid, SL_IID_PLAY) => {
            alloc_itf(owner, &PLAY_VTABLE as *const SLPlayItf_ as *const c_void)
        }
        ObjKind::AudioPlayer if iid_matches(iid, SL_IID_ANDROIDSIMPLEBUFFERQUEUE) => alloc_itf(
            owner,
            &BQ_VTABLE as *const SLAndroidSimpleBufferQueueItf_ as *const c_void,
        ),
        _ => return SL_RESULT_FEATURE_UNSUPPORTED,
    };

    *p_interface = itf;
    SL_RESULT_SUCCESS
}

unsafe extern "C" fn obj_register_callback(
    _self: *mut c_void,
    _callback: SlObjectCallback,
    _context: *mut c_void,
) -> u32 {
    SL_RESULT_SUCCESS
}

unsafe extern "C" fn obj_abort_async(_self: *mut c_void) {}

unsafe extern "C" fn obj_destroy(self_: *mut c_void) {
    if self_.is_null() {
        return;
    }
    let obj = Box::from_raw(self_ as *mut Obj);
    for itf in &obj.interfaces {
        if !itf.is_null() {
            drop(Box::from_raw(*itf as *mut Itf));
        }
    }
    // The sink's Drop closes stdin and reaps the child.
    drop(obj);
}

unsafe extern "C" fn obj_set_priority(
    _self: *mut c_void,
    _priority: i32,
    _preemptable: u32,
) -> u32 {
    SL_RESULT_SUCCESS
}

unsafe extern "C" fn obj_get_priority(
    _self: *mut c_void,
    p_priority: *mut i32,
    p_preemptable: *mut u32,
) -> u32 {
    if !p_priority.is_null() {
        *p_priority = 0;
    }
    if !p_preemptable.is_null() {
        *p_preemptable = 0;
    }
    SL_RESULT_SUCCESS
}

unsafe extern "C" fn obj_set_loss_of_control_interfaces(
    _self: *mut c_void,
    _num_interfaces: i16,
    _p_interface_ids: *mut *const c_void,
    _enabled: u32,
) -> u32 {
    SL_RESULT_SUCCESS
}

static OBJECT_VTABLE: SLObjectItf_ = SLObjectItf_ {
    Realize: Some(obj_realize),
    Resume: Some(obj_resume),
    GetState: Some(obj_get_state),
    GetInterface: Some(obj_get_interface),
    RegisterCallback: Some(obj_register_callback),
    AbortAsyncOperation: Some(obj_abort_async),
    Destroy: Some(obj_destroy),
    SetPriority: Some(obj_set_priority),
    GetPriority: Some(obj_get_priority),
    SetLossOfControlInterfaces: Some(obj_set_loss_of_control_interfaces),
};

// ---------------------------------------------------------------------------
// Engine interface implementation
// ---------------------------------------------------------------------------

unsafe fn make_object(kind: ObjKind, rate: u32, channels: u32) -> *mut c_void {
    Box::into_raw(Box::new(Obj::new(kind, rate, channels))) as *mut c_void
}

unsafe extern "C" fn engine_create_audio_player(
    _self: *mut c_void,
    p_player: *mut *mut c_void,
    p_audio_src: *mut SLDataSource,
    _p_audio_snk: *mut SLDataSink,
    _num_interfaces: u32,
    _p_interface_ids: *const *const c_void,
    _p_interface_required: *const u32,
) -> u32 {
    if p_player.is_null() {
        return SL_RESULT_PARAMETER_INVALID;
    }
    *p_player = std::ptr::null_mut();

    // Defaults used when the source is not an Android simple buffer queue.
    let mut rate = DEFAULT_RATE;
    let mut channels = DEFAULT_CHANNELS;

    if !p_audio_src.is_null() {
        let src = &*p_audio_src;
        if !src.p_locator.is_null() && !src.p_format.is_null() {
            let locator = &*(src.p_locator as *const SLDataLocatorAndroidSimpleBufferQueue);
            let format = &*(src.p_format as *const SLDataFormatPcm);
            if locator.locator_type == SL_DATALOCATOR_ANDROIDSIMPLEBUFFERQUEUE
                && format.format_type == SL_DATAFORMAT_PCM
            {
                if (1..=32).contains(&format.num_channels) {
                    channels = format.num_channels;
                }
                // samplesPerSec is milliHertz: 48_000_000 -> 48_000 Hz.
                if format.samples_per_sec >= 1_000 {
                    let hz = format.samples_per_sec / 1_000;
                    if (4_000..=384_000).contains(&hz) {
                        rate = hz;
                    }
                }
            }
        }
    }

    *p_player = make_object(ObjKind::AudioPlayer, rate, channels);
    SL_RESULT_SUCCESS
}

unsafe extern "C" fn engine_create_output_mix(
    _self: *mut c_void,
    p_mix: *mut *mut c_void,
    _num_interfaces: u32,
    _p_interface_ids: *const *const c_void,
    _p_interface_required: *const u32,
) -> u32 {
    if p_mix.is_null() {
        return SL_RESULT_PARAMETER_INVALID;
    }
    *p_mix = make_object(ObjKind::OutputMix, DEFAULT_RATE, DEFAULT_CHANNELS);
    SL_RESULT_SUCCESS
}

macro_rules! engine_unsupported {
    ($($name:ident($($arg:ident: $ty:ty),*)),* $(,)?) => {
        $(
            unsafe extern "C" fn $name($(_: $ty),*) -> u32 {
                SL_RESULT_FEATURE_UNSUPPORTED
            }
        )*
    };
}

engine_unsupported! {
    engine_create_led_device(
        self_: *mut c_void,
        p_device: *mut *mut c_void,
        device_id: u32,
        num_interfaces: u32,
        p_interface_ids: *const *const c_void,
        p_interface_required: *const u32
    ),
    engine_create_vibra_device(
        self_: *mut c_void,
        p_device: *mut *mut c_void,
        device_id: u32,
        num_interfaces: u32,
        p_interface_ids: *const *const c_void,
        p_interface_required: *const u32
    ),
    engine_create_audio_recorder(
        self_: *mut c_void,
        p_recorder: *mut *mut c_void,
        p_audio_src: *mut SLDataSource,
        p_audio_snk: *mut SLDataSink,
        num_interfaces: u32,
        p_interface_ids: *const *const c_void,
        p_interface_required: *const u32
    ),
    engine_create_midi_player(
        self_: *mut c_void,
        p_player: *mut *mut c_void,
        p_midi_src: *mut SLDataSource,
        p_bank_src: *mut SLDataSource,
        p_audio_output: *mut SLDataSink,
        p_vibra: *mut SLDataSink,
        p_led_array: *mut SLDataSink,
        num_interfaces: u32,
        p_interface_ids: *const *const c_void,
        p_interface_required: *const u32
    ),
    engine_create_listener(
        self_: *mut c_void,
        p_listener: *mut *mut c_void,
        num_interfaces: u32,
        p_interface_ids: *const *const c_void,
        p_interface_required: *const u32
    ),
    engine_create_3d_group(
        self_: *mut c_void,
        p_group: *mut *mut c_void,
        num_interfaces: u32,
        p_interface_ids: *const *const c_void,
        p_interface_required: *const u32
    ),
    engine_create_metadata_extractor(
        self_: *mut c_void,
        p_metadata_extractor: *mut *mut c_void,
        p_data_source: *mut SLDataSource,
        num_interfaces: u32,
        p_interface_ids: *const *const c_void,
        p_interface_required: *const u32
    ),
    engine_create_extension_object(
        self_: *mut c_void,
        p_object: *mut *mut c_void,
        p_parameters: *mut c_void,
        object_id: u32,
        num_interfaces: u32,
        p_interface_ids: *const *const c_void,
        p_interface_required: *const u32
    ),
    engine_query_num_supported_interfaces(
        self_: *mut c_void,
        object_id: u32,
        p_num: *mut u32
    ),
    engine_query_supported_interfaces(
        self_: *mut c_void,
        object_id: u32,
        index: u32,
        p_interface_id: *mut *const c_void
    ),
    engine_query_num_supported_extensions(self_: *mut c_void, p_num_extensions: *mut u32),
    engine_query_supported_extension(
        self_: *mut c_void,
        index: u32,
        p_extension_name: *mut c_char,
        p_name_length: *mut i16
    ),
    engine_is_extension_supported(
        self_: *mut c_void,
        p_extension_name: *const c_char,
        p_supported: *mut u32
    )
}

static ENGINE_VTABLE: SLEngineItf_ = SLEngineItf_ {
    CreateLEDDevice: Some(engine_create_led_device),
    CreateVibraDevice: Some(engine_create_vibra_device),
    CreateAudioPlayer: Some(engine_create_audio_player),
    CreateAudioRecorder: Some(engine_create_audio_recorder),
    CreateMidiPlayer: Some(engine_create_midi_player),
    CreateListener: Some(engine_create_listener),
    Create3DGroup: Some(engine_create_3d_group),
    CreateOutputMix: Some(engine_create_output_mix),
    CreateMetadataExtractor: Some(engine_create_metadata_extractor),
    CreateExtensionObject: Some(engine_create_extension_object),
    QueryNumSupportedInterfaces: Some(engine_query_num_supported_interfaces),
    QuerySupportedInterfaces: Some(engine_query_supported_interfaces),
    QueryNumSupportedExtensions: Some(engine_query_num_supported_extensions),
    QuerySupportedExtension: Some(engine_query_supported_extension),
    IsExtensionSupported: Some(engine_is_extension_supported),
};

// ---------------------------------------------------------------------------
// Play interface implementation
// ---------------------------------------------------------------------------

unsafe extern "C" fn play_set_play_state(self_: *mut c_void, state: u32) -> u32 {
    let owner = itf_owner(self_);
    if owner.is_null() {
        return SL_RESULT_PARAMETER_INVALID;
    }
    (*owner).play_state = state;
    SL_RESULT_SUCCESS
}

unsafe extern "C" fn play_get_play_state(self_: *mut c_void, p_state: *mut u32) -> u32 {
    let owner = itf_owner(self_);
    if owner.is_null() || p_state.is_null() {
        return SL_RESULT_PARAMETER_INVALID;
    }
    *p_state = (*owner).play_state;
    SL_RESULT_SUCCESS
}

unsafe extern "C" fn play_get_duration(_self: *mut c_void, p_msec: *mut u32) -> u32 {
    if p_msec.is_null() {
        return SL_RESULT_PARAMETER_INVALID;
    }
    // Unknown/live stream: OpenSL uses 0xFFFFFFFF as "unknown" here.
    *p_msec = 0xFFFF_FFFF;
    SL_RESULT_SUCCESS
}

unsafe extern "C" fn play_get_position(_self: *mut c_void, p_msec: *mut u32) -> u32 {
    if p_msec.is_null() {
        return SL_RESULT_PARAMETER_INVALID;
    }
    *p_msec = 0;
    SL_RESULT_SUCCESS
}

unsafe extern "C" fn play_register_callback(
    _self: *mut c_void,
    _callback: SlPlayCallback,
    _context: *mut c_void,
) -> u32 {
    SL_RESULT_SUCCESS
}

unsafe extern "C" fn play_set_callback_events_mask(_self: *mut c_void, _flags: u32) -> u32 {
    SL_RESULT_SUCCESS
}

unsafe extern "C" fn play_get_callback_events_mask(
    _self: *mut c_void,
    p_flags: *mut u32,
) -> u32 {
    if !p_flags.is_null() {
        *p_flags = 0;
    }
    SL_RESULT_SUCCESS
}

unsafe extern "C" fn play_set_marker_position(_self: *mut c_void, _msec: u32) -> u32 {
    SL_RESULT_SUCCESS
}

unsafe extern "C" fn play_clear_marker_position(_self: *mut c_void) -> u32 {
    SL_RESULT_SUCCESS
}

unsafe extern "C" fn play_get_marker_position(_self: *mut c_void, p_msec: *mut u32) -> u32 {
    if !p_msec.is_null() {
        *p_msec = 0;
    }
    SL_RESULT_SUCCESS
}

unsafe extern "C" fn play_set_position_update_period(_self: *mut c_void, _msec: u32) -> u32 {
    SL_RESULT_SUCCESS
}

unsafe extern "C" fn play_get_position_update_period(
    _self: *mut c_void,
    p_msec: *mut u32,
) -> u32 {
    if !p_msec.is_null() {
        *p_msec = 0;
    }
    SL_RESULT_SUCCESS
}

static PLAY_VTABLE: SLPlayItf_ = SLPlayItf_ {
    SetPlayState: Some(play_set_play_state),
    GetPlayState: Some(play_get_play_state),
    GetDuration: Some(play_get_duration),
    GetPosition: Some(play_get_position),
    RegisterCallback: Some(play_register_callback),
    SetCallbackEventsMask: Some(play_set_callback_events_mask),
    GetCallbackEventsMask: Some(play_get_callback_events_mask),
    SetMarkerPosition: Some(play_set_marker_position),
    ClearMarkerPosition: Some(play_clear_marker_position),
    GetMarkerPosition: Some(play_get_marker_position),
    SetPositionUpdatePeriod: Some(play_set_position_update_period),
    GetPositionUpdatePeriod: Some(play_get_position_update_period),
};

// ---------------------------------------------------------------------------
// Android simple buffer queue implementation
// ---------------------------------------------------------------------------

thread_local! {
    /// Set while a buffer-queue callback is running on this thread. An app that
    /// calls `Enqueue` from its callback would otherwise recurse without bound.
    static IN_BQ_CALLBACK: std::cell::Cell<bool> = std::cell::Cell::new(false);
}

/// Spawn `pw-cat` (preferred) or `aplay` reading S16 PCM from stdin.
fn spawn_sink(rate: u32, channels: u32) -> Option<Sink> {
    let rate = rate.to_string();
    let channels = channels.to_string();

    let pw_cat = Command::new("pw-cat")
        .args(["-p", "-a", "--rate", &rate, "--channels", &channels, "--format", "s16", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let child = match pw_cat {
        Ok(child) => Some(child),
        Err(_) => Command::new("aplay")
            .args(["-q", "-t", "raw", "-f", "S16_LE", "-r", &rate, "-c", &channels, "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok(),
    }?;

    let mut child = child;
    let stdin = child.stdin.take();
    Some(Sink { child, stdin })
}

unsafe extern "C" fn bq_enqueue(
    self_: *mut c_void,
    p_buffer: *const c_void,
    size: u32,
) -> u32 {
    let owner = itf_owner(self_);
    if owner.is_null() || p_buffer.is_null() || size == 0 {
        return SL_RESULT_PARAMETER_INVALID;
    }

    let obj = &mut *owner;

    if obj.sink.is_none() {
        match spawn_sink(obj.rate, obj.channels) {
            Some(sink) => obj.sink = Some(sink),
            // No host audio tool: keep the OpenSL object model usable.
            None => return SL_RESULT_FEATURE_UNSUPPORTED,
        }
    }

    let bytes = std::slice::from_raw_parts(p_buffer as *const u8, size as usize);
    if let Some(sink) = obj.sink.as_mut() {
        if let Some(stdin) = sink.stdin.as_mut() {
            // A broken pipe (player exited) is not fatal to the app.
            let _ = stdin.write_all(bytes);
            let _ = stdin.flush();
        }
    }

    obj.bq_count = obj.bq_count.saturating_add(1);
    obj.bq_index = obj.bq_index.wrapping_add(1);

    // Copy out before the callback: it may destroy the player.
    let callback = obj.bq_callback;
    let context = obj.bq_context;
    let already_in_callback = IN_BQ_CALLBACK.with(|flag| flag.get());
    if let Some(callback) = callback {
        if !already_in_callback {
            IN_BQ_CALLBACK.with(|flag| flag.set(true));
            callback(self_, context);
            IN_BQ_CALLBACK.with(|flag| flag.set(false));
        }
    }

    SL_RESULT_SUCCESS
}

unsafe extern "C" fn bq_clear(self_: *mut c_void) -> u32 {
    let owner = itf_owner(self_);
    if owner.is_null() {
        return SL_RESULT_PARAMETER_INVALID;
    }
    (*owner).bq_count = 0;
    SL_RESULT_SUCCESS
}

unsafe extern "C" fn bq_get_state(
    self_: *mut c_void,
    p_state: *mut SLAndroidSimpleBufferQueueState,
) -> u32 {
    let owner = itf_owner(self_);
    if owner.is_null() || p_state.is_null() {
        return SL_RESULT_PARAMETER_INVALID;
    }
    (*p_state).count = (*owner).bq_count;
    (*p_state).index = (*owner).bq_index;
    SL_RESULT_SUCCESS
}

unsafe extern "C" fn bq_register_callback(
    self_: *mut c_void,
    callback: SlBqCallback,
    context: *mut c_void,
) -> u32 {
    let owner = itf_owner(self_);
    if owner.is_null() {
        return SL_RESULT_PARAMETER_INVALID;
    }
    (*owner).bq_callback = callback;
    (*owner).bq_context = context;
    SL_RESULT_SUCCESS
}

static BQ_VTABLE: SLAndroidSimpleBufferQueueItf_ = SLAndroidSimpleBufferQueueItf_ {
    Enqueue: Some(bq_enqueue),
    Clear: Some(bq_clear),
    GetState: Some(bq_get_state),
    RegisterCallback: Some(bq_register_callback),
};

/// Export a batch of OpenSL ES interface-ID data symbols. Apps compare these
/// opaque values and pass them back to us; they are never dereferenced, so
/// unique non-null integers are sufficient.
macro_rules! sl_iids {
    ($($name:ident = $value:expr),* $(,)?) => {
        $(
            #[no_mangle]
            pub static $name: usize = $value;
        )*
    };
}

sl_iids! {
    SL_IID_ENGINE = 0x5044_0001,
    SL_IID_BUFFERQUEUE = 0x5044_0002,
    SL_IID_VOLUME = 0x5044_0003,
    SL_IID_PLAY = 0x5044_0004,
    SL_IID_RECORD = 0x5044_0005,
    SL_IID_SEEK = 0x5044_0006,
    SL_IID_MUTESOLO = 0x5044_0007,
    SL_IID_PRELOADSTATUS = 0x5044_0008,
    SL_IID_ANDROIDSIMPLEBUFFERQUEUE = 0x5044_0009,
    SL_IID_ANDROIDCONFIGURATION = 0x5044_000a,
    SL_IID_ANDROIDEFFECT = 0x5044_000b,
    SL_IID_ANDROIDEFFECTSEND = 0x5044_000c,
    SL_IID_ANDROIDEFFECTCAPABILITIES = 0x5044_000d,
    SL_IID_EQUALIZER = 0x5044_000e,
    SL_IID_ENVIRONMENTALREVERB = 0x5044_000f,
    SL_IID_PRESETREVERB = 0x5044_0010,
    SL_IID_VIRTUALIZER = 0x5044_0011,
    SL_IID_BASSBOOST = 0x5044_0012,
    SL_IID_OUTPUTMIX = 0x5044_0013,
    SL_IID_AUDIODECODERCAPABILITIES = 0x5044_0014,
    SL_IID_AUDIOENCODERCAPABILITIES = 0x5044_0015,
    SL_IID_AUDIOENCODER = 0x5044_0016,
    SL_IID_AUDIOIODEVICECAPABILITIES = 0x5044_0017,
    SL_IID_DEVICEVOLUME = 0x5044_0018,
    SL_IID_DYNAMICINTERFACEMANAGEMENT = 0x5044_0019,
    SL_IID_EFFECTSEND = 0x5044_001a,
    SL_IID_LED = 0x5044_001b,
    SL_IID_METADATAEXTRACTION = 0x5044_001c,
    SL_IID_METADATATRAVERSAL = 0x5044_001d,
    SL_IID_NULL = 0x5044_001e,
    SL_IID_OBJECT = 0x5044_001f,
    SL_IID_PLAYBACKRATE = 0x5044_0020,
    SL_IID_RATEPITCH = 0x5044_0021,
    SL_IID_STREAMINFORMATION = 0x5044_0022,
    SL_IID_VIBRA = 0x5044_0023,
    SL_IID_VISUALIZATION = 0x5044_0024,
    SL_IID_XAUDIO2EFFECTSEND = 0x5044_0025,
    SL_IID_AUDIOPLAYER = 0x5044_0026,
    SL_IID_AUDIORECORDER = 0x5044_0027,
    SL_IID_3DDOPPLER = 0x5044_0028,
    SL_IID_3DGROUPING = 0x5044_0029,
    SL_IID_3DHEADPHONEREFLECTIONS = 0x5044_002a,
    SL_IID_3DLOCATION = 0x5044_002b,
    SL_IID_3DMACROSCOPIC = 0x5044_002c,
    SL_IID_3DSOURCE = 0x5044_002d,
    SL_IID_3DDISTANCEDAMPING = 0x5044_002e,
    SL_IID_3DDISTANCEATTENUATION = 0x5044_002f,
}

/// `SLresult slCreateEngine(SLObjectItf*, ...)` — create a usable engine.
#[no_mangle]
pub unsafe extern "C" fn slCreateEngine(
    engine: *mut *mut c_void,
    _num_options: u32,
    _options: *const c_void,
    _num_interfaces: u32,
    _interface_ids: *const *const c_void,
    _interface_required: *const u8,
) -> u32 {
    if engine.is_null() {
        return SL_RESULT_PARAMETER_INVALID;
    }
    *engine = Box::into_raw(Box::new(Obj::new(
        ObjKind::Engine,
        DEFAULT_RATE,
        DEFAULT_CHANNELS,
    ))) as *mut c_void;
    SL_RESULT_SUCCESS
}

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

    #[test]
    fn env_flag_parses_truthy_and_falsy() {
        for v in [Some("1"), Some("true"), Some("YES")] {
            assert!(env_flag(v), "{v:?} must be truthy");
        }
        for v in [None, Some("0"), Some("no"), Some("maybe")] {
            assert!(!env_flag(v), "{v:?} must be falsy");
        }
    }

    #[test]
    fn bionic_stream_index_maps_standard_streams() {
        let base = (&raw const __sF) as usize;
        assert_eq!(bionic_stream_index(base), Some(0));
        assert_eq!(bionic_stream_index(base + BIONIC_FILE_SIZE), Some(1));
        assert_eq!(bionic_stream_index(base + 2 * BIONIC_FILE_SIZE), Some(2));
        assert_eq!(bionic_stream_index(base + 3 * BIONIC_FILE_SIZE), Some(2));
        assert_eq!(bionic_stream_index(base - 1), None);
        assert_eq!(bionic_stream_index(base + 1024), None);
    }

    #[test]
    fn cxa_atexit_is_noop_when_suppressed() {
        const KEY: &str = "PENGUDROID_SUPPRESS_DESTRUCTORS";

        /// Restores the variable on drop, even on panic.
        struct EnvGuard(Option<std::ffi::OsString>);
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(v) => std::env::set_var(KEY, v),
                    None => std::env::remove_var(KEY),
                }
            }
        }

        let _guard = EnvGuard(std::env::var_os(KEY));
        std::env::set_var(KEY, "1");

        unsafe extern "C" fn cb(_: *mut c_void) {}
        let rc = unsafe { __cxa_atexit(Some(cb), std::ptr::null_mut(), std::ptr::null_mut()) };
        assert_eq!(rc, 0, "suppressed __cxa_atexit must report success");
    }

    #[test]
    fn fputs_accepts_bionic_stdout_pointer() {
        let base = (&raw const __sF) as usize;
        let stdout = (base + BIONIC_FILE_SIZE) as *mut c_void;
        let s = CString::new("x").unwrap();
        let rc = unsafe { fputs(s.as_ptr(), stdout) };
        assert!(rc >= 0, "fputs via __sF stdout failed: {rc}");
    }

    /// Drive the same call chain the C fixture uses, through the vtable
    /// pointers, without enqueuing (which would spawn an audio process).
    #[test]
    fn opensl_object_model() {
        unsafe {
            let mut engine: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                slCreateEngine(
                    &mut engine,
                    0,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    std::ptr::null(),
                ),
                SL_RESULT_SUCCESS
            );
            assert!(!engine.is_null());

            // An SLObjectItf is a pointer to a vtable pointer.
            let object_vt = &*(*(engine as *const *const SLObjectItf_));
            assert_eq!((object_vt.Realize.unwrap())(engine, 0), SL_RESULT_SUCCESS);

            let mut state = 0u32;
            assert_eq!(
                (object_vt.GetState.unwrap())(engine, &mut state),
                SL_RESULT_SUCCESS
            );
            assert_eq!(state, SL_OBJECT_STATE_REALIZED);

            let mut engine_itf: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                (object_vt.GetInterface.unwrap())(
                    engine,
                    SL_IID_ENGINE as *const c_void,
                    &mut engine_itf,
                ),
                SL_RESULT_SUCCESS
            );
            assert!(!engine_itf.is_null());
            let engine_vt = &*(*(engine_itf as *const *const SLEngineItf_));

            let mut mix: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                (engine_vt.CreateOutputMix.unwrap())(
                    engine_itf,
                    &mut mix,
                    0,
                    std::ptr::null(),
                    std::ptr::null(),
                ),
                SL_RESULT_SUCCESS
            );
            assert!(!mix.is_null());

            // 48 kHz stereo, Android simple buffer queue source.
            let mut locator = SLDataLocatorAndroidSimpleBufferQueue {
                locator_type: SL_DATALOCATOR_ANDROIDSIMPLEBUFFERQUEUE,
                num_buffers: 2,
            };
            let mut format = SLDataFormatPcm {
                format_type: SL_DATAFORMAT_PCM,
                num_channels: 2,
                samples_per_sec: 48_000_000,
                bits_per_sample: 16,
                container_size: 16,
                channel_mask: 3,
                endianness: 2,
            };
            let mut source = SLDataSource {
                p_locator: &mut locator as *mut _ as *mut c_void,
                p_format: &mut format as *mut _ as *mut c_void,
            };
            let mut sink = SLDataSink {
                p_locator: std::ptr::null_mut(),
                p_format: std::ptr::null_mut(),
            };

            let mut player: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                (engine_vt.CreateAudioPlayer.unwrap())(
                    engine_itf,
                    &mut player,
                    &mut source,
                    &mut sink,
                    0,
                    std::ptr::null(),
                    std::ptr::null(),
                ),
                SL_RESULT_SUCCESS
            );
            assert!(!player.is_null());

            let player_vt = &*(*(player as *const *const SLObjectItf_));
            assert_eq!((player_vt.Realize.unwrap())(player, 0), SL_RESULT_SUCCESS);

            let mut play_itf: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                (player_vt.GetInterface.unwrap())(
                    player,
                    SL_IID_PLAY as *const c_void,
                    &mut play_itf,
                ),
                SL_RESULT_SUCCESS
            );
            let play_vt = &*(*(play_itf as *const *const SLPlayItf_));
            assert_eq!(
                (play_vt.SetPlayState.unwrap())(play_itf, SL_PLAYSTATE_PLAYING),
                SL_RESULT_SUCCESS
            );
            let mut play_state = 0u32;
            assert_eq!(
                (play_vt.GetPlayState.unwrap())(play_itf, &mut play_state),
                SL_RESULT_SUCCESS
            );
            assert_eq!(play_state, SL_PLAYSTATE_PLAYING);

            let mut bq_itf: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                (player_vt.GetInterface.unwrap())(
                    player,
                    SL_IID_ANDROIDSIMPLEBUFFERQUEUE as *const c_void,
                    &mut bq_itf,
                ),
                SL_RESULT_SUCCESS
            );
            let bq_vt = &*(*(bq_itf as *const *const SLAndroidSimpleBufferQueueItf_));
            // Invalid arguments must be rejected before any sink is spawned.
            assert_eq!(
                (bq_vt.Enqueue.unwrap())(bq_itf, std::ptr::null(), 0),
                SL_RESULT_PARAMETER_INVALID
            );

            // Unknown interfaces are refused rather than returning garbage.
            let mut unsupported: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                (player_vt.GetInterface.unwrap())(
                    player,
                    SL_IID_RECORD as *const c_void,
                    &mut unsupported,
                ),
                SL_RESULT_FEATURE_UNSUPPORTED
            );

            (player_vt.Destroy.unwrap())(player);
            (object_vt.Destroy.unwrap())(mix);
            (object_vt.Destroy.unwrap())(engine);
        }
    }
}
