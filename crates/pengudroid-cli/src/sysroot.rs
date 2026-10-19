//! ART bring-up plumbing: `pengudroid sysroot` and `pengudroid art-test`.
//!
//! PenguDroid does not ship an Android system image, but ART and the framework
//! are binaries that must come from somewhere. These commands make that
//! requirement explicit and testable:
//!
//! * `pengudroid sysroot <dir>` inspects an *extracted* Android system tree and
//!   reports which pieces ART needs are present, plus the environment variables
//!   to use.
//! * `pengudroid art-test --sysroot <dir>` re-execs itself as `__art` under the
//!   Bionic shim, loads `libart.so`, and calls `JNI_CreateJavaVM`. It prints
//!   ART's own diagnostic when the runtime cannot start, which is the honest
//!   state of the art today (see `docs/LIMITATIONS.md` §2).
//!
//! Nothing here pretends ART works without its files: a missing `libart.so` or
//! boot image is reported as such.

use std::ffi::{c_char, c_int, c_void, CString};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::deps::find_shim;
use crate::nativelib::ensure_compat_dir;

/// Files ART needs, relative to the sysroot, with a human description.
const REQUIRED: &[(&str, &str)] = &[
    ("lib64/libart.so", "the ART runtime"),
    ("lib64/libartbase.so", "ART's base library"),
    ("lib64/libdexfile.so", "DEX file support"),
    ("lib64/libnativehelper.so", "JNI helpers"),
    ("lib64/libc.so", "Bionic libc"),
    ("framework/framework.jar", "the Android framework classes"),
    ("framework/boot.art", "the boot image"),
    ("apex/com.android.art/apex_manifest.pb", "the ART APEX manifest"),
];

/// Optional but commonly needed pieces.
const OPTIONAL: &[(&str, &str)] = &[
    ("lib64/libEGL.so", "EGL"),
    ("lib64/libGLESv2.so", "GLES"),
    ("lib64/libvulkan.so", "Vulkan"),
    ("lib64/libhwui.so", "hardware UI"),
    ("lib64/libgui.so", "surface/gui"),
    ("lib64/libbinder.so", "binder"),
    ("framework/boot.oat", "boot image (oat)"),
    ("framework/boot.vdex", "boot image (vdex)"),
    ("etc/permissions", "platform permissions"),
];

/// `pengudroid sysroot <dir>`
pub fn sysroot(args: &[String]) -> i32 {
    let Some(dir) = args.iter().find(|a| !a.starts_with('-')) else {
        eprintln!("usage: pengudroid sysroot <extracted-android-system-dir>");
        return 2;
    };
    let root = Path::new(dir);
    if !root.is_dir() {
        eprintln!("pengudroid: {dir} is not a directory");
        return 1;
    }

    println!("sysroot: {}", root.display());
    let mut missing = 0;
    println!("\nrequired:");
    for (rel, what) in REQUIRED {
        let p = root.join(rel);
        if p.exists() {
            println!("  [ ok ] {rel:<44} {what}");
        } else {
            println!("  [MISS] {rel:<44} {what}");
            missing += 1;
        }
    }
    println!("\noptional:");
    for (rel, what) in OPTIONAL {
        let p = root.join(rel);
        println!(
            "  [{}] {rel:<44} {what}",
            if p.exists() { " ok " } else { " -- " }
        );
    }

    println!("\nenvironment for a guest:");
    println!("  ANDROID_ROOT={}", root.display());
    println!("  ANDROID_ART_ROOT={}", root.join("apex/com.android.art").display());
    println!("  ANDROID_DATA={}", root.join("data").display());
    println!("  BOOTCLASSPATH={}", boot_classpath(root));

    if missing == 0 {
        println!("\nverdict: complete — try `pengudroid art-test --sysroot {}`", root.display());
        0
    } else {
        println!(
            "\nverdict: incomplete ({missing} required file(s) missing). \
             Extract an Android system image (system.img / an APEX set) into this directory."
        );
        1
    }
}

/// The boot classpath ART expects, from the jars actually present.
fn boot_classpath(root: &Path) -> String {
    let candidates = [
        "framework/core-oj.jar",
        "framework/core-libart.jar",
        "framework/core-icu4j.jar",
        "framework/conscrypt.jar",
        "framework/okhttp.jar",
        "framework/bouncycastle.jar",
        "framework/apache-xml.jar",
        "framework/framework.jar",
    ];
    let mut jars: Vec<String> = candidates
        .iter()
        .map(|c| root.join(c))
        .filter(|p| p.exists())
        .map(|p| p.display().to_string())
        .collect();
    if jars.is_empty() {
        jars.push(root.join("framework/framework.jar").display().to_string());
    }
    jars.join(":")
}

/// `pengudroid art-test --sysroot <dir>`
pub fn art_test(args: &[String]) -> i32 {
    let mut root: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--sysroot" => root = it.next().map(PathBuf::from),
            other if other.starts_with('-') => {
                eprintln!("pengudroid: unknown option {other}");
                return 2;
            }
            other => root = Some(PathBuf::from(other)),
        }
    }
    let Some(root) = root else {
        eprintln!("usage: pengudroid art-test --sysroot <dir>");
        return 2;
    };
    let art = root.join("lib64/libart.so");
    if !art.exists() {
        eprintln!("pengudroid: {} is missing", art.display());
        eprintln!("           run `pengudroid sysroot {}` to see what else is needed", root.display());
        return 1;
    }
    let Some(shim) = find_shim() else {
        eprintln!("pengudroid: libpengudroid_shim.so not found (set PENGUDROID_SHIM)");
        return 1;
    };
    let compat = match ensure_compat_dir() {
        Ok(d) => d,
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
    cmd.arg("__art").arg("--sysroot").arg(&root);
    cmd.env("LD_PRELOAD", &shim)
        .env("LD_LIBRARY_PATH", format!("{}:{}", compat.display(), root.join("lib64").display()))
        .env("ANDROID_ROOT", &root)
        .env("ANDROID_ART_ROOT", root.join("apex/com.android.art"))
        .env("ANDROID_DATA", root.join("data"))
        .env("BOOTCLASSPATH", boot_classpath(&root))
        .env("PENGUDROID_SUPPRESS_DESTRUCTORS", "1");
    match cmd.status() {
        Ok(s) if s.success() => 0,
        Ok(_) => 1,
        Err(e) => {
            eprintln!("pengudroid: cannot start the ART helper: {e}");
            1
        }
    }
}

// ---------------------------------------------------------------------------
// The hidden `__art` helper
// ---------------------------------------------------------------------------

#[repr(C)]
struct JavaVMOption {
    option_string: *mut c_char,
    extra_info: *mut c_void,
}

#[repr(C)]
struct JavaVMInitArgs {
    version: c_int,
    n_options: c_int,
    options: *mut JavaVMOption,
    ignore_unrecognized: u8,
}

type CreateJavaVM =
    unsafe extern "C" fn(*mut *mut c_void, *mut *mut c_void, *mut c_void) -> c_int;

const JNI_VERSION_1_6: c_int = 0x0001_0006;
const JNI_OK: c_int = 0;
const JNI_ERR: c_int = -1;

/// `pengudroid __art --sysroot <dir>` — load libart and try to start a VM.
pub fn art_main(args: &[String]) -> i32 {
    let mut root: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--sysroot" {
            root = it.next().map(PathBuf::from);
        }
    }
    let Some(root) = root else {
        eprintln!("art: --sysroot is required");
        return 2;
    };
    let lib = root.join("lib64/libart.so");
    let cpath = match CString::new(lib.to_string_lossy().as_bytes()) {
        Ok(c) => c,
        Err(_) => return 2,
    };
    // SAFETY: `cpath` is a valid C string.
    let handle = unsafe { libc::dlopen(cpath.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
    if handle.is_null() {
        eprintln!("art: dlopen {} failed: {}", lib.display(), dlerror());
        return 1;
    }
    println!("art: loaded {}", lib.display());

    // SAFETY: the symbol has the documented JNI signature.
    let create: CreateJavaVM = unsafe {
        let p = libc::dlsym(handle, c"JNI_CreateJavaVM".as_ptr());
        if p.is_null() {
            eprintln!("art: libart.so does not export JNI_CreateJavaVM");
            return 1;
        }
        std::mem::transmute(p)
    };

    let boot = boot_classpath(&root);
    let mut options: Vec<CString> = vec![
        CString::new(format!("-Xbootclasspath:{boot}")).unwrap_or_default(),
        CString::new(format!("-Djava.library.path={}", root.join("lib64").display()))
            .unwrap_or_default(),
        CString::new("-Xmx512m").unwrap_or_default(),
    ];
    let mut vm_options: Vec<JavaVMOption> = options
        .iter_mut()
        .map(|o| JavaVMOption { option_string: o.as_ptr() as *mut c_char, extra_info: std::ptr::null_mut() })
        .collect();
    let mut init = JavaVMInitArgs {
        version: JNI_VERSION_1_6,
        n_options: vm_options.len() as c_int,
        options: vm_options.as_mut_ptr(),
        ignore_unrecognized: 1,
    };

    let mut vm: *mut c_void = std::ptr::null_mut();
    let mut env: *mut c_void = std::ptr::null_mut();
    println!("art: calling JNI_CreateJavaVM ({} options)", vm_options.len());
    // SAFETY: the argument block matches the JNI ABI.
    let rc = unsafe { create(&mut vm, &mut env, &mut init as *mut JavaVMInitArgs as *mut c_void) };
    match rc {
        JNI_OK => {
            println!("art: JNI_CreateJavaVM returned JNI_OK (vm={vm:p}, env={env:p})");
            println!("art: a live JavaVM is available");
            0
        }
        JNI_ERR => {
            eprintln!("art: JNI_CreateJavaVM returned JNI_ERR");
            eprintln!(
                "art: ART ran its startup path but could not create the VM. \
                 The usual causes are a missing boot image, missing APEX manifests, \
                 or Bionic-specific thread/GC state; see docs/LIMITATIONS.md §2."
            );
            1
        }
        other => {
            eprintln!("art: JNI_CreateJavaVM returned {other}");
            1
        }
    }
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
    fn boot_classpath_falls_back_to_framework_jar() {
        let dir = std::env::temp_dir().join(format!("pd-sysroot-{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("framework"));
        let cp = boot_classpath(&dir);
        assert!(cp.ends_with("framework.jar"), "{cp}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sysroot_reports_missing_files() {
        let dir = std::env::temp_dir().join(format!("pd-sysroot-empty-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let code = sysroot(&[dir.display().to_string()]);
        assert_eq!(code, 1, "an empty sysroot must be reported incomplete");
        let _ = std::fs::remove_dir_all(dir);
    }
}
