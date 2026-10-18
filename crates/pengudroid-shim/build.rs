//! Build script for the Bionic shim.
//!
//! Besides compiling the shim, this links it against a tiny version script that
//! gives it the Android-style version nodes `LIBC`, `LIBM` and `LD`.
//!
//! Why: a real Android `.so` carries a `VERNEED` demanding that its `libc.so`
//! provide version `LIBC` (and `libm.so` provide `LIBM`, `libdl.so` provide
//! `LD`). By exporting those nodes from the shim, `pengudroid libpath` can point
//! the Bionic sonames at the shim and satisfy that requirement without shipping
//! a separate stub library or a C compiler. The shim's other symbols stay
//! global and unversioned.

use std::path::PathBuf;

fn main() {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let map = out_dir.join("pengudroid-versions.map");
    let script = "\
LIBC { global: __pengudroid_ver_libc; };
LIBM { global: __pengudroid_ver_libm; };
LD   { global: __pengudroid_ver_ld; };
";
    std::fs::write(&map, script).expect("write version script");

    // Applies to the cdylib target only, so the rlib/tests are unaffected.
    println!("cargo:rustc-cdylib-link-arg=-Wl,--version-script={}", map.display());
    println!("cargo:rerun-if-changed=build.rs");
}
