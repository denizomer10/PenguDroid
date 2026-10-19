//! Running a `NativeActivity` app without ART.
//!
//! Most Android games are Java/Kotlin apps that need ART, but a meaningful class
//! of native games (SDL2, pure-NDK engines) are `android.app.NativeActivity`:
//! the framework loads `libmain.so` and calls `ANativeActivity_onCreate`. That
//! lifecycle is small enough to host directly, so PenguDroid can run those apps
//! with **no Android runtime at all** — the native code talks to the Bionic shim
//! (assets, looper, window) and to `pengudroid-gleshim` (EGL/GLES on the host
//! GPU), and frames reach Wayland as dma-bufs.
//!
//! `pengudroid native app.apk` prepares the library set and re-execs itself as
//! the hidden `__native` command under `LD_PRELOAD`; `__native` builds the
//! `ANativeActivity` structure, drives the lifecycle callbacks, and keeps the
//! process alive until the app calls `ANativeActivity_finish`, the window closes,
//! or `--seconds` elapses.

use std::ffi::{c_char, c_int, c_void, CString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use pengudroid_proto::default_socket_path;

use crate::apk;
use crate::deps::find_shim;
use crate::nativelib::{ensure_compat_dir, host_abi, host_can_load};

/// The symbol every `NativeActivity` library exports.
const ENTRY_SYMBOL: &str = "ANativeActivity_onCreate";

/// Default window size for a native app.
const DEFAULT_W: u32 = 1280;
const DEFAULT_H: u32 = 720;

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

struct Args {
    apk: Option<String>,
    lib: Option<String>,
    abi: Option<String>,
    width: u32,
    height: u32,
    seconds: Option<u64>,
    title: String,
    isolate: bool,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut out = Args {
        apk: None,
        lib: None,
        abi: None,
        width: DEFAULT_W,
        height: DEFAULT_H,
        seconds: None,
        title: "PenguDroid native app".into(),
        isolate: false,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--lib" => out.lib = Some(it.next().ok_or("--lib needs a PATH")?.clone()),
            "--abi" => out.abi = Some(it.next().ok_or("--abi needs a NAME")?.clone()),
            "--width" => {
                out.width = it.next().ok_or("--width needs a number")?.parse().map_err(|_| "bad --width")?
            }
            "--height" => {
                out.height =
                    it.next().ok_or("--height needs a number")?.parse().map_err(|_| "bad --height")?
            }
            "--seconds" => {
                out.seconds =
                    Some(it.next().ok_or("--seconds needs a number")?.parse().map_err(|_| "bad --seconds")?)
            }
            "--title" => out.title = it.next().ok_or("--title needs a value")?.clone(),
            "--isolate" => out.isolate = true,
            other if other.starts_with('-') => return Err(format!("unknown option: {other}")),
            other => out.apk = Some(other.to_string()),
        }
    }
    if out.apk.is_none() && out.lib.is_none() {
        return Err(
            "usage: pengudroid native <app.apk> [--abi ABI] [--width W] [--height H] [--seconds N]\n       \
             pengudroid native --lib libmain.so [--width W] [--height H] [--seconds N]"
                .into(),
        );
    }
    Ok(out)
}

/// `pengudroid native ...` — prepare the library set and supervise the helper.
pub fn native(args: &[String]) -> i32 {
    let args = match parse_args(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("pengudroid: {e}");
            return 2;
        }
    };

    let socket = default_socket_path();
    if let Err(e) = crate::runner::ensure_daemon(&socket) {
        eprintln!("pengudroid: {e}");
        return 1;
    }

    let shim = match find_shim() {
        Some(s) => s,
        None => {
            eprintln!("pengudroid: libpengudroid_shim.so not found (set PENGUDROID_SHIM)");
            return 1;
        }
    };
    let gleshim = match find_gleshim() {
        Some(s) => s,
        None => {
            eprintln!("pengudroid: libpengudroid_gleshim.so not found (set PENGUDROID_GLESHIM)");
            return 1;
        }
    };
    let compat = match ensure_compat_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("pengudroid: {e}");
            return 1;
        }
    };

    let work = std::env::temp_dir().join("pengudroid-native");
    if let Err(e) = std::fs::create_dir_all(&work) {
        eprintln!("pengudroid: cannot create {}: {e}", work.display());
        return 1;
    }

    let (entry, libdir, apk_path) = match prepare_library(&args, &work) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("pengudroid: {e}");
            return 1;
        }
    };

    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("pengudroid: cannot find own exe: {e}");
            return 1;
        }
    };

    let mut cmd = Command::new(exe);
    cmd.arg("__native")
        .arg("--lib")
        .arg(&entry)
        .arg("--width")
        .arg(args.width.to_string())
        .arg("--height")
        .arg(args.height.to_string())
        .arg("--title")
        .arg(&args.title);
    if let Some(secs) = args.seconds {
        cmd.arg("--seconds").arg(secs.to_string());
    }
    let preload = format!("{}:{}", shim.display(), gleshim.display());
    let ld_path = format!("{}:{}", compat.display(), libdir.display());
    cmd.env("LD_PRELOAD", preload)
        .env("LD_LIBRARY_PATH", ld_path)
        .env("PENGUDROID_SURFACE_W", args.width.to_string())
        .env("PENGUDROID_SURFACE_H", args.height.to_string())
        .env("PENGUDROID_SUPPRESS_DESTRUCTORS", "1");
    if args.isolate {
        cmd.env("PENGUDROID_ISOLATE", "1");
    }
    if let Some(apk) = &apk_path {
        cmd.env("PENGUDROID_APK", apk);
    }

    println!("native app: {}", entry.display());
    println!("window:     {}x{}", args.width, args.height);
    if args.isolate {
        println!(
            "isolation: seccomp filter ({} syscalls denied) + no_new_privs",
            crate::isolate::denied_syscalls().len()
        );
    }
    println!("the window is live; press Ctrl-C to stop the app");

    // SAFETY: correct handler signature.
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("pengudroid: cannot start the native host: {e}");
            return 1;
        }
    };
    loop {
        if STOP.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            println!("stopped native app");
            return 0;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    println!("native app exited: {status}");
                    return 0;
                }
                eprintln!("native app exited: {status}");
                return 1;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => {
                eprintln!("pengudroid: waiting on the native host failed: {e}");
                return 1;
            }
        }
    }
}

/// Locate `libpengudroid_gleshim.so` next to the CLI (or via the environment).
fn find_gleshim() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("PENGUDROID_GLESHIM") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(std::fs::canonicalize(&p).unwrap_or(p));
        }
    }
    let exe = std::env::current_exe().ok()?;
    let candidate = exe.parent()?.join("libpengudroid_gleshim.so");
    candidate.exists().then(|| std::fs::canonicalize(&candidate).unwrap_or(candidate))
}

/// Extract (or copy) the app's libraries, strip Android versioning, and pick the
/// one that exports `ANativeActivity_onCreate`.
fn prepare_library(args: &Args, work: &Path) -> Result<(PathBuf, PathBuf, Option<String>), String> {
    if let Some(lib) = &args.lib {
        let data = std::fs::read(lib).map_err(|e| format!("cannot read {lib}: {e}"))?;
        let info = pengudroid_elf::parse(&data).map_err(|e| format!("{lib}: {e}"))?;
        host_can_load(&info)?;
        let stripped = pengudroid_elf::strip_android_versions(&data)
            .map_err(|e| format!("{lib}: {e}"))?;
        let dir = work.join("single");
        std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let out = dir.join(
            Path::new(lib).file_name().map(|s| s.to_os_string()).unwrap_or_else(|| "libmain.so".into()),
        );
        std::fs::write(&out, &stripped).map_err(|e| format!("cannot write {}: {e}", out.display()))?;
        if !info.exports.iter().any(|s| s == ENTRY_SYMBOL) {
            return Err(format!("{lib} does not export {ENTRY_SYMBOL}"));
        }
        return Ok((out, dir, None));
    }

    let apk_path = args.apk.clone().expect("parse_args guarantees an apk");
    let libs = apk::native_libs(&apk_path)?;
    if libs.is_empty() {
        return Err(format!("{apk_path}: no native libraries (lib/<abi>/*.so)"));
    }
    let abi = args.abi.clone().unwrap_or_else(|| host_abi().to_string());
    let selected: Vec<&apk::NativeLib> = libs.iter().filter(|l| l.abi == abi).collect();
    if selected.is_empty() {
        let mut abis: Vec<&str> = libs.iter().map(|l| l.abi.as_str()).collect();
        abis.sort_unstable();
        abis.dedup();
        return Err(format!(
            "{apk_path}: no libraries for abi '{abi}' (available: {})",
            abis.join(", ")
        ));
    }

    let dir = work.join(format!("apk-{abi}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;

    let mut entry: Option<PathBuf> = None;
    let mut preferred: Option<PathBuf> = None;
    for lib in &selected {
        let bytes = apk::extract_named(&apk_path, &lib.entry)?;
        let stripped = pengudroid_elf::strip_android_versions(&bytes)
            .map_err(|e| format!("{}: {e}", lib.file))?;
        let out = dir.join(&lib.file);
        std::fs::write(&out, &stripped)
            .map_err(|e| format!("cannot write {}: {e}", out.display()))?;
        if let Ok(info) = pengudroid_elf::parse(&bytes) {
            if info.exports.iter().any(|s| s == ENTRY_SYMBOL) {
                if entry.is_none() {
                    entry = Some(out.clone());
                }
                if lib.file == "libmain.so" {
                    preferred = Some(out.clone());
                }
            }
        }
    }
    let entry = preferred.or(entry).ok_or_else(|| {
        format!(
            "{apk_path}: no library for {abi} exports {ENTRY_SYMBOL}; \
             this is not a NativeActivity app (it needs ART, which is not available yet)"
        )
    })?;
    Ok((entry, dir, Some(apk_path)))
}

// ---------------------------------------------------------------------------
// The hidden `__native` host
// ---------------------------------------------------------------------------

#[repr(C)]
struct ARect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

type ActivityFn = Option<extern "C" fn(*mut ANativeActivity)>;
type WindowFn = Option<extern "C" fn(*mut ANativeActivity, *mut c_void)>;
type FocusFn = Option<extern "C" fn(*mut ANativeActivity, c_int)>;
type SaveFn = Option<extern "C" fn(*mut ANativeActivity, *mut usize) -> *mut c_void>;
type RectFn = Option<extern "C" fn(*mut ANativeActivity, *const ARect)>;

#[repr(C)]
struct ANativeActivityCallbacks {
    on_start: ActivityFn,
    on_resume: ActivityFn,
    on_save_instance_state: SaveFn,
    on_pause: ActivityFn,
    on_stop: ActivityFn,
    on_destroy: ActivityFn,
    on_window_focus_changed: FocusFn,
    on_native_window_created: WindowFn,
    on_native_window_resized: WindowFn,
    on_native_window_redraw_needed: WindowFn,
    on_native_window_destroyed: WindowFn,
    on_input_queue_created: WindowFn,
    on_input_queue_destroyed: WindowFn,
    on_content_rect_changed: RectFn,
    on_configuration_changed: ActivityFn,
    on_low_memory: ActivityFn,
}

#[repr(C)]
struct ANativeActivity {
    callbacks: *mut ANativeActivityCallbacks,
    vm: *mut c_void,
    env: *mut c_void,
    clazz: *mut c_void,
    internal_data_path: *const c_char,
    external_data_path: *const c_char,
    sdk_version: i32,
    instance: *mut c_void,
    asset_manager: *mut c_void,
    obb_path: *const c_char,
}

type OnCreate = unsafe extern "C" fn(*mut ANativeActivity, *mut c_void, usize);
type ShouldFinish = unsafe extern "C" fn() -> c_int;
type ResetFinish = unsafe extern "C" fn();
type AssetManagerFromJava = unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void;

/// `pengudroid __native ...` — the process that actually hosts the app.
pub fn native_main(args: &[String]) -> i32 {
    let args = match parse_args(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("native: {e}");
            return 2;
        }
    };
    let Some(lib) = args.lib.clone() else {
        eprintln!("native: --lib is required");
        return 2;
    };

    // SAFETY: correct handler signature.
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }

    if std::env::var_os("PENGUDROID_ISOLATE").is_some() {
        match crate::isolate::apply() {
            Ok(()) => eprintln!("native: isolation active (seccomp + no_new_privs)"),
            Err(e) => eprintln!("native: warning: isolation unavailable: {e}"),
        }
    }

    let cpath = match CString::new(lib.clone()) {
        Ok(c) => c,
        Err(_) => {
            eprintln!("native: path contains a NUL byte");
            return 2;
        }
    };
    // SAFETY: `cpath` is a valid C string; the library is loaded for the life of
    // the process (never `dlclose`d, since its destructors assume Bionic state).
    let handle = unsafe { libc::dlopen(cpath.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
    if handle.is_null() {
        eprintln!("native: dlopen failed: {}", dlerror());
        return 1;
    }
    let entry = unsafe { libc::dlsym(handle, c"ANativeActivity_onCreate".as_ptr()) };
    if entry.is_null() {
        eprintln!("native: {lib} does not export {ENTRY_SYMBOL}");
        return 1;
    }
    // SAFETY: the symbol has the documented `ANativeActivity_onCreate` signature.
    let on_create: OnCreate = unsafe { std::mem::transmute(entry) };

    // The shim (preloaded) owns the finish flag and the asset manager.
    let should_finish: Option<ShouldFinish> = unsafe {
        let p = libc::dlsym(libc::RTLD_DEFAULT, c"__pengudroid_native_should_finish".as_ptr());
        if p.is_null() {
            None
        } else {
            Some(std::mem::transmute(p))
        }
    };
    let reset_finish: Option<ResetFinish> = unsafe {
        let p = libc::dlsym(libc::RTLD_DEFAULT, c"__pengudroid_native_reset_finish".as_ptr());
        if p.is_null() {
            None
        } else {
            Some(std::mem::transmute(p))
        }
    };
    let asset_manager: *mut c_void = unsafe {
        let p = libc::dlsym(libc::RTLD_DEFAULT, c"AAssetManager_fromJava".as_ptr());
        if p.is_null() {
            std::ptr::null_mut()
        } else {
            let f: AssetManagerFromJava = std::mem::transmute(p);
            f(std::ptr::null_mut(), std::ptr::null_mut())
        }
    };
    if let Some(reset) = reset_finish {
        unsafe { reset() };
    }

    // Data directories the app may write to.
    let package = std::env::var("PENGUDROID_PACKAGE").unwrap_or_else(|_| "app".into());
    let data_dir = std::env::temp_dir().join("pengudroid-data").join(&package);
    let _ = std::fs::create_dir_all(&data_dir);
    let data_path = CString::new(data_dir.to_string_lossy().as_bytes()).unwrap_or_default();

    // The callbacks table must outlive the activity, so it is leaked.
    let callbacks: &'static mut ANativeActivityCallbacks = Box::leak(Box::new(ANativeActivityCallbacks {
        on_start: None,
        on_resume: None,
        on_save_instance_state: None,
        on_pause: None,
        on_stop: None,
        on_destroy: None,
        on_window_focus_changed: None,
        on_native_window_created: None,
        on_native_window_resized: None,
        on_native_window_redraw_needed: None,
        on_native_window_destroyed: None,
        on_input_queue_created: None,
        on_input_queue_destroyed: None,
        on_content_rect_changed: None,
        on_configuration_changed: None,
        on_low_memory: None,
    }));

    let mut activity = ANativeActivity {
        callbacks,
        vm: std::ptr::null_mut(),
        env: std::ptr::null_mut(),
        clazz: std::ptr::null_mut(),
        internal_data_path: data_path.as_ptr(),
        external_data_path: data_path.as_ptr(),
        sdk_version: pengudroid_proto::abi::ANDROID_API_LEVEL,
        instance: std::ptr::null_mut(),
        asset_manager,
        obb_path: std::ptr::null(),
    };
    let activity_ptr: *mut ANativeActivity = &mut activity;

    // A non-null window: `pengudroid-gleshim` ignores the pointer but EGL and
    // the app require it to be non-null.
    let window = unsafe {
        let p = libc::dlsym(libc::RTLD_DEFAULT, c"ANativeWindow_fromSurface".as_ptr());
        if p.is_null() {
            std::ptr::null_mut()
        } else {
            let f: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
                std::mem::transmute(p);
            f(std::ptr::null_mut(), std::ptr::null_mut())
        }
    };

    println!("native: calling {ENTRY_SYMBOL}");
    // SAFETY: the app's entry point with the documented arguments.
    unsafe { on_create(activity_ptr, std::ptr::null_mut(), 0) };

    let cb = unsafe { &*activity.callbacks };
    if let Some(f) = cb.on_start {
        f(activity_ptr);
    }
    if let Some(f) = cb.on_resume {
        f(activity_ptr);
    }
    if let Some(f) = cb.on_window_focus_changed {
        f(activity_ptr, 1);
    }
    if let Some(f) = cb.on_native_window_created {
        f(activity_ptr, window);
    }

    let deadline = args.seconds.map(|s| Instant::now() + Duration::from_secs(s));
    let mut last_redraw = Instant::now();
    while !STOP.load(Ordering::Relaxed) {
        if let Some(should) = should_finish {
            if unsafe { should() } != 0 {
                println!("native: the app asked to finish");
                break;
            }
        }
        if let Some(deadline) = deadline {
            if Instant::now() >= deadline {
                println!("native: --seconds elapsed");
                break;
            }
        }
        // Some apps rely on the redraw callback instead of their own loop.
        if last_redraw.elapsed() >= Duration::from_millis(16) {
            if let Some(f) = cb.on_native_window_redraw_needed {
                f(activity_ptr, window);
            }
            last_redraw = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(4));
    }

    if let Some(f) = cb.on_pause {
        f(activity_ptr);
    }
    if let Some(f) = cb.on_native_window_destroyed {
        f(activity_ptr, window);
    }
    if let Some(f) = cb.on_stop {
        f(activity_ptr);
    }
    if let Some(f) = cb.on_destroy {
        f(activity_ptr);
    }

    // Skip the app's static destructors: they assume Bionic runtime state (the
    // same reason `nativelib` exits with `_exit`).
    let _ = std::io::Write::flush(&mut std::io::stdout());
    unsafe { libc::_exit(0) }
}

fn dlerror() -> String {
    // SAFETY: `dlerror` returns a NUL-terminated string or NULL.
    unsafe {
        let p = libc::dlerror();
        if p.is_null() {
            "unknown error".into()
        } else {
            std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_apk_and_lib_forms() {
        let a = parse_args(&["app.apk".into(), "--seconds".into(), "3".into()]).unwrap();
        assert_eq!(a.apk.as_deref(), Some("app.apk"));
        assert_eq!(a.seconds, Some(3));
        assert_eq!((a.width, a.height), (DEFAULT_W, DEFAULT_H));

        let b = parse_args(&["--lib".into(), "libmain.so".into(), "--width".into(), "800".into()])
            .unwrap();
        assert_eq!(b.lib.as_deref(), Some("libmain.so"));
        assert_eq!(b.width, 800);

        assert!(parse_args(&[]).is_err());
        assert!(parse_args(&["--nope".into()]).is_err());
    }
}
