//! `pengudroid doctor <app.apk>` — can this APK run, and if not, why?
//!
//! PenguDroid can run two kinds of Android app today:
//!
//! * **NativeActivity** apps (SDL2, pure-NDK engines): their `libmain.so`
//!   exports `ANativeActivity_onCreate`, and `pengudroid native` hosts the
//!   lifecycle directly — no ART needed.
//! * Everything else is a Java/Kotlin app and needs ART, which is not available
//!   yet. `doctor` says so plainly instead of letting `run` produce a window
//!   that never draws.
//!
//! It also reports the facts that decide whether the native path can work: the
//! ABIs shipped, whether the host ABI is present, which graphics APIs the
//! libraries import, and whether the APK carries assets.

use std::collections::BTreeSet;

use pengudroid_elf::Class;

use crate::apk;
use crate::nativelib::host_abi;

/// What the APK needs from the platform.
#[derive(Debug, Default)]
pub struct Report {
    pub package: String,
    pub version: Option<String>,
    pub min_sdk: Option<u32>,
    pub target_sdk: Option<u32>,
    pub main_activity: Option<String>,
    pub abis: Vec<String>,
    pub host_abi_present: bool,
    pub native_libs: usize,
    pub native_entry: Option<String>,
    pub uses_egl: bool,
    pub uses_gles: bool,
    pub uses_vulkan: bool,
    pub uses_opensl: bool,
    pub dex_classes: Option<u32>,
    pub assets: usize,
    pub has_obb: bool,
    pub loadable_libs: usize,
    pub unloadable_libs: Vec<(String, String)>,
}

impl Report {
    /// The one-line verdict.
    pub fn verdict(&self) -> &'static str {
        if self.native_entry.is_some() && self.host_abi_present {
            "runnable now: NativeActivity app (pengudroid native)"
        } else if self.native_libs > 0 && !self.host_abi_present {
            "not runnable: no native libraries for this host's ABI"
        } else if self.native_libs > 0 {
            "needs ART: native libraries exist but no NativeActivity entry point"
        } else {
            "needs ART: pure Java/Kotlin app (no native code)"
        }
    }
}

/// Analyse `path` and print the report. Returns a process exit code: 0 when the
/// app can run today, 1 when it cannot.
pub fn run(args: &[String]) -> i32 {
    let Some(path) = args.iter().find(|a| !a.starts_with('-')) else {
        eprintln!("usage: pengudroid doctor <app.apk>");
        return 2;
    };
    let report = match analyse(path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("pengudroid: {e}");
            return 1;
        }
    };
    print_report(&report, path);
    if report.native_entry.is_some() && report.host_abi_present {
        0
    } else {
        1
    }
}

/// Collect everything `doctor` reports.
pub fn analyse(path: &str) -> Result<Report, String> {
    // A manifest that cannot be parsed is not fatal: the native-library facts
    // are what decide runnability, and a hand-built test APK may carry a
    // placeholder manifest.
    let info = match apk::read_apk(path) {
        Ok(info) => info,
        Err(e) => {
            eprintln!("warning: {e}");
            apk::ApkInfo::default()
        }
    };
    let entries = apk::apk_entries(path)?;
    let libs = apk::native_libs(path)?;

    let mut report = Report {
        package: info.package.clone(),
        version: info.version_name.clone(),
        min_sdk: info.min_sdk,
        target_sdk: info.target_sdk,
        main_activity: info.main_activity.clone(),
        native_libs: libs.len(),
        ..Default::default()
    };

    let mut abis: BTreeSet<String> = BTreeSet::new();
    for lib in &libs {
        abis.insert(lib.abi.clone());
    }
    report.abis = abis.into_iter().collect();
    let host = host_abi();
    report.host_abi_present = report.abis.iter().any(|a| a == host);

    // Assets and OBBs.
    for entry in &entries {
        if entry.starts_with("assets/") && !entry.ends_with('/') {
            report.assets += 1;
        }
        if entry.starts_with("assets/") && entry.ends_with(".obb") {
            report.has_obb = true;
        }
    }

    // DEX: count classes from the first classes.dex header.
    if entries.iter().any(|e| e == "classes.dex") {
        if let Ok(bytes) = apk::extract_named(path, "classes.dex") {
            report.dex_classes = dex_class_count(&bytes);
        }
    }

    // Native libraries for the host ABI: entry point, graphics imports, and
    // whether they load at all.
    let mut imports: BTreeSet<String> = BTreeSet::new();
    for lib in libs.iter().filter(|l| l.abi == host) {
        let Ok(bytes) = apk::extract_named(path, &lib.entry) else { continue };
        let Ok(elf) = pengudroid_elf::parse(&bytes) else { continue };
        if elf.exports.iter().any(|s| s == "ANativeActivity_onCreate") {
            report.native_entry = Some(lib.file.clone());
        }
        for sym in &elf.imports {
            imports.insert(sym.clone());
        }
        match crate::nativelib::host_can_load(&elf) {
            Ok(()) => report.loadable_libs += 1,
            Err(e) => report.unloadable_libs.push((lib.file.clone(), e)),
        }
        let _ = Class::Elf64;
    }
    report.uses_egl = imports.iter().any(|s| s.starts_with("egl"));
    report.uses_gles = imports
        .iter()
        .any(|s| s.starts_with("gl") && !s.starts_with("glib"));
    report.uses_vulkan = imports.iter().any(|s| s.starts_with("vk"));
    report.uses_opensl = imports.iter().any(|s| s.starts_with("slCreate") || s.starts_with("SL_IID"));

    Ok(report)
}

/// `classes.dex` header: `class_defs_size` is a little-endian u32 at offset 96.
fn dex_class_count(bytes: &[u8]) -> Option<u32> {
    if bytes.len() < 112 || !bytes.starts_with(b"dex\n") {
        return None;
    }
    Some(u32::from_le_bytes(bytes[96..100].try_into().ok()?))
}

fn yesno(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

fn print_report(r: &Report, path: &str) {
    println!("package:        {}", if r.package.is_empty() { "<unknown>" } else { &r.package });
    if let Some(v) = &r.version {
        println!("version:        {v}");
    }
    if let Some(sdk) = r.min_sdk {
        println!("minSdk:         {sdk}");
    }
    if let Some(sdk) = r.target_sdk {
        println!("targetSdk:      {sdk}");
    }
    if let Some(a) = &r.main_activity {
        println!("main activity:  {a}");
    }
    println!();
    println!("native libraries: {}", r.native_libs);
    println!("ABIs:             {}", if r.abis.is_empty() { "<none>".into() } else { r.abis.join(", ") });
    println!("host ABI ({}): {}", host_abi(), yesno(r.host_abi_present));
    if r.native_libs > 0 {
        println!("loadable (host):  {}/{}", r.loadable_libs, r.native_libs);
        for (lib, why) in &r.unloadable_libs {
            println!("  [skip] {lib}: {why}");
        }
    }
    println!(
        "NativeActivity:   {}",
        r.native_entry.as_deref().unwrap_or("not found")
    );
    println!();
    println!("graphics:         EGL={} GLES={} Vulkan={}", yesno(r.uses_egl), yesno(r.uses_gles), yesno(r.uses_vulkan));
    println!("audio:            OpenSL ES={}", yesno(r.uses_opensl));
    println!(
        "DEX:              {}",
        match r.dex_classes {
            Some(n) => format!("{n} classes"),
            None => "none".into(),
        }
    );
    println!("assets:           {} files{}", r.assets, if r.has_obb { ", OBB present" } else { "" });
    println!();
    println!("verdict:          {}", r.verdict());
    if r.native_entry.is_some() && r.host_abi_present {
        println!("run it with:      pengudroid native {path}");
    } else {
        println!();
        println!("What is missing:");
        if !r.host_abi_present {
            println!("  - native libraries for this host's ABI ({})", host_abi());
        }
        if r.native_entry.is_none() {
            println!("  - ART (the Android runtime) to execute DEX bytecode; see docs/LIMITATIONS.md §2");
            println!("    `pengudroid sysroot` reports which Android system files are needed.");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dex_header_class_count() {
        let mut dex = vec![0u8; 112];
        dex[..8].copy_from_slice(b"dex\n035\0");
        dex[96..100].copy_from_slice(&1234u32.to_le_bytes());
        assert_eq!(dex_class_count(&dex), Some(1234));
        assert_eq!(dex_class_count(b"not a dex"), None);
    }

    #[test]
    fn verdict_prefers_native_activity() {
        let mut r = Report { native_libs: 3, host_abi_present: true, ..Default::default() };
        assert!(r.verdict().contains("needs ART"));
        r.native_entry = Some("libmain.so".into());
        assert!(r.verdict().contains("runnable now"));
        r.host_abi_present = false;
        assert!(r.verdict().contains("no native libraries"));
    }
}
