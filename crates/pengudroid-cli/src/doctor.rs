//! `pengudroid doctor <app.apk>` — can this APK run, and if not, why?
//!
//! PenguDroid runs an app one of three ways:
//!
//! * **NativeActivity** apps for the host ABI (SDL2, pure-NDK engines): their
//!   `libmain.so` exports `ANativeActivity_onCreate`, and `pengudroid native`
//!   hosts the lifecycle directly — no ART needed.
//! * **Java/Kotlin** apps, with or without host-ABI native code, on the hosted
//!   ART (`pengudroid launch`, given an Android 17 system tree).
//! * **ARM64-only** apps on the same ART, with their native code executed by
//!   the native bridge.
//!
//! `doctor` reports which of those applies and the facts that decide whether it
//! will get anywhere: the ABIs shipped, the engine, which system libraries the
//! native code references, whether the app uses a `WebView`, and whether the
//! APK carries assets. The list of what is missing comes from
//! [`crate::requirements`].

use std::collections::BTreeSet;

use pengudroid_elf::Class;

use crate::apk;
use crate::nativelib::host_abi;
use crate::requirements::{self, Engine, Requirement, Status};

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
    /// Number of split APKs the bundle shipped (0 for a plain APK).
    pub splits: usize,
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
    /// The ABI whose libraries would run: the host's when shipped, else ARM64.
    pub run_abi: Option<String>,
    pub engine: Option<Engine>,
    pub requirements: Vec<Requirement>,
}

impl Report {
    /// The one-line verdict.
    pub fn verdict(&self) -> &'static str {
        if self.native_entry.is_some() && self.host_abi_present {
            return "runnable now: NativeActivity app (pengudroid native)";
        }
        let translated = self.native_libs > 0 && !self.host_abi_present;
        if translated && self.run_abi.as_deref() != Some("arm64-v8a") {
            return if self.abis.iter().any(|a| a.starts_with("arm")) {
                "not runnable: 32-bit ARM code only (the translator is ARM64)"
            } else {
                "not runnable: no native libraries for this host's ABI"
            };
        }
        // A missing piece is not automatically fatal: Beach Buggy Racing 2
        // references a WebView and Play services through its ad SDKs and plays
        // without either. Whether a gap stops *this* app is only known by
        // running it, so the verdict names the gaps instead of predicting.
        match (translated, self.has_gaps()) {
            (true, true) => "ARM64 app: launches on ART + the native bridge; some things it uses are missing (see below)",
            (true, false) => "ARM64 app: runs on ART + the native bridge (pengudroid launch)",
            (false, true) => "launches on ART; some things it uses are missing (see below)",
            (false, false) => "runs on ART (pengudroid launch)",
        }
    }

    /// Whether anything the app references is not implemented.
    pub fn has_gaps(&self) -> bool {
        self.requirements.iter().any(|r| r.status == Status::Missing)
    }
}

/// Analyse `path` and print the report. Returns a process exit code: 0 when the
/// app can be launched, 1 when it cannot.
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
    // 0 when the app can be launched at all; the gaps are in the report.
    if report.verdict().starts_with("not runnable") {
        1
    } else {
        0
    }
}

/// Collect everything `doctor` reports.
pub fn analyse(path: &str) -> Result<Report, String> {
    let p = std::path::Path::new(path);
    if crate::bundle::is_bundle(p) {
        return analyse_bundle(p);
    }
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
    let dex = dex_names(&entries)
        .iter()
        .filter_map(|name| apk::extract_named(path, name).ok())
        .collect();
    let libs = libs
        .into_iter()
        .map(|l| {
            let data = apk::extract_named(path, &l.entry).ok();
            (l, data)
        })
        .collect();
    Ok(build_report(info, entries, libs, dex, 0))
}

/// The same analysis for a Play-style bundle.
///
/// The library facts live in the ABI split, not in `base.apk`, so reading only
/// the base would make an ARM game look like it ships no native code at all.
fn analyse_bundle(path: &std::path::Path) -> Result<Report, String> {
    let b = crate::bundle::Bundle::open(path)?;
    let info = match b.info() {
        Ok(info) => info,
        Err(e) => {
            eprintln!("warning: {e}");
            apk::ApkInfo::default()
        }
    };
    let base = b.base_bytes()?;
    let mut entries = apk::apk_entries_bytes(&base).unwrap_or_default();
    let dex = dex_names(&entries)
        .iter()
        .filter_map(|name| apk::extract_named_bytes(&base, name).ok())
        .collect();
    let mut libs = Vec::new();
    for (member, lib) in b.native_libs()? {
        // Read the bytes now: the library lives in a split, not in the base.
        let data = b.member(&member).ok().and_then(|bytes| {
            apk::extract_named_bytes(&bytes, &lib.entry).ok()
        });
        libs.push((lib, data));
    }
    for split in &b.splits {
        if let Ok(bytes) = b.member(&split.name) {
            if let Ok(more) = apk::apk_entries_bytes(&bytes) {
                for name in more {
                    if !entries.contains(&name) {
                        entries.push(name);
                    }
                }
            }
        }
    }
    Ok(build_report(info, entries, libs, dex, b.splits.len()))
}

/// `classes.dex`, `classes2.dex`, ... in order. A large app spreads its code
/// over many of them, and the WebView it uses may be in any.
fn dex_names(entries: &[String]) -> Vec<String> {
    let mut names: Vec<(u32, String)> = entries
        .iter()
        .filter_map(|e| {
            let number = e.strip_prefix("classes")?.strip_suffix(".dex")?;
            let index = if number.is_empty() { 1 } else { number.parse::<u32>().ok()? };
            Some((index, e.clone()))
        })
        .collect();
    names.sort();
    names.into_iter().map(|(_, name)| name).collect()
}

fn build_report(
    info: apk::ApkInfo,
    entries: Vec<String>,
    libs: Vec<(apk::NativeLib, Option<Vec<u8>>)>,
    dex: Vec<Vec<u8>>,
    splits: usize,
) -> Report {
    let mut report = Report {
        package: info.package.clone(),
        version: info.version_name.clone(),
        min_sdk: info.min_sdk,
        target_sdk: info.target_sdk,
        main_activity: info.main_activity.clone(),
        native_libs: libs.len(),
        splits,
        ..Default::default()
    };

    let mut abis: BTreeSet<String> = BTreeSet::new();
    for (lib, _) in &libs {
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

    // DEX: count classes from every classesN.dex header.
    let counts: Vec<u32> = dex.iter().filter_map(|bytes| dex_class_count(bytes)).collect();
    if !counts.is_empty() {
        report.dex_classes = Some(counts.iter().sum());
    }

    // Native libraries for the host ABI: entry point, graphics imports, and
    // whether they load at all.
    let mut imports: BTreeSet<String> = BTreeSet::new();
    for (lib, data) in libs.iter().filter(|(l, _)| l.abi == host) {
        let Some(bytes) = data.as_ref() else { continue };
        let Ok(elf) = pengudroid_elf::parse(bytes) else { continue };
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

    // What the app needs from the platform, read from the libraries that would
    // actually execute: the host's when the APK ships them, otherwise ARM64.
    let run_abi = if report.host_abi_present {
        Some(host.to_string())
    } else if report.abis.iter().any(|a| a == "arm64-v8a") {
        Some("arm64-v8a".to_string())
    } else {
        report.abis.first().cloned()
    };
    let mut facts = requirements::Facts {
        abi: run_abi.clone().unwrap_or_default(),
        host_abi: report.host_abi_present,
        screen_orientation: info
            .main_activity
            .as_deref()
            .and_then(|a| info.screen_orientation(&info.resolve(a))),
        has_keymap_preset: pengudroid_input::presets::packages().contains(&info.package.as_str()),
        ..Default::default()
    };
    for (lib, data) in libs.iter().filter(|(l, _)| Some(&l.abi) == run_abi.as_ref()) {
        facts.libs.push(lib.file.clone());
        if let Some(bytes) = data {
            requirements::system_libs_in(bytes, &mut facts.system_libs);
        }
    }
    for bytes in &dex {
        let (webview, play) = requirements::dex_uses(bytes);
        facts.uses_webview |= webview;
        facts.uses_play_services |= play;
    }
    if !report.host_abi_present {
        // The import-based flags above only see host-ABI libraries.
        let links = |name: &str| facts.system_libs.contains(&name);
        report.uses_egl = links("libEGL.so");
        report.uses_gles = ["libGLESv1_CM.so", "libGLESv2.so", "libGLESv3.so"].iter().any(|n| links(n));
        report.uses_vulkan = links("libvulkan.so");
        report.uses_opensl = links("libOpenSLES.so");
    }
    report.engine = Engine::detect(&facts.libs);
    report.requirements = requirements::assess(&facts);
    report.run_abi = run_abi;

    report
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
    if r.splits > 0 {
        println!("bundle splits:    {}", r.splits);
    }
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
    if let Some(engine) = r.engine {
        println!("engine:           {}", engine.name());
    }
    println!();
    println!("verdict:          {}", r.verdict());
    if r.native_entry.is_some() && r.host_abi_present {
        println!("run it with:      pengudroid native {path}");
    } else if !r.verdict().starts_with("not runnable") {
        println!("run it with:      pengudroid launch {path}");
        println!("                  (needs an Android 17 system tree; see `pengudroid sysroot`)");
    }
    if !r.requirements.is_empty() {
        println!();
        println!("What it needs:");
        for req in &r.requirements {
            println!("  [{:>7}] {}", req.status.tag(), req.name);
            println!("            {}", req.note);
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
        let mut r = Report {
            native_libs: 3,
            host_abi_present: true,
            abis: vec!["x86_64".into()],
            run_abi: Some("x86_64".into()),
            ..Default::default()
        };
        assert_eq!(r.verdict(), "runs on ART (pengudroid launch)");
        r.native_entry = Some("libmain.so".into());
        assert!(r.verdict().contains("runnable now"));
        r.host_abi_present = false;
        r.abis = vec!["x86".into()];
        r.run_abi = Some("x86".into());
        assert!(r.verdict().contains("no native libraries"));
    }

    #[test]
    fn arm64_apps_are_no_longer_written_off() {
        let mut r = Report {
            native_libs: 3,
            abis: vec!["arm64-v8a".into()],
            run_abi: Some("arm64-v8a".into()),
            ..Default::default()
        };
        assert!(r.verdict().starts_with("ARM64 app: runs"));
        assert!(!r.has_gaps());
        r.requirements.push(Requirement {
            name: "WebView".into(),
            status: Status::Missing,
            note: String::new(),
        });
        assert!(r.has_gaps());
        assert!(r.verdict().contains("missing"));
        r.abis = vec!["armeabi-v7a".into()];
        r.run_abi = Some("armeabi-v7a".into());
        assert!(r.verdict().starts_with("not runnable: 32-bit ARM"));
    }

    #[test]
    fn dex_files_are_listed_in_load_order() {
        let entries: Vec<String> = ["classes10.dex", "lib/x/classes.dex", "classes.dex", "classes2.dex", "classesX.dex"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(dex_names(&entries), ["classes.dex", "classes2.dex", "classes10.dex"]);
    }
}
