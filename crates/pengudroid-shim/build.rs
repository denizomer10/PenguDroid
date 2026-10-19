//! Build script for the Bionic shim.
//!
//! Two jobs:
//!
//! 1. Compile a small generated C file (see `src/vlog.rs`) into
//!    `libpengudroid_vlog.a`, which the shim links in. Rust cannot define
//!    C-variadic functions on stable, so the variadic Bionic log entry points
//!    (`__android_log_print`, `__android_log_assert`) are written in C and
//!    forward to Rust, where the format string is expanded with `vsnprintf`.
//! 2. Link the shim against a tiny version script that gives it the
//!    Android-style version nodes `LIBC`, `LIBM` and `LD`.
//!
//! Why the version nodes: a real Android `.so` carries a `VERNEED` demanding that
//! its `libc.so` provide version `LIBC` (and `libm.so` provide `LIBM`, `libdl.so`
//! provide `LD`). By exporting those nodes from the shim, `pengudroid libpath`
//! can point the Bionic sonames at the shim and satisfy that requirement without
//! shipping a separate stub library. The shim's other symbols stay global and
//! unversioned.
//!
//! If no C compiler is available the build still succeeds, but the two variadic
//! entry points are omitted (the shim reports this with a cargo warning).

use std::path::PathBuf;
use std::process::Command;

#[path = "src/vlog.rs"]
mod vlog;

fn main() {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    compile_variadic_shims(&out_dir);
    write_version_script(&out_dir);

    println!("cargo:rustc-check-cfg=cfg(has_variadic_log)");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/vlog.rs");
}

/// Compile the generated C and bundle it into `libpengudroid_vlog.a`.
fn compile_variadic_shims(out_dir: &PathBuf) {
    let dir = out_dir.join("vlog");
    std::fs::create_dir_all(&dir).expect("create vlog build dir");
    let c_path = dir.join("vlog.c");
    let o_path = dir.join("vlog.o");
    let a_path = dir.join("libpengudroid_vlog.a");

    std::fs::write(&c_path, vlog::source()).expect("write generated C");

    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let compiled = Command::new(&cc)
        .args(["-c", "-O2", "-fPIC", "-fvisibility=default"])
        .arg(&c_path)
        .arg("-o")
        .arg(&o_path)
        .status();
    if !matches!(compiled, Ok(s) if s.success()) {
        println!(
            "cargo:warning=pengudroid-shim: no working C compiler; \
             variadic __android_log_print/__android_log_assert are omitted"
        );
        return;
    }

    let ar = std::env::var("AR").unwrap_or_else(|_| "ar".to_string());
    let archived = Command::new(&ar).arg("crs").arg(&a_path).arg(&o_path).status();
    if matches!(archived, Ok(s) if s.success()) {
        println!("cargo:rustc-link-search=native={}", dir.display());
        println!("cargo:rustc-link-lib=static=pengudroid_vlog");
        // The archive members define only the C entry points; nothing in the
        // Rust half references them, so without these the linker drops the
        // objects and the cdylib silently fails to export them. Force each one
        // in; `write_version_script` then re-declares them global.
        for sym in vlog::C_SYMBOLS {
            println!("cargo:rustc-cdylib-link-arg=-Wl,--undefined={sym}");
        }
        println!("cargo:rustc-cfg=has_variadic_log");
    } else {
        println!(
            "cargo:warning=pengudroid-shim: could not archive the C shims; \
             variadic log entry points are omitted"
        );
    }
}

/// Assign the three anchor symbols to `LIBC`/`LIBM`/`LD`; everything else stays
/// global, so the shim can serve as the provider of the Bionic sonames.
///
/// The C-implemented entry points are listed in the `LIBC` node as well: rustc
/// emits its own export list for a cdylib, and a symbol that appears in *no*
/// version node is hidden by it. Naming them here is what makes them visible.
fn write_version_script(out_dir: &PathBuf) {
    let map = out_dir.join("pengudroid-versions.map");
    let mut libc_globals = String::from("__pengudroid_ver_libc");
    for sym in vlog::C_SYMBOLS {
        libc_globals.push_str("; ");
        libc_globals.push_str(sym);
    }
    let script = format!(
        "LIBC {{ global: {libc_globals}; }};\
         LIBM {{ global: __pengudroid_ver_libm; }};\
         LD   {{ global: __pengudroid_ver_ld; }};\n"
    );
    std::fs::write(&map, script).expect("write version script");

    // Applies to the cdylib target only, so the rlib/tests are unaffected.
    //
    // `--undefined-version` keeps a symbol written by an *earlier* version
    // script (rustc's export list) from being an error; without it GNU ld
    // complains the symbol already belongs to another version node.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // Rust's cdylib export list hides symbols it does not know about, which
        // includes the C-implemented variadic log entry points. Re-declare them
        // global here and force the export.
        println!(
            "cargo:rustc-cdylib-link-arg=-Wl,--version-script={},--undefined-version",
            map.display()
        );
        // A SONAME makes every compat-directory symlink (`libc.so`, `libdl.so`,
        // `ld-android.so`, ...) resolve to the *already-loaded* shim by name
        // instead of being mapped a second time under the alias. Without it the
        // loader maps the shim again for each alias, and an alias such as
        // `ld-android.so` then runs the real Android linker's constructor.
        println!("cargo:rustc-cdylib-link-arg=-Wl,-soname,libpengudroid_shim.so");
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let _ = map;
}
