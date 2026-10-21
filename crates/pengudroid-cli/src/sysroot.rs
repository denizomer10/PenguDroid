//! ART bring-up plumbing: `pengudroid sysroot` and `pengudroid art-test`.
//!
//! PenguDroid does not ship an Android system image, but ART and the framework
//! are binaries that must come from somewhere. These commands make that
//! requirement explicit and testable:
//!
//! * `pengudroid sysroot <dir>` inspects an *extracted* Android system tree and
//!   reports which pieces ART needs are present, plus the environment to use.
//! * `pengudroid art-test --sysroot <dir>` re-execs itself as `__art` under the
//!   Bionic shim, loads `libart.so`, and calls `JNI_CreateJavaVM`. It prints
//!   ART's own diagnostic when the runtime cannot start.
//!
//! Two layouts are understood:
//!
//! * **flat** — `lib64/libart.so`, `framework/framework.jar`, ... (what
//!   `pengudroid sysroot` used to require);
//! * **android** — an extracted Android root: `system/lib64`,
//!   `system/apex/com.android.art/lib64`, `system/framework/x86_64/boot.art`,
//!   ... (what a Waydroid/emulator `system.img` contains).

use std::ffi::{c_char, c_int, c_void, CString};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::deps::find_shim;
use crate::nativelib::ensure_compat_dir;

/// The host's Android ABI directory name (`x86_64`, `arm64-v8a`, ...).
fn arch_dir() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "arm64",
        "x86" => "x86",
        "arm" => "arm",
        other => other,
    }
}

/// Where the pieces of an Android system tree live.
#[derive(Debug, Clone)]
pub struct Layout {
    pub root: PathBuf,
    /// Directories to put on `LD_LIBRARY_PATH`.
    pub lib_dirs: Vec<PathBuf>,
    pub art_root: PathBuf,
    pub framework: PathBuf,
    /// `framework/<arch>`, holding `boot.art`/`boot.oat`/`boot.vdex`.
    pub boot_images: PathBuf,
    /// The boot classpath, in order.
    pub boot_jars: Vec<PathBuf>,
    /// The locations ART's boot image was built with (for `-Xbootclasspath-locations`).
    pub boot_locations: Vec<String>,
    pub apex_manifest: PathBuf,
    pub android_root: PathBuf,
}

/// Collect the boot classpath fragments published by APEXes.
///
/// Every mainline APEX ships `etc/classpaths/bootclasspath.pb`; init
/// concatenates them into `BOOTCLASSPATH` on a device. Sorted by APEX name so
/// the order is stable across runs.
fn apex_bootclasspath_fragments(system: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let Ok(entries) = std::fs::read_dir(system.join("apex")) else {
        return out;
    };
    let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    dirs.sort();
    for dir in dirs {
        let pb = dir.join("etc/classpaths/bootclasspath.pb");
        let Ok(data) = std::fs::read(&pb) else { continue };
        for path in jar_paths_in_bootclasspath_pb(&data) {
            if !out.contains(&path) {
                out.push(path);
            }
        }
    }
    out
}

/// Read a protobuf varint, returning the value and the bytes consumed.
fn read_varint(data: &[u8], mut pos: usize) -> Option<(usize, usize)> {
    let start = pos;
    let mut value = 0usize;
    let mut shift = 0;
    loop {
        let byte = *data.get(pos)?;
        pos += 1;
        value |= ((byte & 0x7f) as usize) << shift;
        if byte < 0x80 {
            return Some((value, pos - start));
        }
        shift += 7;
        if shift > 28 {
            return None;
        }
    }
}

/// Extract the jar paths from a serialized `BootClasspath` message.
///
/// Layout, verified against the image's fragments:
///
/// ```text
/// BootClasspath { repeated Jar paths = 1; }
/// Jar           { string path = 1; }
/// ```
///
/// A tiny hand-written parser keeps the CLI free of a protobuf dependency.
fn jar_paths_in_bootclasspath_pb(data: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < data.len() {
        // Outer field 1, wire type 2 (length-delimited).
        if data[pos] != 0x0a {
            break;
        }
        pos += 1;
        let Some((len, used)) = read_varint(data, pos) else { break };
        pos += used;
        let end = pos.saturating_add(len).min(data.len());
        if pos < end && data[pos] == 0x0a {
            let mut inner = pos + 1;
            if let Some((slen, u2)) = read_varint(data, inner) {
                inner += u2;
                if inner + slen <= data.len() {
                    if let Ok(s) = std::str::from_utf8(&data[inner..inner + slen]) {
                        out.push(s.to_string());
                    }
                }
            }
        }
        pos = end;
    }
    out
}

impl Layout {
    /// Detect the layout under `root`.
    pub fn detect(root: &Path) -> Result<Layout, String> {
        // Everything downstream builds absolute guest paths from `root`
        // (`ANDROID_ROOT + "/etc/fonts.xml"`, boot class path locations, ...),
        // so a relative `--sysroot` would make them cwd-dependent or plain wrong.
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let root = root.as_path();
        let flat_art = root.join("lib64/libart.so");
        let android_art = root.join("system/apex/com.android.art/lib64/libart.so");
        if flat_art.exists() {
            Ok(Self::flat(root))
        } else if android_art.exists() {
            Ok(Self::android(root))
        } else {
            Err(format!(
                "{} contains neither lib64/libart.so nor \
                 system/apex/com.android.art/lib64/libart.so",
                root.display()
            ))
        }
    }

    fn flat(root: &Path) -> Layout {
        let art_root = root.join("apex/com.android.art");
        let framework = root.join("framework");
        let mut boot_jars = Vec::new();
        for name in [
            "core-oj.jar",
            "core-libart.jar",
            "okhttp.jar",
            "bouncycastle.jar",
            "apache-xml.jar",
        ] {
            let p = art_root.join("javalib").join(name);
            if p.exists() {
                boot_jars.push(p);
            }
        }
        for name in [
            "framework.jar",
            "framework-graphics.jar",
            "ext.jar",
            "telephony-common.jar",
            "voip-common.jar",
            "ims-common.jar",
        ] {
            let p = framework.join(name);
            if p.exists() {
                boot_jars.push(p);
            }
        }
        Layout {
            root: root.to_path_buf(),
            lib_dirs: vec![root.join("lib64")],
            art_root,
            framework: framework.clone(),
            boot_images: framework.join(arch_dir()),
            boot_locations: boot_jars.iter().map(|p| p.display().to_string()).collect(),
            boot_jars,
            apex_manifest: root.join("apex/com.android.art/apex_manifest.pb"),
            android_root: root.to_path_buf(),
        }
    }

    fn android(root: &Path) -> Layout {
        let system = root.join("system");
        let art_root = system.join("apex/com.android.art");
        let framework = system.join("framework");
        // Every APEX ships its own lib64; ART's dependency closure reaches
        // several of them (statsd, i18n, conscrypt, ...), so all are added.
        let mut lib_dirs = vec![system.join("lib64"), art_root.join("lib64")];
        if let Ok(entries) = std::fs::read_dir(system.join("apex")) {
            let mut apex_libs: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path().join("lib64"))
                .filter(|p| p.is_dir())
                .collect();
            apex_libs.sort();
            for p in apex_libs {
                if !lib_dirs.contains(&p) {
                    lib_dirs.push(p);
                }
            }
        }

        // The ART APEX's boot classpath, then the framework jars. The locations
        // are the paths the boot image was built with.
        let mut boot_jars = Vec::new();
        let mut boot_locations = Vec::new();
        for name in ["core-oj.jar", "core-libart.jar", "okhttp.jar", "bouncycastle.jar", "apache-xml.jar"] {
            let p = art_root.join("javalib").join(name);
            if p.exists() {
                boot_jars.push(p);
                boot_locations.push(format!("/apex/com.android.art/javalib/{name}"));
            }
        }
        // ICU4J lives in the i18n APEX and is part of the boot classpath.
        let icu = system.join("apex/com.android.i18n/javalib/core-icu4j.jar");
        if icu.exists() {
            boot_jars.push(icu);
            boot_locations.push("/apex/com.android.i18n/javalib/core-icu4j.jar".into());
        }
        for name in [
            "framework.jar",
            "framework-graphics.jar",
            "ext.jar",
            "telephony-common.jar",
            "voip-common.jar",
            "ims-common.jar",
        ] {
            let p = framework.join(name);
            if p.exists() {
                boot_jars.push(p);
                boot_locations.push(format!("/system/framework/{name}"));
            }
        }

        // Extra boot classpath entries that are *not* covered by the boot image:
        // Android 13 moved connectivity, bluetooth, conscrypt, permission and
        // friends into mainline APEX modules, each publishing its own
        // `etc/classpaths/bootclasspath.pb` fragment. On a device init
        // concatenates those fragments into BOOTCLASSPATH, so the boot loader
        // can resolve the classes; without them a native FindClass for (say)
        // `android.net.ProxyInfo` fails with "Class not found using the boot
        // class loader". The image covers the first 12 entries, so these are
        // appended after them.
        for loc in apex_bootclasspath_fragments(&system) {
            if boot_locations.contains(&loc) {
                continue;
            }
            let Some(rest) = loc.strip_prefix("/apex/") else { continue };
            let file = system.join("apex").join(rest);
            if file.exists() {
                boot_jars.push(file);
                boot_locations.push(loc);
            }
        }

        Layout {
            root: root.to_path_buf(),
            lib_dirs,
            art_root,
            framework: framework.clone(),
            boot_images: framework.join(arch_dir()),
            boot_jars,
            boot_locations,
            apex_manifest: system.join("apex/com.android.art/apex_manifest.pb"),
            android_root: system,
        }
    }

    /// Files ART needs, with a description.
    pub fn required(&self) -> Vec<(String, PathBuf)> {
        let mut out = vec![
            ("ART runtime".into(), self.art_root.join("lib64/libart.so")),
            ("ART base".into(), self.art_root.join("lib64/libartbase.so")),
            ("DEX support".into(), self.art_root.join("lib64/libdexfile.so")),
            ("JNI helpers".into(), self.art_root.join("lib64/libnativehelper.so")),
            ("Bionic libc".into(), self.lib_dirs[0].join("libc.so")),
            ("boot image".into(), self.boot_images.join("boot.art")),
            ("boot image (oat)".into(), self.boot_images.join("boot.oat")),
            ("framework classes".into(), self.framework.join("framework.jar")),
            ("ART APEX manifest".into(), self.apex_manifest.clone()),
        ];
        if self.boot_jars.is_empty() {
            out.push(("boot classpath jars".into(), self.art_root.join("javalib/core-oj.jar")));
        }
        out
    }

    /// `LD_LIBRARY_PATH` value.
    pub fn ld_library_path(&self) -> String {
        self.lib_dirs.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(":")
    }

    /// `-Xbootclasspath:` value.
    pub fn boot_classpath(&self) -> String {
        self.boot_jars.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(":")
    }

    /// `-Xbootclasspath-locations:` value.
    pub fn boot_classpath_locations(&self) -> String {
        self.boot_locations.join(":")
    }
}

/// The APK paths given to `--classpath`, in order.
fn classpath_apks(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--classpath" {
            if let Some(v) = it.next() {
                out.push(v.clone());
            }
        }
    }
    out
}

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
    let layout = match Layout::detect(root) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("pengudroid: {e}");
            return 1;
        }
    };

    println!("sysroot:  {}", layout.root.display());
    println!("layout:   {}", if layout.root.join("system").is_dir() { "android" } else { "flat" });
    println!("art root: {}", layout.art_root.display());
    println!("boot:     {}", layout.boot_images.display());
    println!();
    let mut missing = 0;
    println!("required:");
    for (what, path) in layout.required() {
        if path.exists() {
            println!("  [ ok ] {:<22} {}", what, path.display());
        } else {
            println!("  [MISS] {:<22} {}", what, path.display());
            missing += 1;
        }
    }
    println!("\nboot classpath ({} jars):", layout.boot_jars.len());
    for jar in &layout.boot_jars {
        println!("  {}", jar.display());
    }
    println!("\nenvironment for a guest:");
    println!("  ANDROID_ROOT={}", layout.android_root.display());
    println!("  ANDROID_ART_ROOT={}", layout.art_root.display());
    println!("  ANDROID_DATA={}", layout.root.join("data").display());
    println!("  BOOTCLASSPATH={}", layout.boot_classpath());
    println!("  LD_LIBRARY_PATH={}", layout.ld_library_path());

    if missing == 0 {
        println!("\nverdict: complete — try `pengudroid art-test --sysroot {}`", root.display());
        0
    } else {
        println!("\nverdict: incomplete ({missing} required file(s) missing)");
        1
    }
}

/// Sonames the shim provides itself; the Android copies must not shadow them.
const SHIM_SONAMES: &[&str] = &["libc.so", "libm.so", "libdl.so", "liblog.so", "libdl_android.so"];

/// Copy every library ART may load into a scratch directory, with Android's
/// packed relocations expanded and symbol versioning stripped.
///
/// glibc cannot apply `ANDROID_RELA`/`APS2` relocations, so a modern Android
/// `.so` loaded as-is would run with *no* relocations applied. `de-bionic` fixes
/// that; the copies are what `__art` actually loads.
fn prepare_art_libs(layout: &Layout, work: &Path) -> Result<PathBuf, String> {
    let out = work.join("art-libs");
    std::fs::create_dir_all(&out).map_err(|e| format!("cannot create {}: {e}", out.display()))?;
    let mut written = 0usize;
    for dir in &layout.lib_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
            if !name.contains(".so") || SHIM_SONAMES.contains(&name) {
                continue;
            }
            let dest = out.join(name);
            if dest.exists() {
                continue;
            }
            let Ok(data) = std::fs::read(&path) else { continue };
            match pengudroid_elf::strip_android_versions(&data) {
                Ok(stripped) => {
                    if std::fs::write(&dest, stripped).is_ok() {
                        written += 1;
                    }
                }
                Err(_) => {
                    // Not an ELF we understand (a linker script, a text file):
                    // copy it verbatim so the loader still finds it.
                    if std::fs::copy(&path, &dest).is_ok() {
                        written += 1;
                    }
                }
            }
        }
    }
    println!("art: prepared {written} libraries in {}", out.display());
    Ok(out)
}

/// Remove work directories left behind by earlier runs.
///
/// Each run copies the whole de-bionic'd library closure (~250 MB) into a
/// per-pid directory. On a tmpfs `/tmp` a handful of runs will fill it, and the
/// failure mode is silent (every write fails, so ART sees an empty directory).
fn prune_stale_work_dirs() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    let me = std::process::id();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(pid) = name.strip_prefix("pengudroid-art-") else {
            continue;
        };
        let Ok(pid) = pid.parse::<u32>() else { continue };
        if pid == me {
            continue;
        }
        // A pid that is still alive owns its directory; anything else is stale.
        let alive = Path::new(&format!("/proc/{pid}")).exists();
        if !alive {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Copy the top-level `.so` files of `src` into `dst` with their Android symbol
/// versions stripped. Subdirectories (the ARM guest runtime) are left alone:
/// those are ARM ELF files that only the translator's own loader reads.
fn de_bionic_dir(src: &Path, dst: &Path) -> Result<usize, String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("cannot create {}: {e}", dst.display()))?;
    let entries = std::fs::read_dir(src)
        .map_err(|e| format!("cannot read {}: {e}", src.display()))?;
    let mut written = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.contains(".so") {
            continue;
        }
        let Ok(data) = std::fs::read(&path) else { continue };
        let out = pengudroid_elf::strip_android_versions(&data).unwrap_or(data);
        if std::fs::write(dst.join(name), out).is_ok() {
            written += 1;
        }
    }
    Ok(written)
}

/// `pengudroid art-test --sysroot <dir>`
pub fn art_test(args: &[String]) -> i32 {
    let mut root: Option<PathBuf> = None;
    let mut passthrough: Vec<String> = Vec::new();
    let mut native_bridge: Option<String> = None;
    let mut bridge_lib: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--sysroot" => {
                root = it.next().map(PathBuf::from);
            }
            "--native-bridge" => native_bridge = it.next().cloned(),
            "--bridge-lib" => bridge_lib = it.next().cloned(),
            other if other.starts_with('-') => {
                passthrough.push(other.to_string());
                // Options that take a value: keep the next token with them.
                if matches!(
                    other,
                    "--main" | "--find-class" | "--classpath" | "--jni-lib" | "--register-natives"
                        | "--init-bridge" | "--native-bridge" | "--bridge-lib" | "--xmx"
                        | "--bridge-selftest" | "--vm-option" | "--dump-class"
                ) {
                    if let Some(v) = it.next() {
                        passthrough.push(v.clone());
                    }
                }
            }
            other => root = Some(PathBuf::from(other)),
        }
    }
    let Some(root) = root else {
        eprintln!("usage: pengudroid art-test --sysroot <dir>");
        return 2;
    };
    let layout = match Layout::detect(&root) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("pengudroid: {e}");
            return 1;
        }
    };
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

    let work = std::env::temp_dir().join(format!("pengudroid-art-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    prune_stale_work_dirs();
    let art_libs = match prepare_art_libs(&layout, &work) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("pengudroid: {e}");
            return 1;
        }
    };

    let mut cmd = if let Some(wrap) = std::env::var_os("PENGUDROID_DEBUG_WRAP") {
        // Diagnostic hook: prefix the ART helper with a wrapper such as a
        // ptrace tracer. The value may carry wrapper arguments, e.g.
        // `PENGUDROID_DEBUG_WRAP="/tmp/bp 0x41cd60"`.
        let text = wrap.to_string_lossy().into_owned();
        let mut parts = text.split_whitespace();
        let mut c = Command::new(parts.next().unwrap_or(""));
        for p in parts {
            c.arg(p);
        }
        c.arg(&exe);
        c
    } else {
        Command::new(&exe)
    };
    cmd.arg("__art")
        .arg("--sysroot")
        .arg(&root)
        .arg("--libs")
        .arg(&art_libs);
    cmd.args(&passthrough);
    let mut ld_path = format!(
        "{}:{}:{}",
        art_libs.display(),
        compat.display(),
        layout.ld_library_path()
    );
    // `System.loadLibrary` ends up in `dlopen`, and glibc does not know about
    // the class loader's native library directories (that is an Android linker
    // feature), so the app's own `lib/<abi>/*.so` have to be on
    // `LD_LIBRARY_PATH` -- which glibc reads once, before the process starts.
    for apk in classpath_apks(&passthrough) {
        let Ok(info) = crate::apk::read_apk(&apk) else { continue };
        let package = if info.package.is_empty() {
            "org.pengudroid.app".to_string()
        } else {
            info.package.clone()
        };
        let dir = std::env::temp_dir().join(format!("pengudroid-libs/{package}"));
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(libs) = crate::apk::native_libs(&apk) {
            for lib in libs.iter().filter(|l| l.abi == "x86_64") {
                let dest = dir.join(&lib.file);
                if dest.exists() {
                    continue;
                }
                if let Ok(bytes) = crate::apk::extract_named(&apk, &lib.entry) {
                    // The APK's libraries carry Bionic symbol versions
                    // (`__register_atfork@LIBC`, ...) that glibc cannot match,
                    // so they need the same treatment as the system libraries.
                    let out = pengudroid_elf::strip_android_versions(&bytes).unwrap_or(bytes);
                    let _ = std::fs::write(&dest, out);
                }
            }
        }
        ld_path = format!("{}:{ld_path}", dir.display());
    }
    if let Some(dir) = &bridge_lib {
        // The translator is a normal x86-64 `.so` but carries Bionic VERNEED
        // entries (`LIBLOG`, `LIBC`, ...) and references like
        // `pthread_exit@LIBC` that glibc neither provides nor version-matches.
        // Stripping the versions makes those references resolve by name
        // against the shim or glibc itself.
        let prepared = work.join("bridge-libs");
        match de_bionic_dir(Path::new(dir), &prepared) {
            Ok(n) => println!("art: prepared {n} bridge libraries in {}", prepared.display()),
            Err(e) => {
                eprintln!("pengudroid: {e}");
                return 1;
            }
        }
        ld_path = format!("{}:{ld_path}", prepared.display());
    }
    // `libpengudroid_gleshim.so` implements Bionic's `libEGL`/`libGLESv2` on the
    // host GPU and presents every `eglSwapBuffers` as a dma-buf. Without it
    // hwui's EGL calls land on the sysroot's Bionic `libEGL`, which has no
    // driver behind it, so nothing is ever drawn.
    let preload = match crate::nativelib::find_gleshim() {
        Some(g) => format!("{}:{}", shim.display(), g.display()),
        None => shim.display().to_string(),
    };
    cmd.env("LD_PRELOAD", &preload)
        .env("LD_LIBRARY_PATH", &ld_path)
        .env("ANDROID_ROOT", &layout.android_root)
        .env("ANDROID_ART_ROOT", &layout.art_root)
        .env("ANDROID_DATA", layout.root.join("data"))
        .env("ANDROID_TZDATA_ROOT", layout.root.join("system/apex/com.android.tzdata"))
        .env("ANDROID_RUNTIME_ROOT", layout.root.join("system/apex/com.android.runtime"))
        .env("ANDROID_I18N_ROOT", layout.root.join("system/apex/com.android.i18n"))
        .env("ANDROID_CONSCRYPT_ROOT", layout.root.join("system/apex/com.android.conscrypt"))
        .env("BOOTCLASSPATH", layout.boot_classpath())
        // The boot class path *locations* ART recorded (`/apex/...`,
        // `/system/...`) do not exist on the host. The shim rewrites lookups
        // under those mount points into this tree, so ART's Java-side boot
        // class loader can open them without a chroot or a mount namespace.
        .env(
            "PENGUDROID_ROOT",
            std::fs::canonicalize(&layout.root).unwrap_or_else(|_| layout.root.clone()),
        )
        .env("PENGUDROID_SUPPRESS_DESTRUCTORS", "1");
    if let Some(bridge) = &native_bridge {
        // Enabling the bridge is pure property configuration, exactly as on a
        // device: ART's libnativebridge dlopens the named library and reads its
        // `NativeBridgeItf` callbacks, and the ISA properties tell ART that ARM
        // code runs on an x86 CPU.
        cmd.env("PENGUDROID_PROP_ro.dalvik.vm.native.bridge", bridge)
            .env("PENGUDROID_PROP_ro.dalvik.vm.isa.arm64", "x86_64")
            .env("PENGUDROID_PROP_ro.dalvik.vm.isa.arm", "x86")
            .env("PENGUDROID_PROP_ro.enable.native.bridge.exec", "1")
            .env("PENGUDROID_PROP_ro.dalvik.vm.isa.arm64.variant", "generic")
            .env("PENGUDROID_PROP_ro.dalvik.vm.isa.arm.variant", "generic");
        println!("art: native bridge = {bridge}");
    }
    match cmd.status() {
        Ok(s) if s.success() => 0,
        Ok(s) => {
            if let Some(code) = s.code() {
                eprintln!("art: helper exited with status {code}");
            } else {
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    eprintln!("art: helper killed by signal {:?}", s.signal());
                }
            }
            1
        }
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
    let mut libs: Option<PathBuf> = None;
    let mut main_class: Option<String> = None;
    let mut probe_class: Option<String> = None;
    let mut classpath: Vec<String> = Vec::new();
    let mut app_args: Vec<String> = Vec::new();
    let mut jni_libs: Vec<String> = Vec::new();
    let mut register_libs: Vec<String> = Vec::new();
    let mut init_bridge: Option<String> = None;
    let mut bridge_selftest: Option<String> = None;
    let mut bind_app = false;
    let mut dump_class: Option<String> = None;
    let mut java_props = false;
    let mut xmx: String = "512m".to_string();
    let mut vm_extra: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--sysroot" => root = it.next().map(PathBuf::from),
            "--libs" => libs = it.next().map(PathBuf::from),
            "--main" => main_class = it.next().cloned(),
            "--find-class" => probe_class = it.next().cloned(),
            "--jni-lib" => {
                if let Some(v) = it.next() {
                    jni_libs.push(v.clone());
                }
            }
            "--register-natives" => {
                if let Some(v) = it.next() {
                    // Comma-separated, because framework natives are split over
                    // several libraries (`libandroid_runtime`, `libhwui`, ...).
                    for part in v.split(',') {
                        let part = part.trim();
                        if !part.is_empty() {
                            register_libs.push(part.to_string());
                        }
                    }
                }
            }
            "--init-bridge" => init_bridge = it.next().cloned(),
            "--bridge-selftest" => bridge_selftest = it.next().cloned(),
            "--bind-app" => bind_app = true,
            "--dump-class" => dump_class = it.next().cloned(),
            "--java-props" => java_props = true,
            "--xmx" => {
                if let Some(v) = it.next() {
                    xmx = v.clone();
                }
            }
            // Escape hatch for diagnostics: pass an arbitrary ART option
            // through (e.g. `-verbose:class` or `-Xlog:class+load`).
            "--vm-option" => {
                if let Some(v) = it.next() {
                    vm_extra.push(v.clone());
                }
            }
            "--classpath" => {
                if let Some(v) = it.next() {
                    classpath.push(v.clone());
                }
            }
            "--" => {
                app_args.extend(it.by_ref().cloned());
                break;
            }
            _ => {}
        }
    }
    let Some(root) = root else {
        eprintln!("art: --sysroot is required");
        return 2;
    };
    let layout = match Layout::detect(&root) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("art: {e}");
            return 1;
        }
    };
    let lib = libs
        .as_ref()
        .map(|d| d.join("libart.so"))
        .filter(|p| p.exists())
        .unwrap_or_else(|| layout.art_root.join("lib64/libart.so"));
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

    let mut options: Vec<CString> = vec![
        CString::new(format!("-Xbootclasspath:{}", layout.boot_classpath())).unwrap_or_default(),
        CString::new(format!(
            "-Xbootclasspath-locations:{}",
            layout.boot_classpath_locations()
        ))
        .unwrap_or_default(),
        // ART appends `<arch>/boot.art` to the image *directory*, so pass the
        // framework dir, not the arch dir (otherwise it looks for
        // `framework/x86_64/x86_64/boot.art`).
        CString::new(format!("-Ximage:{}", layout.framework.join("boot.art").display()))
            .unwrap_or_default(),
        CString::new(format!("-Djava.library.path={}", layout.ld_library_path())).unwrap_or_default(),
        CString::new(format!("-Xmx{xmx}")).unwrap_or_default(),
    ];
    for opt in &vm_extra {
        options.push(CString::new(opt.as_str()).unwrap_or_default());
    }
    if !classpath.is_empty() {
        options.push(CString::new(format!("-Djava.class.path={}", classpath.join(":"))).unwrap_or_default());
    }
    let mut vm_options: Vec<JavaVMOption> = options
        .iter_mut()
        .map(|o| JavaVMOption {
            option_string: o.as_ptr() as *mut c_char,
            extra_info: std::ptr::null_mut(),
        })
        .collect();
    let mut init = JavaVMInitArgs {
        version: JNI_VERSION_1_6,
        n_options: vm_options.len() as c_int,
        options: vm_options.as_mut_ptr(),
        ignore_unrecognized: 1,
    };

    let bridge_setup = if let Some(isa) = init_bridge.as_deref().or(bridge_selftest.as_deref().map(|_| "arm64")) {
        let data_dir = std::env::var_os("PENGUDROID_BRIDGE_DATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("pengudroid-bridge"));
        let _ = std::fs::create_dir_all(&data_dir);
        setup_native_bridge(libs.as_deref(), &data_dir, isa)
    } else {
        None
    };

    let mut vm: *mut c_void = std::ptr::null_mut();
    let mut env: *mut c_void = std::ptr::null_mut();
    println!(
        "art: calling JNI_CreateJavaVM ({} options) main={main_class:?} probe={probe_class:?} cp={classpath:?} jni={jni_libs:?} reg={register_libs:?}",
        vm_options.len()
    );
    // SAFETY: the argument block matches the JNI ABI.
    let rc = unsafe { create(&mut vm, &mut env, &mut init as *mut JavaVMInitArgs as *mut c_void) };
    match rc {
        JNI_OK => {
            println!("art: JNI_CreateJavaVM returned JNI_OK (vm={vm:p}, env={env:p})");
            println!("art: a live JavaVM is available");
            if std::env::var_os("PENGUDROID_TRACE_FINDCLASS").is_some() {
                // SAFETY: `env` is the live JNIEnv returned above.
                unsafe { install_findclass_tracer(env) };
            }
            let env = crate::jni::JniEnv(env);
            let version = env.version();
            println!("art: JNIEnv::GetVersion() = 0x{version:08x}");
            if std::env::var_os("PENGUDROID_DUMP_JNI").is_some() {
                dump_jni_table(&env);
            }
            for lib in &jni_libs {
                if load_jni_library(&env, vm, lib) != 0 {
                    return 1;
                }
            }
            for lib in &register_libs {
                if register_framework_natives(&env, lib, libs.as_deref()) != 0 {
                    return 1;
                }
            }
            if let Some(setup) = &bridge_setup {
                if let Some(spec) = bridge_selftest.as_deref() {
                    // Direct Itf use: no libnativebridge state machine, so no
                    // guest app_process is started.
                    run_bridge_selftest(setup, &env, spec);
                } else {
                    finish_native_bridge(setup, &env);
                }
            }
            if version != crate::jni::JNI_VERSION_1_6 {
                eprintln!("art: unexpected JNI version; the function table is misaligned");
                return 1;
            }
            if let Ok(name) = std::env::var("PENGUDROID_PROBE_SERVICE") {
                probe_service(&env, &name);
            }
            if let Some(name) = dump_class.as_deref() {
                crate::apphost::dump_class(&env, name);
                return 0;
            }
            if let Ok(mode) = std::env::var("PENGUDROID_FONTDEBUG") {
                if mode == "2" {
                    crate::apphost::font_parse_debug(&env);
                } else {
                    crate::apphost::font_debug(&env);
                }
                return 0;
            }
            if std::env::var_os("PENGUDROID_FONTMAP").is_some() {
                crate::apphost::font_map_size(&env);
                return 0;
            }
            if java_props {
                crate::apphost::print_java_props(&env);
                return 0;
            }
            if let Some(class) = main_class {
                if std::env::var_os("PENGUDROID_BINDER_POOL").is_some() {
                    start_binder_pool(libs.as_deref());
                }
                if bind_app {
                    // The Activity has to be created on the thread that owns the
                    // main Looper (androidx enforces it), so set the main thread
                    // up here and bind from it instead of calling
                    // `ActivityThread.main`, which would block in Looper.loop().
                    crate::apphost::prepare_main_thread(&env);
                    bind_application_now(&env, &classpath);
                    crate::apphost::loop_forever(&env);
                    return 0;
                }
                return run_main(&env, &class, &app_args);
            }
            if let Some(class) = probe_class {
                let descriptor = format!("L{};", class.replace('.', "/"));
                return match env.load_class(&descriptor) {
                    Some(_) => {
                        println!("art: resolved {class} from the system class loader");
                        0
                    }
                    None => {
                        eprintln!("art: {class} not found via the system class loader");
                        1
                    }
                };
            }
            0
        }
        JNI_ERR => {
            eprintln!("art: JNI_CreateJavaVM returned JNI_ERR");
            eprintln!(
                "art: ART ran its startup path but could not create the VM. \
                 See docs/LIMITATIONS.md §2 for the remaining Bionic/runtime gaps."
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

/// `NativeBridgeRuntimeCallbacks` — the three hooks ART gives the bridge so it
/// can walk a class's native methods (`libnativebridge` only uses them when the
/// translated code calls `RegisterNatives`).
#[repr(C)]
struct BridgeCallbacks {
    get_method_shorty: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *const c_char,
    get_native_method_count: unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32,
    get_native_methods:
        unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, u32) -> u32,
}

unsafe extern "C" fn bridge_get_method_shorty(_env: *mut c_void, _method: *mut c_void) -> *const c_char {
    c"".as_ptr()
}

unsafe extern "C" fn bridge_get_native_method_count(_env: *mut c_void, _class: *mut c_void) -> u32 {
    0
}

unsafe extern "C" fn bridge_get_native_methods(
    _env: *mut c_void,
    _class: *mut c_void,
    _methods: *mut c_void,
    _count: u32,
) -> u32 {
    0
}

/// The native bridge, loaded but not yet handed the VM.
struct BridgeSetup {
    handle: *mut c_void,
    /// Handle of the translator itself (`libndk_translation.so`), so its
    /// `NativeBridgeItf` vtable can be called directly.
    translator: *mut c_void,
    initialize: unsafe extern "C" fn(*mut c_void, *const c_char) -> bool,
    isa: String,
}

/// Steps 1 and 2 of the bridge state machine, run before the VM exists.
///
/// `app_process` does `LoadNativeBridge` and `PreInitializeNativeBridge` before
/// `JNI_CreateJavaVM`, and `InitializeNativeBridge` afterwards; the order is
/// load-bearing because each step asserts the previous one happened.
fn setup_native_bridge(libs: Option<&Path>, data_dir: &Path, isa: &str) -> Option<BridgeSetup> {
    let dir = libs.map(|d| d.display().to_string()).unwrap_or_default();
    let path = if dir.is_empty() {
        "libnativebridge.so".to_string()
    } else {
        format!("{dir}/libnativebridge.so")
    };
    let cpath = CString::new(path.as_str()).ok()?;
    let handle = unsafe { libc::dlopen(cpath.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
    if handle.is_null() {
        diag(&format!("art: dlopen {path} failed: {}", dlerror()));
        return None;
    }
    let load = unsafe { libc::dlsym(handle, c"LoadNativeBridge".as_ptr()) };
    let pre = unsafe { libc::dlsym(handle, c"PreInitializeNativeBridge".as_ptr()) };
    let init = unsafe { libc::dlsym(handle, c"InitializeNativeBridge".as_ptr()) };
    if load.is_null() || pre.is_null() || init.is_null() {
        diag("art: libnativebridge is missing part of the state machine");
        return None;
    }

    let name = std::env::var("PENGUDROID_PROP_ro.dalvik.vm.native.bridge")
        .unwrap_or_else(|_| "libndk_translation.so".to_string());
    let cname = CString::new(name.as_str()).ok()?;
    let callbacks = BridgeCallbacks {
        get_method_shorty: bridge_get_method_shorty,
        get_native_method_count: bridge_get_native_method_count,
        get_native_methods: bridge_get_native_methods,
    };
    type Load = unsafe extern "C" fn(*const c_char, *const BridgeCallbacks) -> bool;
    let f: Load = unsafe { std::mem::transmute(load) };
    let loaded = unsafe { f(cname.as_ptr(), &callbacks) };
    diag(&format!("art: LoadNativeBridge({name}) = {loaded}"));
    if !loaded {
        return None;
    }

    type PreInit = unsafe extern "C" fn(*const c_char, *const c_char) -> bool;
    let cdata = CString::new(data_dir.display().to_string()).ok()?;
    let cisa = CString::new(isa).ok()?;
    let f: PreInit = unsafe { std::mem::transmute(pre) };
    let pre_ok = unsafe { f(cdata.as_ptr(), cisa.as_ptr()) };
    diag(&format!(
        "art: PreInitializeNativeBridge({}, {isa}) = {pre_ok}",
        data_dir.display()
    ));
    if !pre_ok {
        return None;
    }

    // ART needs a 1 GiB region space immediately after this point. Report the
    // limits and probe the allocation, because a bridge that changed either is
    // otherwise very hard to see from ART's "Cannot allocate memory".
    let mut lim: libc::rlimit = unsafe { std::mem::zeroed() };
    unsafe { libc::getrlimit(libc::RLIMIT_AS, &mut lim) };
    diag(&format!(
        "art: RLIMIT_AS = {}/{} bytes",
        lim.rlim_cur, lim.rlim_max
    ));
    let probe_len = 1074003968usize;
    let probe = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            probe_len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if probe == libc::MAP_FAILED {
        diag(&format!(
            "art: {probe_len} byte probe mmap FAILED: {}",
            std::io::Error::last_os_error()
        ));
    } else {
        diag(&format!("art: {probe_len} byte probe mmap ok at {probe:p}"));
        unsafe { libc::munmap(probe, probe_len) };
    }
    // Load the translator a second time through our own handle: the Itf is a
    // table of function pointers we can drive without libnativebridge's state
    // machine (which would start a guest app_process).
    let translator = unsafe { libc::dlopen(cname.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
    if translator.is_null() {
        diag(&format!("art: dlopen({name}) failed: {}", dlerror()));
        return None;
    }

    Some(BridgeSetup {
        handle,
        translator,
        initialize: unsafe { std::mem::transmute(init) },
        isa: isa.to_string(),
    })
}

/// Alternative call shapes for the translator's initialiser.
///
/// This ndk_translation build declares `native_bridge_initialize(callbacks,
/// const char*, const char*)`. Which strings it wants, and whether one of them
/// selects "do not start a guest app_process", is not documented, so the
/// selftest can try the shapes out with `PENGUDROID_BRIDGE_MODE`.
fn bridge_init_mode() -> String {
    std::env::var("PENGUDROID_BRIDGE_MODE").unwrap_or_else(|_| "dir+isa".into())
}

/// Drive the translator's `NativeBridgeItf` table directly.
///
/// libnativebridge's state machine calls `Itf.initialize` *and* something that
/// starts `/system/bin/arm64/app_process64` on a thread. For running an ARM
/// library inside our own (x86_64) ART we only need the first half:
///
/// ```text
///   Itf.initialize(callbacks)                 set up the JIT + signal handlers
///   Itf.loadLibrary(path, RTLD_NOW)           map/parse the guest ELF
///   Itf.getTrampoline(handle, name, shorty)   host-callable entry point
/// ```
///
/// `spec` is `<lib path>#<symbol>`; the symbol is called with a JNI-static
/// shape `(JNIEnv*, jclass) -> jint` and its return value printed, so a guest
/// function that returns a known constant proves translation end to end.
fn run_bridge_selftest(setup: &BridgeSetup, env: &crate::jni::JniEnv, spec: &str) {
    let (lib, symbol) = match spec.split_once('#') {
        Some(v) => v,
        None => {
            eprintln!("art: --bridge-selftest expects <lib>#<symbol>");
            return;
        }
    };
    let itf = unsafe { libc::dlsym(setup.translator, c"NativeBridgeItf".as_ptr()) };
    if itf.is_null() {
        eprintln!("art: the translator does not export NativeBridgeItf");
        return;
    }
    // struct NativeBridgeCallbacks { uint32_t version; <function pointers...> }
    let version = unsafe { *(itf as *const u32) };
    let slot = |i: usize| -> *const c_void {
        // Slot 0 is the 32-bit version; the pointers start at offset 8.
        unsafe { *(itf as *const *const c_void).add(i) }
    };
    diag(&format!("art: NativeBridgeItf.version = {version}"));

    let callbacks = BridgeCallbacks {
        get_method_shorty: bridge_get_method_shorty,
        get_native_method_count: bridge_get_native_method_count,
        get_native_methods: bridge_get_native_methods,
    };

    let init_ptr = {
        // Offset 8 = slot 1. This ndk_translation build takes three arguments:
        // (callbacks, app_data_dir, instruction_set) — passing only the
        // callbacks leaves the other two as garbage, and it strcmp()s them.
        unsafe { *(itf as *const u8).add(8).cast::<*const c_void>() }
    };
    if !init_ptr.is_null() {
        type Init = unsafe extern "C" fn(*const BridgeCallbacks, *const c_char, *const c_char) -> bool;
        let f: Init = unsafe { std::mem::transmute(init_ptr) };
        let data_dir = std::env::var_os("PENGUDROID_BRIDGE_DATA")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("pengudroid-bridge"));
        let _ = std::fs::create_dir_all(&data_dir);
        let (Ok(cdata), Ok(cisa)) = (
            CString::new(data_dir.display().to_string()),
            CString::new("arm64"),
        ) else {
            return;
        };
        let mode = bridge_init_mode();
        let (p2, p3) = match mode.as_str() {
            "nulls" => (std::ptr::null(), std::ptr::null()),
            "dir" => (cdata.as_ptr(), std::ptr::null()),
            "isa+dir" => (cisa.as_ptr(), cdata.as_ptr()),
            _ => (cdata.as_ptr(), cisa.as_ptr()),
        };
        let ok = unsafe { f(&callbacks, p2, p3) };
        diag(&format!("art: Itf.initialize(mode={mode}) = {ok}"));
    }

    let load = slot(2);
    if load.is_null() {
        eprintln!("art: Itf.loadLibrary is null");
        return;
    }
    type LoadLib = unsafe extern "C" fn(*const c_char, c_int) -> *mut c_void;
    let f: LoadLib = unsafe { std::mem::transmute(load) };
    let Ok(clib) = CString::new(lib) else { return };
    let handle = unsafe { f(clib.as_ptr(), 2) }; // RTLD_NOW
    diag(&format!("art: Itf.loadLibrary({lib}) = {handle:p}"));
    if handle.is_null() {
        return;
    }

    let tramp = slot(3);
    if tramp.is_null() {
        eprintln!("art: Itf.getTrampoline is null");
        return;
    }
    type GetTramp = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> *mut c_void;
    let f: GetTramp = unsafe { std::mem::transmute(tramp) };
    let (Ok(csym), Ok(cshorty)) = (CString::new(symbol), CString::new("()I")) else {
        return;
    };
    let entry = unsafe { f(handle, csym.as_ptr(), cshorty.as_ptr()) };
    diag(&format!("art: Itf.getTrampoline({symbol}) = {entry:p}"));
    if entry.is_null() {
        return;
    }

    type StaticIntFn = unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32;
    let call: StaticIntFn = unsafe { std::mem::transmute(entry) };
    diag("art: calling the translated guest function");
    let value = unsafe { call(env.0, std::ptr::null_mut()) };
    diag(&format!("art: guest {symbol}() returned {value}"));
}

/// Re-install the translator's signal handlers over ART's.
///
/// Both runtimes live in this one process. The bridge installs its handlers in
/// `Itf.initialize` (during `LoadNativeBridge`, before the VM), then ART's
/// `FaultManager::Init` replaces them with its crash dumper while the VM comes
/// up. Guest code needs the bridge's handlers to resolve the lazy-translation
/// page faults, so they are put back once the VM exists.
fn install_bridge_handlers(handle: *mut c_void) {
    let get = unsafe { libc::dlsym(handle, c"NativeBridgeGetSignalHandler".as_ptr()) };
    if get.is_null() {
        diag("art: libnativebridge has no NativeBridgeGetSignalHandler");
        return;
    }
    type GetHandler = unsafe extern "C" fn(c_int) -> *const c_void;
    let f: GetHandler = unsafe { std::mem::transmute(get) };
    let install = unsafe {
        libc::dlsym(
            libc::RTLD_DEFAULT,
            c"__pengudroid_install_bridge_handler".as_ptr(),
        )
    };
    if install.is_null() {
        diag("art: shim does not export __pengudroid_install_bridge_handler");
        return;
    }
    type Install = unsafe extern "C" fn(c_int, *const c_void) -> c_int;
    let g: Install = unsafe { std::mem::transmute(install) };
    for sig in [4, 7, 8, 11] {
        let act = unsafe { f(sig) };
        if act.is_null() {
            diag(&format!("art: bridge has no handler for signal {sig}"));
            continue;
        }
        let rc = unsafe { g(sig, act) };
        diag(&format!("art: installed bridge handler for signal {sig} (rc={rc})"));
    }
}

/// Step 3: hand the live VM to the bridge.
fn finish_native_bridge(setup: &BridgeSetup, env: &crate::jni::JniEnv) {
    let Ok(cisa) = CString::new(setup.isa.as_str()) else {
        return;
    };
    let ok = unsafe { (setup.initialize)(env.0, cisa.as_ptr()) };
    diag(&format!("art: InitializeNativeBridge({}) = {ok}", setup.isa));

    // ART's FaultManager::Init() installed its own SIGSEGV/SIGBUS/SIGILL/SIGFPE
    // actions while the VM came up, silently replacing the translator's. Both
    // runtimes live in this one process, so the guest's lazy-translation faults
    // would land in ART's crash dumper. Re-install the bridge's handlers now
    // that the VM is up (on a device, sigchain does the equivalent).
    install_bridge_handlers(setup.handle);
    let available = unsafe { libc::dlsym(setup.handle, c"NativeBridgeAvailable".as_ptr()) };
    if !available.is_null() {
        let g: unsafe extern "C" fn() -> bool = unsafe { std::mem::transmute(available) };
        diag(&format!("art: NativeBridgeAvailable() = {}", unsafe { g() }));
    }
    let version = unsafe { libc::dlsym(setup.handle, c"NativeBridgeGetVersion".as_ptr()) };
    if !version.is_null() {
        let g: unsafe extern "C" fn() -> u32 = unsafe { std::mem::transmute(version) };
        diag(&format!("art: NativeBridgeGetVersion() = {}", unsafe { g() }));
    }
}

/// Append a diagnostic line to `$PENGUDROID_ART_DIAG` (and stderr).
///
/// ART's abort path can swallow buffered output, and a Rust panic inside a
/// JNI callback would too, so diagnostics are also appended to a file when the
/// caller asks for it.
fn diag(msg: &str) {
    eprintln!("{msg}");
    if let Some(path) = std::env::var_os("PENGUDROID_ART_DIAG") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{msg}");
        }
    }
}

/// Call every `register_*` entry point a framework library exports.
///
/// `libandroid_runtime.so` has no `JNI_OnLoad`: `app_process` links the
/// `register_android_*` functions into `gRegJNI` and calls them one by one
/// (`AndroidRuntime::startReg`). We do the same thing generically, by walking
/// the library's exported symbols instead of hard-coding the list.
fn register_framework_natives(env: &crate::jni::JniEnv, path: &str, libs: Option<&Path>) -> i32 {
    diag(&format!("art: register_natives({path}) libs={libs:?}"));
    // Resolve a bare soname against the prepared library directory, which is
    // where the de-bionic'd copies live.
    let real = if Path::new(path).exists() {
        path.to_string()
    } else if let Some(dir) = libs {
        dir.join(path).to_string_lossy().into_owned()
    } else {
        path.to_string()
    };
    let Ok(cpath) = CString::new(real.as_str()) else {
        return 1;
    };
    let handle = unsafe { libc::dlopen(cpath.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
    if handle.is_null() {
        eprintln!("art: dlopen {real} failed: {}", dlerror());
        return 1;
    }
    diag(&format!("art: dlopen({real}) ok"));
    let Ok(exports) = pengudroid_elf::read_file(&real) else {
        eprintln!("art: cannot read the symbol table of {real}");
        return 1;
    };
    diag(&format!(
        "art: {} exports, {} imports",
        exports.exports.len(),
        exports.imports.len()
    ));
    let names: Vec<String> = exports
        .exports
        .into_iter()
        .filter(|n| n.contains("register_") && !n.contains("RegisterNatives"))
        .collect();
    if names.is_empty() {
        eprintln!("art: {real} exports no register_* entry points");
        return 1;
    }
    // Order matters: ART's `gRegJNI` registers the logging natives first, and
    // `register_android_os_Binder` last (it triggers `StrictMode.<clinit>`, whose
    // natives must already exist). Symbol-table order is arbitrary, so impose
    // the same shape here: a small head, then everything else, then Binder.
    let head = ["register_android_util_Log", "register_android_util_EventLog"];
    let mut ordered: Vec<String> = Vec::with_capacity(names.len());
    for h in head {
        if let Some(i) = names.iter().position(|n| n.contains(h)) {
            ordered.push(names[i].clone());
        }
    }
    let tail = ["register_android_os_Binder"];
    for n in &names {
        let is_head = head.iter().any(|h| n.contains(h));
        let is_tail = tail.iter().any(|t| n.contains(t));
        if !is_head && !is_tail {
            ordered.push(n.clone());
        }
    }
    for t in tail {
        if let Some(i) = names.iter().position(|n| n.contains(t)) {
            ordered.push(names[i].clone());
        }
    }
    let names = ordered;
    type Register = unsafe extern "C" fn(*mut c_void) -> c_int;
    let mut ok = 0usize;
    let mut failed = Vec::new();
    let mut missing = 0usize;
    for name in &names {
        let Ok(cname) = CString::new(name.as_str()) else {
            continue;
        };
        let sym = unsafe { libc::dlsym(handle, cname.as_ptr()) };
        if sym.is_null() {
            missing += 1;
            continue;
        }
        let f: Register = unsafe { std::mem::transmute(sym) };
        let rc = unsafe { f(env.0) };
        if rc == 0 {
            ok += 1;
        } else {
            failed.push((name.clone(), rc));
        }
    }
    eprintln!(
        "art: registered {ok} of {} native groups from {real} ({missing} dlsym misses)",
        names.len()
    );
    let _ = std::io::Write::flush(&mut std::io::stdout());
    diag(&format!("art: registered {ok}/{} ({missing} missing)", names.len()));
    for (name, rc) in failed.iter().take(20) {
        diag(&format!("art:   {name} returned {rc}"));
    }
    0
}

/// dlopen a JNI library and call its `JNI_OnLoad`.
///
/// Android's framework natives live in `libandroid_runtime.so`, which registers
/// them from `JNI_OnLoad`. Without this call every `android.*` native method
/// throws `UnsatisfiedLinkError`.
fn load_jni_library(env: &crate::jni::JniEnv, vm: *mut c_void, path: &str) -> i32 {
    let Ok(cpath) = CString::new(path) else {
        return 1;
    };
    let handle = unsafe { libc::dlopen(cpath.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
    if handle.is_null() {
        eprintln!("art: dlopen {path} failed: {}", dlerror());
        return 1;
    }
    println!("art: loaded JNI library {path}");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let on_load = unsafe { libc::dlsym(handle, c"JNI_OnLoad".as_ptr()) };
    if on_load.is_null() {
        println!("art: {path} has no JNI_OnLoad (nothing to register)");
        return 0;
    }
    type JniOnLoad = unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int;
    let f: JniOnLoad = unsafe { std::mem::transmute(on_load) };
    let rc = unsafe { f(vm, std::ptr::null_mut()) };
    println!("art: {path} JNI_OnLoad returned 0x{rc:08x}");
    if rc < 0 {
        eprintln!("art: {path} rejected the VM");
        return 1;
    }
    let _ = env;
    0
}

/// Dump the JNI function table with symbol names (`PENGUDROID_DUMP_JNI=1`).
///
/// The table layout is fixed by the JNI specification but is easy to get wrong
/// by hand, so this lets us read the real slot numbers out of a running VM.
fn dump_jni_table(env: &crate::jni::JniEnv) {
    let table = unsafe { *(env.0 as *const *const *const c_void) };
    if table.is_null() {
        eprintln!("art: JNI table is null");
        return;
    }
    for i in 0..240 {
        let p = unsafe { *table.add(i) };
        if p.is_null() {
            println!("jni[{i:>3}] = null");
            continue;
        }
        let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
        let name = unsafe {
            if libc::dladdr(p, &mut info) != 0 && !info.dli_sname.is_null() {
                std::ffi::CStr::from_ptr(info.dli_sname)
                    .to_string_lossy()
                    .into_owned()
            } else {
                String::from("??")
            }
        };
        let base = if unsafe { libc::dladdr(p, &mut info) } != 0 {
            info.dli_fbase as usize
        } else {
            0
        };
        println!(
            "jni[{i:>3}] = {p:p} base={base:#x} off={:#x} {name}",
            (p as usize).wrapping_sub(base)
        );
    }
}

/// Slot of `FindClass` in the JNI native interface table.
///
/// Layout (JNI spec): four reserved pointers, then `GetVersion`, `DefineClass`,
/// `FindClass` — so `FindClass` is index 6.
const JNI_FINDCLASS_INDEX: usize = 6;

type FindClassFn = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void;

/// The real `FindClass` captured before the table slot is patched.
static REAL_FIND_CLASS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Wrapper that reports every `FindClass` call and its outcome.
///
/// Writes directly with `write(2)`: logging here would itself call back into
/// the JVM and recurse through `FindClass`.
unsafe extern "C" fn traced_find_class(env: *mut c_void, name: *const c_char) -> *mut c_void {
    use std::sync::atomic::Ordering;
    let addr = REAL_FIND_CLASS.load(Ordering::Relaxed);
    if addr == 0 {
        return std::ptr::null_mut();
    }
    let real: FindClassFn = std::mem::transmute(addr);
    let result = real(env, name);
    let mut line: Vec<u8> = Vec::with_capacity(160);
    line.extend_from_slice(if result.is_null() {
        b"[findclass MISS] ".as_slice()
    } else {
        b"[findclass ok] ".as_slice()
    });
    if !name.is_null() {
        line.extend_from_slice(std::ffi::CStr::from_ptr(name).to_bytes());
    }
    line.push(b'\n');
    libc::write(2, line.as_ptr() as *const c_void, line.len());
    result
}

/// Replace the `FindClass` slot of the process-wide JNI table.
///
/// Every `JNIEnv` points at this one table, so patching it instruments calls
/// made by native libraries as well as by this harness. Used to name the class
/// a failing `ActivityThread.main` cannot resolve.
unsafe fn install_findclass_tracer(env: *mut c_void) {
    use std::sync::atomic::Ordering;
    if env.is_null() {
        return;
    }
    let table = *(env as *const *mut *mut c_void);
    if table.is_null() {
        return;
    }
    let slot = table.add(JNI_FINDCLASS_INDEX);
    REAL_FIND_CLASS.store(*slot as usize, Ordering::Relaxed);
    let page = (slot as usize) & !0xfff;
    if libc::mprotect(page as *mut c_void, 0x1000, libc::PROT_READ | libc::PROT_WRITE) != 0 {
        eprintln!("art: cannot make the JNI table writable");
        return;
    }
    *slot = traced_find_class as *mut c_void;
    let _ = libc::mprotect(page as *mut c_void, 0x1000, libc::PROT_READ);
    println!("art: tracing JNI FindClass (PENGUDROID_TRACE_FINDCLASS)");
}

/// Bind an APK into the `ActivityThread` that `main` creates.
///
/// Runs on the calling thread (the one that created the VM). `ActivityThread`
/// only exists once `main` has started on the helper thread, so this polls for
/// the instance before binding.
fn bind_application_now(env: &crate::jni::JniEnv, classpath: &[String]) {
    let Some(apk) = classpath.first().cloned() else {
        eprintln!("apphost: --bind-app needs --classpath <apk>");
        return;
    };
    let mut thread = std::ptr::null_mut();
    for _ in 0..300 {
        if let Some(t) = crate::apphost::current_activity_thread(env) {
            if !t.is_null() {
                thread = t;
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if thread.is_null() {
        eprintln!("apphost: ActivityThread.currentActivityThread() is null");
        return;
    }
    let info = match crate::apk::read_apk(&apk) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("apphost: {e}");
            return;
        }
    };
    let package = if info.package.is_empty() {
        "org.pengudroid.app".to_string()
    } else {
        info.package.clone()
    };
    let apk_path = std::path::PathBuf::from(&apk);
    let data_dir = std::env::temp_dir().join(format!("pengudroid-data/{package}"));
    let lib_dir = std::env::temp_dir().join(format!("pengudroid-libs/{package}"));
    // `handleBindApplication` creates `code_cache` under the data directory and
    // logs "Data directory doesn't exist" (plus a stack trace) when the parent
    // is missing, so lay the tree out the way the framework expects.
    for dir in [
        data_dir.clone(),
        data_dir.join("device_protected"),
        data_dir.join("device_protected/code_cache"),
        data_dir.join("code_cache"),
        lib_dir.clone(),
    ] {
        let _ = std::fs::create_dir_all(&dir);
    }
    // `StatFs("/data")` is how apps ask for the free space on the data
    // partition; the shim maps `/data` into the sysroot, so the directory has to
    // exist or `statvfs` fails with ENOENT and the app sees "Invalid path".
    if let Some(root) = std::env::var_os("PENGUDROID_ROOT") {
        let _ = std::fs::create_dir_all(std::path::Path::new(&root).join("data"));
    }
    // `System.loadLibrary` resolves through `ApplicationInfo.nativeLibraryDir`,
    // so the APK's own `lib/<abi>/*.so` have to exist on disk there. Without
    // this every `native*` method is an UnsatisfiedLinkError (Roblox's
    // ActivitySplash dies on its first one).
    let mut extracted = 0usize;
    for apk in classpath {
        let Ok(libs) = crate::apk::native_libs(apk) else { continue };
        for lib in libs.iter().filter(|l| l.abi == "x86_64") {
            let dest = lib_dir.join(&lib.file);
            if dest.exists() {
                continue;
            }
            if let Ok(bytes) = crate::apk::extract_named(apk, &lib.entry) {
                if std::fs::write(&dest, bytes).is_ok() {
                    extracted += 1;
                }
            }
        }
    }
    if extracted > 0 {
        println!("apphost: extracted {extracted} x86_64 native libraries");
    }
    let spec = crate::apphost::AppSpec {
        package: &package,
        apk: &apk_path,
        data_dir: &data_dir,
        native_lib_dir: &lib_dir,
        target_sdk: info.target_sdk.map(|v| v as i32).unwrap_or(35),
        min_sdk: info.min_sdk.map(|v| v as i32).unwrap_or(23),
        app_class: info.application_class.as_deref(),
        providers: &info.providers,
        // `ActivityInfo.theme` decides whether AppCompat accepts the activity:
        // with no theme the framework falls back to the platform default and
        // `setContentView` throws "You need to use a Theme.AppCompat theme".
        // The activity's own `android:theme` wins over the application's.
        theme: info
            .activities
            .iter()
            .find(|a| {
                Some(a.name.as_str())
                    == std::env::var("PENGUDROID_LAUNCH_ACTIVITY").ok().as_deref()
            })
            .and_then(|a| a.theme)
            .or(info.application_theme),
        // Must belong to user 0: `ActivityThread.getPackageInfo` skips its
        // `LoadedApk` cache when `UserHandle.getUserId(uid) != myUserId()`, and
        // a cache miss rebuilds the package info with `mIncludeCode == false`
        // (empty dex path, so the APK's classes are unreachable).
        uid: 10_000,
    };
    crate::apphost::bind_application(env, thread, &spec);
}

/// Start libbinder's thread pool in this process.
///
/// A real app process inherits its binder threads from zygote: the pool is
/// started before the fork, so `system_server`'s callbacks (`bindApplication`,
/// `scheduleLaunchActivity`) have a thread to arrive on. We do not fork from a
/// zygote, so nothing is reading the binder device and an app would sit in
/// `Looper.loop()` forever.
///
/// KNOWN BROKEN (hence `PENGUDROID_BINDER_POOL`): calling `ProcessState::self()`
/// from here SIGSEGVs inside `ProcessState::init` — the fault is a store into
/// libbinder's own text mapping (`r15` points into the code segment), which
/// suggests the object layout/C++ runtime state init expects is not what a
/// glibc-hosted call provides. The app's *own* binder use works, so the fix has
/// to reach the pool from the same context that creates `ProcessState` (a JNI
/// entry point) rather than from the host side.
fn start_binder_pool(libs: Option<&Path>) {
    // Open by *soname* on purpose: glibc keys loaded objects by the name they
    // were resolved with, so dlopening an absolute path here can map a second
    // copy of libbinder.so. That copy would have its own ProcessState singleton,
    // and initialising it re-enters `ProcessState::init` on a half-built object
    // (observed as a SIGSEGV inside init).
    let _ = libs;
    let handle = unsafe { libc::dlopen(c"libbinder.so".as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
    if handle.is_null() {
        diag(&format!("art: dlopen(libbinder.so) failed: {}", dlerror()));
        return;
    }
    diag(&format!("art: libbinder.so handle {handle:p}"));
    let self_fn = unsafe { libc::dlsym(handle, c"_ZN7android12ProcessState4selfEv".as_ptr()) };
    let start_fn = unsafe {
        libc::dlsym(handle, c"_ZN7android12ProcessState15startThreadPoolEv".as_ptr())
    };
    if self_fn.is_null() || start_fn.is_null() {
        diag("art: libbinder has no ProcessState::self/startThreadPool");
        return;
    }
    type SelfFn = unsafe extern "C" fn() -> *mut c_void;
    type StartFn = unsafe extern "C" fn(*mut c_void);
    let get_self: SelfFn = unsafe { std::mem::transmute(self_fn) };
    let start: StartFn = unsafe { std::mem::transmute(start_fn) };
    let state = unsafe { get_self() };
    if state.is_null() {
        diag("art: ProcessState::self() returned null");
        return;
    }
    unsafe { start(state) };
    diag(&format!("art: binder thread pool started (ProcessState {state:p})"));
}

/// Run `<class>.main(String[])` on a live VM.
///
/// The class name may be written in either slash or dot form; JNI wants the
/// slash form with an `L...;` wrapper.
/// Call `android.os.ServiceManager.getService(name)` and report the result.
///
/// Verifies that the binder bridge answered `checkService` with a real handle,
/// independently of whatever else `ActivityThread.main` does.
fn probe_service(env: &crate::jni::JniEnv, name: &str) {
    // Handle 0 is the context object; if this returns a proxy, the
    // BinderProxy creation path itself works and a null from getService is
    // specific to the handle we returned.
    if let Some(klass) = env.find_class("com/android/internal/os/BinderInternal") {
        match env.static_method_id(klass, "getContextObject", "()Landroid/os/IBinder;") {
            Some(m) => match env.call_static_object(klass, m, &[]) {
                Some(o) if o.is_null() => println!("art: getContextObject() = null"),
                Some(_) => println!("art: getContextObject() = non-null"),
                None => {
                    println!("art: getContextObject() threw");
                    env.describe_exception();
                }
            },
            None => println!("art: BinderInternal.getContextObject not found"),
        }
    } else {
        println!("art: BinderInternal not found");
    }
    let Some(klass) = env.find_class("android/os/ServiceManager") else {
        eprintln!("art: ServiceManager not found");
        return;
    };
    let Some(method) = env.static_method_id(
        klass,
        "getService",
        "(Ljava/lang/String;)Landroid/os/IBinder;",
    ) else {
        eprintln!("art: ServiceManager.getService not found");
        return;
    };
    let s = env.new_string(name);
    let args = [crate::jni::JValue { l: s }];
    match env.call_static_object(klass, method, &args) {
        Some(obj) if obj.is_null() => println!("art: getService({name}) = null"),
        Some(_) => println!("art: getService({name}) = non-null"),
        None => {
            println!("art: getService({name}) threw");
            env.describe_exception();
        }
    }
}

fn run_main(env: &crate::jni::JniEnv, class: &str, args: &[String]) -> i32 {
    let descriptor = format!("L{};", class.replace('.', "/"));
    let Some(klass) = env
        .load_class(&descriptor)
        .or_else(|| env.find_class(&descriptor))
    else {
        eprintln!("art: class not found: {class}");
        return 1;
    };
    let Some(main) = env.static_method_id(klass, "main", "([Ljava/lang/String;)V") else {
        eprintln!("art: {class} has no main([Ljava/lang/String;)V");
        return 1;
    };
    let Some(array) = env.string_array(args) else {
        eprintln!("art: cannot build the argument array");
        return 1;
    };
    let argv = [crate::jni::JValue { l: array }];
    println!("art: invoking {class}.main with {} argument(s)", args.len());
    let _ = std::io::Write::flush(&mut std::io::stdout());
    if !env.call_static_void(klass, main, &argv) {
        eprintln!("art: {class}.main threw");
        return 1;
    }
    println!("art: {class}.main returned");
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_flat_and_android_layouts() {
        let dir = std::env::temp_dir().join(format!("pd-layout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("lib64")).unwrap();
        std::fs::write(dir.join("lib64/libart.so"), b"x").unwrap();
        let flat = Layout::detect(&dir).unwrap();
        assert_eq!(flat.lib_dirs.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);

        let art = dir.join("system/apex/com.android.art/lib64");
        std::fs::create_dir_all(&art).unwrap();
        std::fs::write(art.join("libart.so"), b"x").unwrap();
        let android = Layout::detect(&dir).unwrap();
        // system/lib64 and the ART APEX are always on the path; every other
        // APEX that ships a lib64 is appended as it appears.
        assert_eq!(android.lib_dirs.len(), 2);
        for apex in ["com.android.runtime", "com.android.i18n"] {
            std::fs::create_dir_all(dir.join("system/apex").join(apex).join("lib64")).unwrap();
        }
        let android = Layout::detect(&dir).unwrap();
        assert_eq!(android.lib_dirs.len(), 4);
        assert!(android.android_root.ends_with("system"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_a_directory_without_art() {
        let dir = std::env::temp_dir().join(format!("pd-layout-empty-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        assert!(Layout::detect(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Serialize one `BootClasspath { repeated Jar paths = 1 }` entry.
    fn pb_entry(path: &str) -> Vec<u8> {
        let mut inner = vec![0x0a];
        inner.push(path.len() as u8);
        inner.extend_from_slice(path.as_bytes());
        let mut out = vec![0x0a];
        out.push(inner.len() as u8);
        out.extend_from_slice(&inner);
        out
    }

    #[test]
    fn parses_a_bootclasspath_fragment() {
        let mut data = Vec::new();
        data.extend_from_slice(&pb_entry("/apex/com.android.tethering/javalib/framework-connectivity.jar"));
        data.extend_from_slice(&pb_entry("/apex/com.android.tethering/javalib/framework-tethering.jar"));
        assert_eq!(
            jar_paths_in_bootclasspath_pb(&data),
            vec![
                "/apex/com.android.tethering/javalib/framework-connectivity.jar",
                "/apex/com.android.tethering/javalib/framework-tethering.jar",
            ]
        );
    }

    #[test]
    fn collects_fragments_from_every_apex() {
        let dir = std::env::temp_dir().join(format!("pd-apex-pb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (apex, jar) in [
            ("com.android.tethering", "framework-connectivity.jar"),
            ("com.android.conscrypt", "conscrypt.jar"),
        ] {
            let etc = dir.join("apex").join(apex).join("etc/classpaths");
            std::fs::create_dir_all(&etc).unwrap();
            let path = format!("/apex/{apex}/javalib/{jar}");
            std::fs::write(etc.join("bootclasspath.pb"), pb_entry(&path)).unwrap();
        }
        let found = apex_bootclasspath_fragments(&dir);
        assert_eq!(
            found,
            vec![
                "/apex/com.android.conscrypt/javalib/conscrypt.jar",
                "/apex/com.android.tethering/javalib/framework-connectivity.jar",
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
