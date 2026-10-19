//! Loading real Android native libraries on glibc.
//!
//! Three commands make this work end to end:
//!
//! * `pengudroid de-bionic <in.so> <out.so>` — strip Android's coarse symbol
//!   versioning (see [`pengudroid_elf::strip_android_versions`]).
//! * `pengudroid libpath` — materialise a directory that maps Bionic sonames
//!   (`libc.so`, `libdl.so`, `liblog.so`, ...) onto host libraries and the
//!   shim, so `DT_NEEDED` resolution succeeds. Print the directory for
//!   `LD_LIBRARY_PATH`.
//! * `pengudroid nativelib <lib.so>` — do the above and then actually attempt
//!   `dlopen` in a child process (via the hidden `__dlopen` command) configured
//!   with the shim and the compat path, reporting whether it loads.
//!
//! The reason all three are needed, discovered empirically on real APK
//! libraries:
//! 1. Bionic objects carry a `VERNEED` for version `LIBC`, which glibc cannot
//!    satisfy (`version ``LIBC' not found`).
//! 2. Their `DT_NEEDED` names (`libc.so`, `libdl.so`) do not exist on a modern
//!    glibc system (`libc.so` is a linker script; `libdl.so` is gone).
//! 3. They import a few symbols glibc lacks (`__strlen_chk`, `__sF`, ...),
//!    which the shim supplies.

use std::path::{Path, PathBuf};

use pengudroid_elf::{machine_name, read_file, strip_android_versions, Class};

use crate::apk;
use crate::deps::find_shim;

/// Directory that holds the Bionic-soname symlinks, under the user's cache.
fn compat_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("pengudroid").join("bionic-compat")
}

/// Host multiarch directory for the current architecture.
fn multiarch_dir() -> String {
    match std::env::consts::ARCH {
        "x86_64" => "x86_64-linux-gnu".to_string(),
        "aarch64" => "aarch64-linux-gnu".to_string(),
        "x86" => "i386-linux-gnu".to_string(),
        "arm" => "arm-linux-gnueabihf".to_string(),
        other => format!("{other}-linux-gnu"),
    }
}

fn find_host_lib(sonames: &[&str]) -> Option<PathBuf> {
    let arch = multiarch_dir();
    let dirs = [
        format!("/lib/{arch}"),
        format!("/usr/lib/{arch}"),
        "/lib64".to_string(),
        "/usr/lib64".to_string(),
        "/lib".to_string(),
        "/usr/lib".to_string(),
    ];
    for d in &dirs {
        for s in sonames {
            let p = Path::new(d).join(s);
            if p.exists() {
                return Some(p);
            }
        }
    }
    None
}

fn symlink(link: &Path, target: &Path) -> std::io::Result<()> {
    let _ = std::fs::remove_file(link);
    std::os::unix::fs::symlink(target, link)
}

/// Locate `libpengudroid_gleshim.so` (the Bionic libEGL/GLESv2 shim) next to
/// this executable.
fn find_gleshim() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("PENGUDROID_GLESHIM") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let candidate = dir.join("libpengudroid_gleshim.so");
    if candidate.exists() {
        Some(candidate)
    } else {
        None
    }
}

/// Create (or refresh) the compat directory. Returns its path.
///
/// Every Bionic soname an Android object might `DT_NEEDED` resolves to the
/// **shim**. That single choice kills three birds: the shim provides the Bionic
/// symbols themselves, it carries the Android version nodes `LIBC`/`LIBM`/`LD`
/// (so the object's `VERNEED` is satisfied), and — because those references
/// have been made unversioned by
/// [`pengudroid_elf::strip_android_versions`] — ordinary libc symbols such as
/// `free` resolve from the already-loaded glibc. `libz`/`libstdc++` are mapped
/// to the host libraries instead, since those are not Bionic-specific.
pub fn ensure_compat_dir() -> Result<PathBuf, String> {
    let dir = compat_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;

    let shim = find_shim().ok_or_else(|| {
        "libpengudroid_shim.so not found next to this binary (set PENGUDROID_SHIM)".to_string()
    })?;

    const BIONIC_SONAMES: &[&str] = &[
        "libc.so",
        "libm.so",
        "libdl.so",
        "liblog.so",
        "libandroid.so",
        "libnativewindow.so",
        "libnativehelper.so",
        "libOpenSLES.so",
        // Android's linker API (`android_dlopen_ext`, `android_create_namespace`,
        // the `__loader_*` entry points) is translated to glibc by the shim, so
        // the linker and its `libdl_android` wrapper resolve here too.
        "libdl_android.so",
        "ld-android.so",
    ];
    for name in BIONIC_SONAMES {
        symlink(&dir.join(name), &shim).map_err(|e| format!("symlink {name}: {e}"))?;
    }

    // EGL must go through our shim: it owns the virtual surface, the FBO, and
    // the dma-buf swap. GL itself, however, can bind straight to the host
    // driver — our shim makes the host context current and binds its FBO, so
    // direct `gl*` calls land in the right place. That also gives the app the
    // *full* GLES entry-point surface instead of the handful we export.
    if let Some(gleshim) = find_gleshim() {
        let _ = symlink(&dir.join("libEGL.so"), &gleshim);
    }
    if let Some(gles) = find_host_lib(&["libGLESv2.so.2"]) {
        let _ = symlink(&dir.join("libGLESv2.so"), &gles);
    }
    if let Some(gles1) = find_host_lib(&["libGLESv1_CM.so.1"]) {
        let _ = symlink(&dir.join("libGLESv1_CM.so"), &gles1);
    }

    // Vulkan: Android's `libvulkan.so` is a loader; the host's loader is the
    // right implementation (it finds the NVIDIA/AMD/Intel ICDs), so map the
    // Bionic soname straight onto it. No translation layer is needed because
    // Vulkan's ABI is identical on Android and Linux.
    if let Some(vk) = find_host_lib(&["libvulkan.so.1"]) {
        let _ = symlink(&dir.join("libvulkan.so"), &vk);
        let _ = symlink(&dir.join("libvulkan.so.1"), &vk);
    }

    if let Some(z) = find_host_lib(&["libz.so.1"]) {
        let _ = symlink(&dir.join("libz.so"), &z);
    }
    if let Some(cxx) = find_host_lib(&["libstdc++.so.6"]) {
        let _ = symlink(&dir.join("libstdc++.so"), &cxx);
    }
    Ok(dir)
}

/// `pengudroid libpath`
pub fn libpath(_args: &[String]) -> i32 {
    match ensure_compat_dir() {
        Ok(dir) => {
            println!("{}", dir.display());
            eprintln!("use: LD_PRELOAD=<shim> LD_LIBRARY_PATH={} ...", dir.display());
            0
        }
        Err(e) => {
            eprintln!("pengudroid: {e}");
            1
        }
    }
}

/// `pengudroid de-bionic <in.so> <out.so>`
pub fn de_bionic(args: &[String]) -> i32 {
    let files: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    if files.len() != 2 {
        eprintln!("usage: pengudroid de-bionic <in.so> <out.so>");
        return 2;
    }
    let data = match std::fs::read(files[0]) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("pengudroid: cannot read {}: {e}", files[0]);
            return 1;
        }
    };
    match strip_android_versions(&data) {
        Ok(out) => match std::fs::write(files[1], &out) {
            Ok(()) => {
                println!("de-bionic: {} -> {} ({} bytes)", files[0], files[1], out.len());
                0
            }
            Err(e) => {
                eprintln!("pengudroid: cannot write {}: {e}", files[1]);
                1
            }
        },
        Err(e) => {
            eprintln!("pengudroid: {e}");
            1
        }
    }
}

/// Best-effort host-architecture check so we can explain cross-arch failures.
pub(crate) fn host_can_load(info: &pengudroid_elf::ElfInfo) -> Result<(), String> {
    let host64 = std::mem::size_of::<usize>() == 8;
    let class_ok = matches!(
        (info.class, host64),
        (Class::Elf64, true) | (Class::Elf32, false)
    );
    let arch_ok = match (std::env::consts::ARCH, info.machine) {
        ("x86_64", 0x3E) => true,
        ("x86", 0x03) => true,
        ("aarch64", 0xB7) => true,
        ("arm", 0x28) => true,
        _ => false,
    };
    if class_ok && arch_ok {
        Ok(())
    } else {
        Err(format!(
            "cannot dlopen a {} {}-bit library on this host ({}-bit {})",
            machine_name(info.machine),
            info.class.bits(),
            if host64 { 64 } else { 32 },
            std::env::consts::ARCH
        ))
    }
}

/// Parsed arguments for `nativelib`.
#[derive(Debug, PartialEq, Eq)]
struct NativeArgs {
    lib: Option<String>,
    apk: Option<String>,
    abi: Option<String>,
    call: Option<String>,
    lib_dir: Option<PathBuf>,
    /// Exercise `dlclose` (teardown) instead of exiting via `_exit`.
    dlclose: bool,
    /// Ask the shim to suppress Bionic C++ destructors (default on).
    suppress_destructors: bool,
}

fn parse_args(args: &[String]) -> Result<NativeArgs, String> {
    let mut lib = None;
    let mut apk = None;
    let mut abi = None;
    let mut call = None;
    let mut lib_dir = None;
    let mut dlclose = false;
    let mut suppress_destructors = true;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--call" => call = Some(it.next().ok_or("--call needs a SYMBOL")?.clone()),
            "--lib-dir" => lib_dir = Some(PathBuf::from(it.next().ok_or("--lib-dir needs a DIR")?)),
            "--apk" => apk = Some(it.next().ok_or("--apk needs a PATH")?.clone()),
            "--abi" => abi = Some(it.next().ok_or("--abi needs a NAME")?.clone()),
            "--dlclose" => dlclose = true,
            "--no-suppress-destructors" => suppress_destructors = false,
            other if other.starts_with('-') => {
                return Err(format!("unknown option: {other}"));
            }
            other => lib = Some(other.to_string()),
        }
    }
    if lib.is_none() && apk.is_none() {
        return Err(
            "usage: pengudroid nativelib <lib.so> [--call SYM] [--lib-dir DIR]\n       \
             pengudroid nativelib --apk <app.apk> [--abi <abi>]"
                .into(),
        );
    }
    Ok(NativeArgs { lib, apk, abi, call, lib_dir, dlclose, suppress_destructors })
}

/// The Android ABI directory that matches this host's architecture.
pub(crate) fn host_abi() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "x86" => "x86",
        "aarch64" => "arm64-v8a",
        "arm" => "armeabi-v7a",
        other => other,
    }
}

/// Pick the most informative line from a helper run: the OK/FAIL verdict.
fn verdict(stdout: &str, stderr: &str) -> String {
    for line in stdout.lines().chain(stderr.lines()) {
        if line.starts_with("OK:") || line.starts_with("FAIL:") {
            return line.to_string();
        }
    }
    stderr.lines().last().unwrap_or("(no output)").to_string()
}

/// `pengudroid nativelib <lib.so> [--call SYM] [--lib-dir DIR]`
/// `pengudroid nativelib --apk <app.apk> [--abi <abi>]`
pub fn nativelib(args: &[String]) -> i32 {
    let na = match parse_args(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("pengudroid: {e}");
            return 2;
        }
    };
    if na.apk.is_some() {
        return run_apk(&na);
    }
    let path = na.lib.as_deref().expect("parse_args guarantees a lib");

    let info = match read_file(path) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("pengudroid: {e}");
            return 1;
        }
    };
    println!("library: {} ({}, {})", path, match info.class { Class::Elf32 => "ELF32", Class::Elf64 => "ELF64" }, machine_name(info.machine));
    if let Err(e) = host_can_load(&info) {
        eprintln!("pengudroid: {e}");
        return 1;
    }

    let shim = match find_shim() {
        Some(s) => s,
        None => {
            eprintln!("pengudroid: libpengudroid_shim.so not found next to this binary (set PENGUDROID_SHIM)");
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

    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("pengudroid: cannot read {path}: {e}");
            return 1;
        }
    };
    let patched_bytes = match strip_android_versions(&data) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("pengudroid: {e}");
            return 1;
        }
    };
    let work = std::env::temp_dir().join("pengudroid-nativelib");
    if let Err(e) = std::fs::create_dir_all(&work) {
        eprintln!("pengudroid: cannot create {}: {e}", work.display());
        return 1;
    }
    let patched = work.join(
        Path::new(path).file_name().map(|s| s.to_os_string()).unwrap_or_else(|| "library.so".into()),
    );
    if let Err(e) = std::fs::write(&patched, &patched_bytes) {
        eprintln!("pengudroid: cannot write {}: {e}", patched.display());
        return 1;
    }

    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("pengudroid: cannot find own exe: {e}");
            return 1;
        }
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("__dlopen").arg(&patched);
    if let Some(sym) = &na.call {
        cmd.arg(sym);
    }
    let mut ld_path = compat.display().to_string();
    if let Some(dir) = &na.lib_dir {
        if !ld_path.is_empty() {
            ld_path.push(':');
        }
        ld_path.push_str(&dir.display().to_string());
    }
    cmd.env("LD_PRELOAD", &shim)
        .env("LD_LIBRARY_PATH", ld_path)
        .env("PENGUDROID_SUPPRESS_DESTRUCTORS", if na.suppress_destructors { "1" } else { "0" });
    if na.dlclose {
        cmd.env("PENGUDROID_DLCLOSE", "1");
    }
    let status = cmd.status();
    match status {
        Ok(s) if s.success() => 0,
        Ok(_) => 1,
        Err(e) => {
            eprintln!("pengudroid: failed to spawn helper: {e}");
            1
        }
    }
}

/// `pengudroid __dlopen <lib.so> [SYMBOL]` — hidden helper run under `LD_PRELOAD`.
///
/// Loads the library, and if `SYMBOL` is given, resolves it and calls it as a
/// nullary `int`-returning function, printing the result. This is how PenguDroid
/// demonstrates *running*, not just loading, real native code.
pub fn dlopen_helper(args: &[String]) -> i32 {
    let Some(path) = args.first() else {
        eprintln!("usage: pengudroid __dlopen <lib.so> [SYMBOL]");
        return 2;
    };
    let symbol = args.get(1);
    let Ok(cpath) = std::ffi::CString::new(path.as_str()) else {
        eprintln!("path contains a NUL byte");
        return 2;
    };
    // SAFETY: valid C strings; dlopen/dlsym/dlerror are standard libc calls.
    unsafe {
        // Bionic folds several sonames into single libraries; we map `libm.so`,
        // `libc.so`, etc. to the shim to satisfy their version nodes. Make the
        // host libraries that really contain those symbols available in the
        // global scope so unversioned references (e.g. `fma`) resolve.
        for soname in ["libm.so.6", "libstdc++.so.6", "libz.so.1", "libgcc_s.so.1"] {
            if let Ok(c) = std::ffi::CString::new(soname) {
                libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL);
            }
        }

        let handle = libc::dlopen(cpath.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if handle.is_null() {
            let err = libc::dlerror();
            let msg = if err.is_null() {
                "unknown error".to_string()
            } else {
                std::ffi::CStr::from_ptr(err).to_string_lossy().into_owned()
            };
            eprintln!("FAIL: {msg}");
            let _ = std::io::Write::flush(&mut std::io::stdout());
            libc::_exit(1);
        }
        println!("OK: dlopen succeeded ({path})");

        if let Some(sym) = symbol {
            let Ok(csym) = std::ffi::CString::new(sym.as_str()) else {
                eprintln!("symbol contains a NUL byte");
                libc::_exit(2);
            };
            let addr = libc::dlsym(handle, csym.as_ptr());
            if addr.is_null() {
                let err = libc::dlerror();
                let msg = if err.is_null() {
                    "symbol not found".to_string()
                } else {
                    std::ffi::CStr::from_ptr(err).to_string_lossy().into_owned()
                };
                eprintln!("FAIL: {msg}");
                let _ = std::io::Write::flush(&mut std::io::stdout());
                libc::_exit(1);
            }
            // Convention: `--call` invokes a nullary function returning i32.
            let func: extern "C" fn() -> i32 = std::mem::transmute(addr);
            let result = func();
            println!("CALL {sym}() = {result}");
        }

        let _ = std::io::Write::flush(&mut std::io::stdout());
        if std::env::var("PENGUDROID_DLCLOSE").as_deref() == Ok("1") {
            // Teardown path. With PENGUDROID_SUPPRESS_DESTRUCTORS=1 the shim
            // keeps Bionic C++ static destructors from running (they assume
            // Bionic runtime state and can segfault).
            libc::dlclose(handle);
            println!("dlclose: returned without crashing");
            let _ = std::io::Write::flush(&mut std::io::stdout());
            // Also run process-exit handlers, so a destructor registered with
            // `dso_handle = 0` (process-global) is actually invoked — this is
            // precisely what suppression must prevent.
            libc::exit(0);
        }
        libc::_exit(0);
    }
}

/// Load every native library an APK ships for one ABI.
///
/// The libraries are extracted to a scratch directory and stripped, so that
/// dependencies *between* them (for example a library that `DT_NEEDED`s the
/// APK's own `libc++_shared.so`) resolve within that directory — this is what
/// makes an application's own native code loadable as a set, not just one file.
fn run_apk(na: &NativeArgs) -> i32 {
    let apk_path = na.apk.as_deref().expect("run_apk requires --apk");
    let libs = match apk::native_libs(apk_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("pengudroid: {e}");
            return 1;
        }
    };
    if libs.is_empty() {
        println!("{apk_path}: no native libraries (no lib/<abi>/*.so entries)");
        return 0;
    }

    let abi = na.abi.clone().unwrap_or_else(|| host_abi().to_string());
    let selected: Vec<&apk::NativeLib> = libs.iter().filter(|l| l.abi == abi).collect();
    if selected.is_empty() {
        let mut abis: Vec<&str> = libs.iter().map(|l| l.abi.as_str()).collect();
        abis.sort_unstable();
        abis.dedup();
        println!("{apk_path}: no libraries for abi '{abi}'");
        println!("available abis: {}", abis.join(", "));
        return 0;
    }

    let shim = match find_shim() {
        Some(s) => s,
        None => {
            eprintln!("pengudroid: libpengudroid_shim.so not found next to this binary (set PENGUDROID_SHIM)");
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

    let libdir = std::env::temp_dir().join("pengudroid-nativelib").join(format!("apk-{abi}"));
    if let Err(e) = std::fs::create_dir_all(&libdir) {
        eprintln!("pengudroid: cannot create {}: {e}", libdir.display());
        return 1;
    }

    // Extract and strip each library into `libdir` so intra-APK dependencies
    // resolve there.
    let mut ready = Vec::new();
    for l in &selected {
        let bytes = match apk::extract_named(apk_path, &l.entry) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("pengudroid: {e}");
                return 1;
            }
        };
        let stripped = match strip_android_versions(&bytes) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("pengudroid: {}: {e}", l.file);
                return 1;
            }
        };
        let out = libdir.join(&l.file);
        if let Err(e) = std::fs::write(&out, &stripped) {
            eprintln!("pengudroid: cannot write {}: {e}", out.display());
            return 1;
        }
        let info = pengudroid_elf::parse(&bytes).ok();
        ready.push((l, out, info));
    }

    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("pengudroid: cannot find own exe: {e}");
            return 1;
        }
    };
    let ld_path = format!("{}:{}", compat.display(), libdir.display());

    println!("{apk_path}: {} native libraries for {abi}", ready.len());
    let mut failures = 0;
    let mut skipped = 0;
    for (l, target, info) in &ready {
        if let Some(info) = info {
            if let Err(msg) = host_can_load(info) {
                println!("  [SKIP] {}  {msg}", l.file);
                skipped += 1;
                continue;
            }
        }
        let output = std::process::Command::new(&exe)
            .arg("__dlopen")
            .arg(target)
            .env("LD_PRELOAD", &shim)
            .env("LD_LIBRARY_PATH", &ld_path)
            .env("PENGUDROID_SUPPRESS_DESTRUCTORS", "1")
            .output();
        match output {
            Ok(o) => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                let stderr = String::from_utf8_lossy(&o.stderr);
                if o.status.success() {
                    println!("  [ OK ] {}", l.file);
                } else {
                    failures += 1;
                    println!("  [FAIL] {}  {}", l.file, verdict(&stdout, &stderr));
                }
            }
            Err(e) => {
                failures += 1;
                println!("  [ERR ] {}  spawn failed: {e}", l.file);
            }
        }
    }
    println!(
        "{apk_path}: {} loadable, {} skipped, {} failed",
        ready.len() - skipped - failures,
        skipped,
        failures
    );
    if failures == 0 {
        0
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compat_dir_is_under_cache() {
        let d = compat_dir();
        assert!(d.ends_with("pengudroid/bionic-compat"));
    }

    #[test]
    fn multiarch_dir_is_plausible() {
        assert!(multiarch_dir().contains("linux-gnu"));
    }

    #[test]
    fn parses_plain_lib() {
        let a = parse_args(&["libfoo.so".to_string()]).unwrap();
        assert_eq!(a.lib.as_deref(), Some("libfoo.so"));
        assert_eq!(a.apk, None);
        assert_eq!(a.abi, None);
        assert_eq!(a.call, None);
        assert_eq!(a.lib_dir, None);
        assert!(!a.dlclose);
        assert!(a.suppress_destructors);
    }

    #[test]
    fn parses_dlclose_and_no_suppress() {
        let a = parse_args(&[
            "libfoo.so".to_string(),
            "--dlclose".to_string(),
            "--no-suppress-destructors".to_string(),
        ])
        .unwrap();
        assert!(a.dlclose);
        assert!(!a.suppress_destructors);
    }

    #[test]
    fn parses_call_and_lib_dir() {
        let args = vec![
            "libfoo.so".to_string(),
            "--call".to_string(),
            "do_it".to_string(),
            "--lib-dir".to_string(),
            "/tmp/libs".to_string(),
        ];
        let a = parse_args(&args).unwrap();
        assert_eq!(a.lib.as_deref(), Some("libfoo.so"));
        assert_eq!(a.call.as_deref(), Some("do_it"));
        assert_eq!(a.lib_dir.as_deref(), Some(Path::new("/tmp/libs")));
    }

    #[test]
    fn parses_apk_and_abi() {
        let args =
            vec!["--apk".to_string(), "app.apk".to_string(), "--abi".to_string(), "x86_64".to_string()];
        let a = parse_args(&args).unwrap();
        assert_eq!(a.apk.as_deref(), Some("app.apk"));
        assert_eq!(a.abi.as_deref(), Some("x86_64"));
        assert_eq!(a.lib, None);
    }

    #[test]
    fn host_abi_is_an_android_abi_name() {
        let a = host_abi();
        assert!(!a.is_empty());
        // Sanity: on any supported host it is one of the known Android ABI dirs.
        assert!(matches!(a, "x86_64" | "x86" | "arm64-v8a" | "armeabi-v7a") || !a.is_empty());
    }

    #[test]
    fn rejects_missing_value_and_unknown_flag() {
        assert!(parse_args(&["--call".to_string()]).is_err());
        assert!(parse_args(&["--nope".to_string()]).is_err());
        assert!(parse_args(&[]).is_err());
    }
}
