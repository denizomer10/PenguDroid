//! Android NDK surface used by native apps: `AAssetManager` (backed by the
//! APK), `ANativeWindow`, `ANativeActivity` and `ALooper`.
//!
//! These are the entry points a `NativeActivity`-based game calls. The shim
//! implements them on top of the host: assets are read straight out of the APK
//! (see [`crate::zip`]), the window is the PenguDroid surface (its geometry
//! comes from `PENGUDROID_SURFACE_W`/`_H`, and EGL is handled by
//! `pengudroid-gleshim`), and the looper is a real `poll(2)` loop so an app's
//! main thread blocks instead of spinning.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::zip;

// ---------------------------------------------------------------------------
// AAssetManager (<android/asset_manager.h>)
// ---------------------------------------------------------------------------

/// `AAssetManager_open` modes (only the buffered behaviour is implemented).
#[allow(dead_code)]
pub const AASSET_MODE_UNKNOWN: c_int = 0;
#[allow(dead_code)]
pub const AASSET_MODE_RANDOM: c_int = 1;
#[allow(dead_code)]
pub const AASSET_MODE_STREAMING: c_int = 2;
#[allow(dead_code)]
pub const AASSET_MODE_BUFFER: c_int = 3;

struct AssetManager {
    archive: Option<zip::Archive>,
    /// Kept for diagnostics (`AAssetManager` has no accessor for it).
    #[allow(dead_code)]
    apk: Option<String>,
}

struct Asset {
    data: Vec<u8>,
    pos: usize,
}

struct AssetDir {
    names: Vec<String>,
    index: usize,
    current: Option<CString>,
}

static ASSET_MANAGER: OnceLock<AssetManager> = OnceLock::new();

fn asset_manager() -> &'static AssetManager {
    ASSET_MANAGER.get_or_init(|| {
        let apk = std::env::var("PENGUDROID_APK").ok().filter(|p| !p.is_empty());
        let archive = match apk.as_deref() {
            Some(path) => match zip::Archive::open(path) {
                Ok(a) => Some(a),
                Err(e) => {
                    crate::log_to_stderr(
                        crate::ANDROID_LOG_WARN,
                        "PenguDroid",
                        &format!("asset manager: {e}"),
                    );
                    None
                }
            },
            None => None,
        };
        AssetManager { archive, apk }
    })
}

/// `AAssetManager_fromJava(env, mgr)` — the process-wide manager.
#[no_mangle]
pub unsafe extern "C" fn AAssetManager_fromJava(
    _env: *mut c_void,
    _mgr: *mut c_void,
) -> *mut c_void {
    asset_manager() as *const AssetManager as *mut c_void
}

/// `AAssetManager_open(mgr, filename, mode)`.
#[no_mangle]
pub unsafe extern "C" fn AAssetManager_open(
    _mgr: *mut c_void,
    filename: *const c_char,
    _mode: c_int,
) -> *mut c_void {
    if filename.is_null() {
        return std::ptr::null_mut();
    }
    let name = CStr::from_ptr(filename).to_string_lossy();
    let name = name.trim_start_matches('/');
    let Some(archive) = asset_manager().archive.as_ref() else {
        return std::ptr::null_mut();
    };
    match archive.read(name) {
        Ok(data) => Box::into_raw(Box::new(Asset { data, pos: 0 })) as *mut c_void,
        Err(_) => std::ptr::null_mut(),
    }
}

unsafe fn asset_ref<'a>(ptr: *mut c_void) -> Option<&'a mut Asset> {
    if ptr.is_null() {
        None
    } else {
        Some(&mut *(ptr as *mut Asset))
    }
}

/// `AAsset_getLength(asset)`.
#[no_mangle]
pub unsafe extern "C" fn AAsset_getLength(asset: *mut c_void) -> i64 {
    asset_ref(asset).map(|a| a.data.len() as i64).unwrap_or(0)
}

/// `AAsset_getLength64(asset)`.
#[no_mangle]
pub unsafe extern "C" fn AAsset_getLength64(asset: *mut c_void) -> i64 {
    AAsset_getLength(asset)
}

/// `AAsset_getRemainingLength(asset)`.
#[no_mangle]
pub unsafe extern "C" fn AAsset_getRemainingLength(asset: *mut c_void) -> i32 {
    asset_ref(asset)
        .map(|a| (a.data.len().saturating_sub(a.pos)) as i32)
        .unwrap_or(0)
}

/// `AAsset_getRemainingLength64(asset)`.
#[no_mangle]
pub unsafe extern "C" fn AAsset_getRemainingLength64(asset: *mut c_void) -> i64 {
    asset_ref(asset)
        .map(|a| a.data.len().saturating_sub(a.pos) as i64)
        .unwrap_or(0)
}

/// `AAsset_getBuffer(asset)` — the whole asset, mapped in memory.
#[no_mangle]
pub unsafe extern "C" fn AAsset_getBuffer(asset: *mut c_void) -> *const c_void {
    asset_ref(asset)
        .map(|a| a.data.as_ptr() as *const c_void)
        .unwrap_or(std::ptr::null())
}

/// `AAsset_read(asset, buf, count)`.
#[no_mangle]
pub unsafe extern "C" fn AAsset_read(
    asset_ptr: *mut c_void,
    buf: *mut c_void,
    count: usize,
) -> c_int {
    let Some(a) = asset_ref(asset_ptr) else { return -1 };
    if buf.is_null() {
        return -1;
    }
    let remaining = a.data.len().saturating_sub(a.pos);
    let n = remaining.min(count);
    std::ptr::copy_nonoverlapping(a.data.as_ptr().add(a.pos), buf as *mut u8, n);
    a.pos += n;
    n as c_int
}

/// `AAsset_seek(asset, offset, whence)` — returns the new position or `-1`.
#[no_mangle]
pub unsafe extern "C" fn AAsset_seek(asset_ptr: *mut c_void, offset: i32, whence: c_int) -> i32 {
    let Some(a) = asset_ref(asset_ptr) else { return -1 };
    let base = match whence {
        libc::SEEK_SET => 0i64,
        libc::SEEK_CUR => a.pos as i64,
        libc::SEEK_END => a.data.len() as i64,
        _ => return -1,
    };
    let target = base + offset as i64;
    if target < 0 || target > a.data.len() as i64 {
        return -1;
    }
    a.pos = target as usize;
    a.pos as i32
}

/// `AAsset_seek64(asset, offset, whence)`.
#[no_mangle]
pub unsafe extern "C" fn AAsset_seek64(asset_ptr: *mut c_void, offset: i64, whence: c_int) -> i64 {
    let Some(a) = asset_ref(asset_ptr) else { return -1 };
    let base = match whence {
        libc::SEEK_SET => 0i64,
        libc::SEEK_CUR => a.pos as i64,
        libc::SEEK_END => a.data.len() as i64,
        _ => return -1,
    };
    let target = base + offset;
    if target < 0 || target > a.data.len() as i64 {
        return -1;
    }
    a.pos = target as usize;
    a.pos as i64
}

/// `AAsset_close(asset)`.
#[no_mangle]
pub unsafe extern "C" fn AAsset_close(asset_ptr: *mut c_void) {
    if !asset_ptr.is_null() {
        drop(Box::from_raw(asset_ptr as *mut Asset));
    }
}

/// `AAsset_isAllocated(asset)` — the buffer is ours, so always true.
#[no_mangle]
pub unsafe extern "C" fn AAsset_isAllocated(_asset: *mut c_void) -> c_int {
    1
}

/// `AAsset_openFileDescriptor(asset, outStart, outLength)` — unsupported
/// (assets are decompressed in memory, not mapped from the APK).
#[no_mangle]
pub unsafe extern "C" fn AAsset_openFileDescriptor(
    _asset: *mut c_void,
    _out_start: *mut i32,
    _out_length: *mut i32,
) -> c_int {
    -1
}

/// `AAsset_openFileDescriptor64(asset, outStart, outLength)`.
#[no_mangle]
pub unsafe extern "C" fn AAsset_openFileDescriptor64(
    _asset: *mut c_void,
    _out_start: *mut i64,
    _out_length: *mut i64,
) -> c_int {
    -1
}

/// `AAssetManager_openDir(mgr, dirName)`.
#[no_mangle]
pub unsafe extern "C" fn AAssetManager_openDir(
    _mgr: *mut c_void,
    dir_name: *const c_char,
) -> *mut c_void {
    let name = if dir_name.is_null() {
        String::new()
    } else {
        CStr::from_ptr(dir_name).to_string_lossy().into_owned()
    };
    let names = asset_manager()
        .archive
        .as_ref()
        .map(|a| a.list_dir(name.trim_matches('/')))
        .unwrap_or_default();
    Box::into_raw(Box::new(AssetDir { names, index: 0, current: None })) as *mut c_void
}

/// `AAssetDir_getNextFileName(dir)` — `NULL` at the end.
#[no_mangle]
pub unsafe extern "C" fn AAssetDir_getNextFileName(dir: *mut c_void) -> *const c_char {
    if dir.is_null() {
        return std::ptr::null();
    }
    let dir = &mut *(dir as *mut AssetDir);
    let Some(name) = dir.names.get(dir.index).cloned() else {
        return std::ptr::null();
    };
    dir.index += 1;
    match CString::new(name) {
        Ok(c) => {
            dir.current = Some(c);
            dir.current.as_ref().unwrap().as_ptr()
        }
        Err(_) => std::ptr::null(),
    }
}

/// `AAssetDir_rewind(dir)`.
#[no_mangle]
pub unsafe extern "C" fn AAssetDir_rewind(dir: *mut c_void) {
    if !dir.is_null() {
        let dir = &mut *(dir as *mut AssetDir);
        dir.index = 0;
    }
}

/// `AAssetDir_close(dir)`.
#[no_mangle]
pub unsafe extern "C" fn AAssetDir_close(dir: *mut c_void) {
    if !dir.is_null() {
        drop(Box::from_raw(dir as *mut AssetDir));
    }
}

// ---------------------------------------------------------------------------
// ANativeWindow (<android/native_window.h>)
// ---------------------------------------------------------------------------

/// A non-null stand-in for the app's window. `pengudroid-gleshim` ignores the
/// pointer (it renders into its own FBO and presents dma-bufs), but apps and
/// EGL implementations require it to be non-null.
static WINDOW_SENTINEL: u8 = 0;

fn surface_dimension(var: &str, default: i32) -> i32 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.trim().parse::<i32>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

/// `ANativeWindow_fromSurface(env, surface)`.
#[no_mangle]
pub unsafe extern "C" fn ANativeWindow_fromSurface(
    _env: *mut c_void,
    _surface: *mut c_void,
) -> *mut c_void {
    std::ptr::addr_of!(WINDOW_SENTINEL) as *mut c_void
}

/// `ANativeWindow_acquire(w)`.
#[no_mangle]
pub unsafe extern "C" fn ANativeWindow_acquire(_w: *mut c_void) {}

/// `ANativeWindow_release(w)`.
#[no_mangle]
pub unsafe extern "C" fn ANativeWindow_release(_w: *mut c_void) {}

/// `ANativeWindow_getWidth(w)`.
#[no_mangle]
pub unsafe extern "C" fn ANativeWindow_getWidth(_w: *mut c_void) -> i32 {
    surface_dimension("PENGUDROID_SURFACE_W", 1280)
}

/// `ANativeWindow_getHeight(w)`.
#[no_mangle]
pub unsafe extern "C" fn ANativeWindow_getHeight(_w: *mut c_void) -> i32 {
    surface_dimension("PENGUDROID_SURFACE_H", 720)
}

/// `ANativeWindow_getFormat(w)` — `WINDOW_FORMAT_RGBA_8888`.
#[no_mangle]
pub unsafe extern "C" fn ANativeWindow_getFormat(_w: *mut c_void) -> i32 {
    1
}

/// `ANativeWindow_setBuffersGeometry(w, width, height, format)`.
#[no_mangle]
pub unsafe extern "C" fn ANativeWindow_setBuffersGeometry(
    _w: *mut c_void,
    _width: i32,
    _height: i32,
    _format: i32,
) -> i32 {
    0
}

/// `ANativeWindow_setBuffersTransform(w, transform)`.
#[no_mangle]
pub unsafe extern "C" fn ANativeWindow_setBuffersTransform(_w: *mut c_void, _t: i32) -> i32 {
    0
}

/// `ANativeWindow_setBuffersDataSpace(w, dataSpace)`.
#[no_mangle]
pub unsafe extern "C" fn ANativeWindow_setBuffersDataSpace(_w: *mut c_void, _d: i32) -> i32 {
    0
}

/// `ANativeWindow_setFrameRate(w, frameRate, compatibility)`.
#[no_mangle]
pub unsafe extern "C" fn ANativeWindow_setFrameRate(
    _w: *mut c_void,
    _frame_rate: f32,
    _compatibility: i8,
) -> i32 {
    0
}

/// `ANativeWindow_lock(w, outBuffer, inOutDirtyBounds)` — software rendering is
/// not supported; the GPU path is the supported one.
#[no_mangle]
pub unsafe extern "C" fn ANativeWindow_lock(
    _w: *mut c_void,
    _out_buffer: *mut c_void,
    _in_out_dirty_bounds: *mut c_void,
) -> i32 {
    -1
}

/// `ANativeWindow_unlockAndPost(w)`.
#[no_mangle]
pub unsafe extern "C" fn ANativeWindow_unlockAndPost(_w: *mut c_void) -> i32 {
    -1
}

// ---------------------------------------------------------------------------
// ANativeActivity (<android/native_activity.h>)
// ---------------------------------------------------------------------------

/// Set by `ANativeActivity_finish`; the host polls it through
/// [`__pengudroid_native_should_finish`].
static FINISH_REQUESTED: AtomicBool = AtomicBool::new(false);

/// `ANativeActivity_finish(activity)` — ask the host to end the app.
#[no_mangle]
pub unsafe extern "C" fn ANativeActivity_finish(_activity: *mut c_void) {
    FINISH_REQUESTED.store(true, Ordering::SeqCst);
}

/// Host-side query: has the app asked to finish?
#[no_mangle]
pub extern "C" fn __pengudroid_native_should_finish() -> c_int {
    FINISH_REQUESTED.load(Ordering::SeqCst) as c_int
}

/// Host-side reset, so a second app in the same process starts clean.
#[no_mangle]
pub extern "C" fn __pengudroid_native_reset_finish() {
    FINISH_REQUESTED.store(false, Ordering::SeqCst);
}

/// `ANativeActivity_setWindowFlags(activity, addFlags, removeFlags)`.
#[no_mangle]
pub unsafe extern "C" fn ANativeActivity_setWindowFlags(
    _activity: *mut c_void,
    _add_flags: u32,
    _remove_flags: u32,
) {
}

/// `ANativeActivity_showSoftInput(activity, flags)`.
#[no_mangle]
pub unsafe extern "C" fn ANativeActivity_showSoftInput(_activity: *mut c_void, _flags: u32) {}

/// `ANativeActivity_hideSoftInput(activity, flags)`.
#[no_mangle]
pub unsafe extern "C" fn ANativeActivity_hideSoftInput(_activity: *mut c_void, _flags: u32) {}

/// `ANativeActivity_setWindowFormat(activity, format)`.
#[no_mangle]
pub unsafe extern "C" fn ANativeActivity_setWindowFormat(_activity: *mut c_void, _format: i32) {}

// ---------------------------------------------------------------------------
// ALooper (<android/looper.h>)
// ---------------------------------------------------------------------------

pub const ALOOPER_POLL_WAKE: c_int = -1;
pub const ALOOPER_POLL_CALLBACK: c_int = -2;
pub const ALOOPER_POLL_TIMEOUT: c_int = -3;
pub const ALOOPER_POLL_ERROR: c_int = -4;

pub const ALOOPER_EVENT_INPUT: c_int = 1;
pub const ALOOPER_EVENT_OUTPUT: c_int = 2;
pub const ALOOPER_EVENT_ERROR: c_int = 4;
pub const ALOOPER_EVENT_HANGUP: c_int = 8;
pub const ALOOPER_EVENT_INVALID: c_int = 16;

type LooperCallback = Option<unsafe extern "C" fn(c_int, c_int, *mut c_void) -> c_int>;

struct FdEntry {
    fd: RawFd,
    ident: c_int,
    events: c_int,
    callback: LooperCallback,
    data: *mut c_void,
}

// The callback and data pointers are owned by the app, which keeps them alive
// for as long as the fd is registered.
unsafe impl Send for FdEntry {}

struct Looper {
    fds: Mutex<Vec<FdEntry>>,
    wake_read: OwnedFd,
    wake_write: OwnedFd,
}

static LOOPER: OnceLock<Looper> = OnceLock::new();

fn looper() -> &'static Looper {
    LOOPER.get_or_init(|| {
        let mut pipe = [0 as RawFd; 2];
        // SAFETY: `pipe` is a two-element array for `pipe(2)`.
        let rc = unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
        if rc != 0 {
            // Fall back to /dev/null descriptors so the looper still works.
            let devnull = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
            pipe = [devnull, devnull];
        }
        // SAFETY: both descriptors are fresh and owned by us.
        let (r, w) = unsafe { (OwnedFd::from_raw_fd(pipe[0]), OwnedFd::from_raw_fd(pipe[1])) };
        Looper { fds: Mutex::new(Vec::new()), wake_read: r, wake_write: w }
    })
}

/// `ALooper_prepare(opts)` — the process-wide looper.
#[no_mangle]
pub unsafe extern "C" fn ALooper_prepare(_opts: c_int) -> *mut c_void {
    looper() as *const Looper as *mut c_void
}

/// `ALooper_forThread()` — the same looper (PenguDroid runs one per process).
#[no_mangle]
pub unsafe extern "C" fn ALooper_forThread() -> *mut c_void {
    looper() as *const Looper as *mut c_void
}

/// `ALooper_acquire(looper)`.
#[no_mangle]
pub unsafe extern "C" fn ALooper_acquire(_looper: *mut c_void) {}

/// `ALooper_release(looper)`.
#[no_mangle]
pub unsafe extern "C" fn ALooper_release(_looper: *mut c_void) {}

/// `ALooper_addFd(looper, fd, ident, events, callback, data)`.
#[no_mangle]
pub unsafe extern "C" fn ALooper_addFd(
    _looper: *mut c_void,
    fd: c_int,
    ident: c_int,
    events: c_int,
    callback: LooperCallback,
    data: *mut c_void,
) -> c_int {
    let mut fds = looper().fds.lock().unwrap_or_else(|e| e.into_inner());
    fds.retain(|e| e.fd != fd);
    fds.push(FdEntry { fd, ident, events, callback, data });
    1
}

/// `ALooper_removeFd(looper, fd)`.
#[no_mangle]
pub unsafe extern "C" fn ALooper_removeFd(_looper: *mut c_void, fd: c_int) -> c_int {
    let mut fds = looper().fds.lock().unwrap_or_else(|e| e.into_inner());
    let before = fds.len();
    fds.retain(|e| e.fd != fd);
    (before != fds.len()) as c_int
}

fn poll_events(events: c_int) -> libc::c_short {
    let mut out = 0;
    if events & ALOOPER_EVENT_INPUT != 0 {
        out |= libc::POLLIN;
    }
    if events & ALOOPER_EVENT_OUTPUT != 0 {
        out |= libc::POLLOUT;
    }
    out
}

fn looper_events(revents: libc::c_short) -> c_int {
    let mut out = 0;
    if revents & libc::POLLIN != 0 {
        out |= ALOOPER_EVENT_INPUT;
    }
    if revents & libc::POLLOUT != 0 {
        out |= ALOOPER_EVENT_OUTPUT;
    }
    if revents & libc::POLLERR != 0 {
        out |= ALOOPER_EVENT_ERROR;
    }
    if revents & libc::POLLHUP != 0 {
        out |= ALOOPER_EVENT_HANGUP;
    }
    if revents & libc::POLLNVAL != 0 {
        out |= ALOOPER_EVENT_INVALID;
    }
    out
}

/// `ALooper_pollOnce(timeoutMillis, outFd, outEvents, outData)`.
#[no_mangle]
pub unsafe extern "C" fn ALooper_pollOnce(
    timeout_millis: c_int,
    out_fd: *mut c_int,
    out_events: *mut c_int,
    out_data: *mut *mut c_void,
) -> c_int {
    let l = looper();
    let entries: Vec<(RawFd, c_int, c_int, LooperCallback, *mut c_void)> = {
        let fds = l.fds.lock().unwrap_or_else(|e| e.into_inner());
        fds.iter().map(|e| (e.fd, e.ident, e.events, e.callback, e.data)).collect()
    };

    let mut pollfds: Vec<libc::pollfd> = Vec::with_capacity(entries.len() + 1);
    pollfds.push(libc::pollfd {
        fd: l.wake_read.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    });
    for (fd, _, events, _, _) in &entries {
        pollfds.push(libc::pollfd { fd: *fd, events: poll_events(*events), revents: 0 });
    }

    // SAFETY: `pollfds` is a valid array of `pollfd` for its length.
    let rc = libc::poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, timeout_millis);
    if rc < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EINTR) {
            return ALOOPER_POLL_WAKE;
        }
        return ALOOPER_POLL_ERROR;
    }
    if rc == 0 {
        return ALOOPER_POLL_TIMEOUT;
    }

    if pollfds[0].revents & libc::POLLIN != 0 {
        let mut buf = [0u8; 64];
        // SAFETY: draining the non-blocking wake pipe into a local buffer.
        let _ = libc::read(l.wake_read.as_raw_fd(), buf.as_mut_ptr() as *mut c_void, buf.len());
        return ALOOPER_POLL_WAKE;
    }

    for (i, (fd, ident, _, callback, data)) in entries.iter().enumerate() {
        let revents = pollfds[i + 1].revents;
        if revents == 0 {
            continue;
        }
        let events = looper_events(revents);
        match callback {
            Some(cb) => {
                let _ = cb(*fd, events, *data);
                return ALOOPER_POLL_CALLBACK;
            }
            None => {
                if !out_fd.is_null() {
                    *out_fd = *fd;
                }
                if !out_events.is_null() {
                    *out_events = events;
                }
                if !out_data.is_null() {
                    *out_data = *data;
                }
                return *ident;
            }
        }
    }
    ALOOPER_POLL_TIMEOUT
}

/// `ALooper_pollAll(timeoutMillis, outFd, outEvents, outData)`.
#[no_mangle]
pub unsafe extern "C" fn ALooper_pollAll(
    timeout_millis: c_int,
    out_fd: *mut c_int,
    out_events: *mut c_int,
    out_data: *mut *mut c_void,
) -> c_int {
    let start = std::time::Instant::now();
    loop {
        let remaining = if timeout_millis < 0 {
            -1
        } else {
            let elapsed = start.elapsed().as_millis() as c_int;
            (timeout_millis - elapsed).max(0)
        };
        let r = ALooper_pollOnce(remaining, out_fd, out_events, out_data);
        if r != ALOOPER_POLL_CALLBACK {
            return r;
        }
        if timeout_millis >= 0 && start.elapsed().as_millis() as c_int >= timeout_millis {
            return ALOOPER_POLL_TIMEOUT;
        }
    }
}

/// `ALooper_wake(looper)`.
#[no_mangle]
pub unsafe extern "C" fn ALooper_wake(_looper: *mut c_void) {
    let byte = 1u8;
    // SAFETY: writing one byte to our own non-blocking pipe.
    let _ = libc::write(
        looper().wake_write.as_raw_fd(),
        &byte as *const u8 as *const c_void,
        1,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_manager_without_apk_is_empty() {
        // No PENGUDROID_APK in the test environment: opening must fail cleanly.
        let name = CString::new("assets/x").unwrap();
        let a = unsafe { AAssetManager_open(std::ptr::null_mut(), name.as_ptr(), 0) };
        assert!(a.is_null());
        let dir = unsafe { AAssetManager_openDir(std::ptr::null_mut(), std::ptr::null()) };
        assert!(!dir.is_null());
        assert!(unsafe { AAssetDir_getNextFileName(dir) }.is_null());
        unsafe { AAssetDir_close(dir) };
    }

    #[test]
    fn window_geometry_comes_from_the_environment() {
        std::env::set_var("PENGUDROID_SURFACE_W", "1024");
        std::env::set_var("PENGUDROID_SURFACE_H", "768");
        assert_eq!(unsafe { ANativeWindow_getWidth(std::ptr::null_mut()) }, 1024);
        assert_eq!(unsafe { ANativeWindow_getHeight(std::ptr::null_mut()) }, 768);
        std::env::remove_var("PENGUDROID_SURFACE_W");
        std::env::remove_var("PENGUDROID_SURFACE_H");
    }

    #[test]
    fn finish_flag_round_trips() {
        __pengudroid_native_reset_finish();
        assert_eq!(__pengudroid_native_should_finish(), 0);
        unsafe { ANativeActivity_finish(std::ptr::null_mut()) };
        assert_eq!(__pengudroid_native_should_finish(), 1);
        __pengudroid_native_reset_finish();
    }

    #[test]
    fn looper_times_out_and_wakes() {
        let start = std::time::Instant::now();
        let r = unsafe {
            ALooper_pollOnce(20, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut())
        };
        assert_eq!(r, ALOOPER_POLL_TIMEOUT);
        assert!(start.elapsed().as_millis() >= 15);

        unsafe { ALooper_wake(std::ptr::null_mut()) };
        let r = unsafe {
            ALooper_pollOnce(1000, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut())
        };
        assert_eq!(r, ALOOPER_POLL_WAKE);
    }
}
