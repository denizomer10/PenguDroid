//! `pengudroid deps <lib.so>` — coverage analysis for a native library.
//!
//! Loading an Android `.so` on glibc only works if every Bionic symbol it
//! imports is supplied by the `LD_PRELOAD` shim (or by glibc itself). This
//! command reads a target library's **undefined dynamic symbols** and compares
//! them against the shim's **actual exported symbols** — read from the built
//! `libpengudroid_shim.so`, so the report cannot drift from reality.
//!
//! Output classifies imports into:
//! * **bionic** — symbols the shim is expected to provide;
//! * **host** — ordinary libc/libm/etc. symbols served by glibc.
//!
//! and reports the bionic coverage gap.

use std::collections::BTreeSet;
use std::path::PathBuf;

use pengudroid_elf::{machine_name, read_file, Class};

/// Prefixes/exact names that indicate an Android-specific (Bionic/NDK) import.
const BIONIC_PREFIXES: &[&str] = &[
    "__android_",
    "ashmem_",
    "AAsset",
    "AAssetManager",
    "ANativeWindow",
    "ALooper_",
    "AInput",
    "ASensor",
    "AMedia",
    "ACamera",
    "android_",
    "__system_property",
    "android_dlopen",
];
const BIONIC_EXACT: &[&str] = &["property_get", "property_set", "__errno"];

/// Is this symbol one the Bionic shim is expected to provide?
pub fn is_bionic(sym: &str) -> bool {
    BIONIC_EXACT.contains(&sym) || BIONIC_PREFIXES.iter().any(|p| sym.starts_with(p))
}

/// Locate the built shim: explicit override, then next to this executable.
pub(crate) fn find_shim() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("PENGUDROID_SHIM") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let candidate = dir.join("libpengudroid_shim.so");
    if candidate.exists() {
        Some(candidate)
    } else {
        None
    }
}

pub fn run(args: &[String]) -> i32 {
    let verbose = args.iter().any(|a| a == "-v" || a == "--verbose");
    let Some(path) = args.iter().find(|a| !a.starts_with('-')) else {
        eprintln!("usage: pengudroid deps [-v] <lib.so>");
        return 2;
    };

    let info = match read_file(path) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("pengudroid: {e}");
            return 1;
        }
    };

    let imports: BTreeSet<&str> = info.imports.iter().map(String::as_str).collect();
    let bionic: Vec<&str> = imports.iter().copied().filter(|s| is_bionic(s)).collect();

    let shim_path = find_shim();
    let shim_exports: BTreeSet<String> = match &shim_path {
        Some(p) => match read_file(p.to_str().unwrap_or_default()) {
            Ok(e) => e.exports.into_iter().collect(),
            Err(e) => {
                eprintln!("pengudroid: warning: cannot read shim: {e}");
                BTreeSet::new()
            }
        },
        None => {
            eprintln!("pengudroid: warning: libpengudroid_shim.so not found (set PENGUDROID_SHIM)");
            BTreeSet::new()
        }
    };

    let provided: Vec<&str> = bionic.iter().copied().filter(|s| shim_exports.contains(*s)).collect();
    let missing: Vec<&str> = bionic.iter().copied().filter(|s| !shim_exports.contains(*s)).collect();

    let class = match info.class {
        Class::Elf32 => "ELF32",
        Class::Elf64 => "ELF64",
    };
    println!("file:    {} ({}, {})", path, class, machine_name(info.machine));
    println!("imports: {} ({} bionic-specific)", imports.len(), bionic.len());
    println!("exports: {}", info.exports.len());
    match &shim_path {
        Some(p) => println!("shim:    {} ({} symbols)", p.display(), shim_exports.len()),
        None => println!("shim:    <not found>"),
    }

    if bionic.is_empty() {
        println!("bionic coverage: n/a (no Android-specific imports)");
    } else {
        let pct = provided.len() * 100 / bionic.len();
        println!("bionic coverage: {}/{} ({}%)", provided.len(), bionic.len(), pct);
        if !missing.is_empty() {
            println!("missing (would fail to load):");
            for m in &missing {
                println!("  - {m}");
            }
        }
    }

    if verbose {
        println!("\nall imports:");
        for s in &imports {
            let tag = if shim_exports.contains(*s) {
                "shim"
            } else if is_bionic(s) {
                "MISS"
            } else {
                "host"
            };
            println!("  [{tag:>4}] {s}");
        }
    }

    if missing.is_empty() {
        0
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_bionic_symbols() {
        assert!(is_bionic("__android_log_print"));
        assert!(is_bionic("ashmem_create_region"));
        assert!(is_bionic("AAssetManager_open"));
        assert!(is_bionic("__system_property_get"));
        assert!(is_bionic("android_get_device_api_level"));
        assert!(is_bionic("property_get"));
        assert!(is_bionic("__errno"));
        assert!(!is_bionic("malloc"));
        assert!(!is_bionic("memcpy"));
        assert!(!is_bionic("pthread_create"));
    }
}
