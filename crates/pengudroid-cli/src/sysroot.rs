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
//! Only an extracted Android 17 / API 37 x86_64 root is supported:
//! `system/lib64`, `system/apex/com.android.art/lib64`, and
//! `system/framework/x86_64/boot.art`. The runtime's own `system/build.prop`
//! and ART ELF header must identify this target; legacy flat layouts are not
//! accepted. Framework boot jars come from `system/etc/classpaths/bootclasspath.pb`.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::deps::find_shim;
use crate::nativelib::ensure_compat_dir;
use pengudroid_nativebridge::{RuntimeCallbacks, Shorty, Translator};

/// Validate the runtime itself, independently of APK minSdk/targetSdk metadata.
fn validate_runtime(system: &Path) -> Result<(), String> {
    let props_path = system.join("build.prop");
    let props = std::fs::read_to_string(&props_path).map_err(|e| format!(
        "Android 17 / API 37 x86_64 requires {}: {e}", props_path.display()
    ))?;
    let sdk: Vec<&str> = props.lines().filter_map(|line| {
        let (key, value) = line.split_once('=')?;
        (key.trim() == "ro.build.version.sdk").then_some(value.trim())
    }).collect();
    if sdk.len() != 1 || sdk[0] != "37" {
        return Err(format!(
            "unsupported runtime in {}: expected Android 17 / API 37 x86_64 \
             (ro.build.version.sdk=37), found {}",
            props_path.display(),
            if sdk.is_empty() { "no SDK property".into() } else { sdk.join(", ") }
        ));
    }

    // Only identity is needed here, not section headers or dynamic symbols.
    // A lib64 directory (or a CPU property) does not prove the ELF is x86_64.
    let art = system.join("apex/com.android.art/lib64/libart.so");
    let mut header = [0u8; 20];
    std::fs::File::open(&art).and_then(|mut file| file.read_exact(&mut header))
        .map_err(|e| format!(
            "Android 17 / API 37 x86_64 requires an ART ELF header at {}: {e}",
            art.display()
        ))?;
    if &header[..4] != b"\x7fELF" {
        return Err(format!("{} is not an ELF file; Android 17 / API 37 x86_64 is required", art.display()));
    }
    let machine = u16::from_le_bytes([header[18], header[19]]);
    if header[4] != 2 || header[5] != 1 || machine != 0x3e {
        return Err(format!(
            "unsupported ART runtime {}: Android 17 / API 37 requires a 64-bit \
             little-endian x86_64 ELF (class=2, data=1, machine=62); \
             found class={}, data={}, machine={} ({})",
            art.display(), header[4], header[5], machine, pengudroid_elf::machine_name(machine)
        ));
    }
    Ok(())
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

/// Collect fragments from APEX modules that extend the boot classpath.
/// Sorted by APEX name so the order is stable across runs. An APEX need not
/// publish a fragment, but a published fragment must be readable and valid.
fn apex_bootclasspath_fragments(system: &Path) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let apex = system.join("apex");
    let entries = std::fs::read_dir(&apex)
        .map_err(|e| format!("cannot read {}: {e}", apex.display()))?;
    let mut dirs = entries.map(|entry| entry.map(|e| e.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("cannot enumerate {}: {e}", apex.display()))?;
    // Extracted trees may retain the packed .apex files alongside directories.
    dirs.retain(|dir| dir.is_dir());
    dirs.sort();
    for dir in dirs {
        let pb = dir.join("etc/classpaths/bootclasspath.pb");
        let data = match std::fs::read(&pb) {
            Ok(data) => data,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("cannot read {}: {e}", pb.display())),
        };
        let paths = jar_paths_in_bootclasspath_pb(&data)
            .map_err(|e| format!("invalid boot classpath fragment {}: {e}", pb.display()))?;
        for path in paths {
            if !out.contains(&path) {
                out.push(path);
            }
        }
    }
    Ok(out)
}

fn read_varint(data: &mut &[u8]) -> Result<u64, String> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let (&byte, rest) = data.split_first().ok_or("truncated protobuf varint")?;
        *data = rest;
        if shift == 63 && byte > 1 {
            return Err("overflowing protobuf varint".into());
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Ok(value);
        }
    }
    Err("overflowing protobuf varint".into())
}

/// Skip unneeded fields without guessing their meaning. Length-delimited
/// values are bounded by the enclosing message, never the whole fragment.
fn read_protobuf_field<'a>(data: &mut &'a [u8]) -> Result<(u64, Option<&'a [u8]>), String> {
    let tag = read_varint(data)?;
    let field = tag >> 3;
    if field == 0 || field > 0x1fff_ffff {
        return Err("invalid protobuf field number".into());
    }
    let wire = tag & 7;
    let len = match wire {
        0 => { read_varint(data)?; return Ok((field, None)); }
        1 => 8,
        2 => usize::try_from(read_varint(data)?).map_err(|_| "protobuf length overflow")?,
        5 => 4,
        _ => return Err(format!("unsupported protobuf wire type {wire}")),
    };
    let value = data.get(..len).ok_or("truncated protobuf field")?;
    *data = &data[len..];
    Ok((field, (wire == 2).then_some(value)))
}

/// Extract the jar paths from the image's boot classpath fragment.
///
/// The fields used here, verified against the image's fragments, are:
///
/// ```text
/// BootClasspath { repeated Jar paths = 1; }
/// Jar           { string path = 1; }
/// ```
///
/// Other fields are skipped, not interpreted. Malformed data must not yield a
/// partial classpath. A small parser avoids adding a protobuf dependency.
fn jar_paths_in_bootclasspath_pb(mut data: &[u8]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    while !data.is_empty() {
        let (field, value) = read_protobuf_field(&mut data)?;
        if field != 1 {
            continue;
        }
        let mut jar = value.ok_or("jar must be length-delimited")?;
        let mut path = None;
        while !jar.is_empty() {
            let (field, value) = read_protobuf_field(&mut jar)?;
            if field == 1 {
                if path.is_some() {
                    return Err("jar has duplicate paths".into());
                }
                let value = value.ok_or("jar path must be length-delimited")?;
                let value = std::str::from_utf8(value).map_err(|_| "jar path is not UTF-8")?;
                if value.is_empty() {
                    return Err("jar path is empty".into());
                }
                path = Some(value.to_string());
            }
        }
        out.push(path.ok_or("jar is missing its path")?);
    }
    Ok(out)
}

fn boot_jar_path(system: &Path, location: &str) -> Result<PathBuf, String> {
    let relative = location.strip_prefix("/system/")
        .or_else(|| location.strip_prefix('/').filter(|p| p.starts_with("apex/")))
        .ok_or_else(|| format!("unsupported boot classpath location: {location:?}"))?;
    if location.contains(':') || location.contains('\0') || Path::new(relative).components()
        .any(|part| !matches!(part, std::path::Component::Normal(_))) {
        return Err(format!("invalid boot classpath location: {location:?}"));
    }
    let path = system.join(relative);
    if !path.is_file() {
        return Err(format!("required boot classpath jar {location} is missing: {}", path.display()));
    }
    Ok(path)
}

impl Layout {
    /// Detect the layout under `root`.
    pub fn detect(root: &Path) -> Result<Layout, String> {
        // Everything downstream builds absolute guest paths from `root`
        // (`ANDROID_ROOT + "/etc/fonts.xml"`, boot class path locations, ...),
        // so a relative `--sysroot` would make them cwd-dependent or plain wrong.
        let root = std::fs::canonicalize(root)
            .map_err(|e| format!("cannot resolve sysroot {}: {e}", root.display()))?;
        if !root.join("system").is_dir() {
            return Err(format!(
                "unsupported sysroot {}: expected an extracted Android 17 / API 37 x86_64 \
                 root with system/build.prop and system/apex/com.android.art/lib64/libart.so; \
                 legacy flat sysroots are not supported",
                root.display()
            ));
        }
        Self::android(&root)
    }

    fn android(root: &Path) -> Result<Layout, String> {
        let system = root.join("system");
        validate_runtime(&system)?;
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

        // Preserve the ART/ICU prefix used to build the boot image. Framework
        // jars, including Android 17's split jars, must come from its fragment.
        let mut boot_locations: Vec<String> = [
            "/apex/com.android.art/javalib/core-oj.jar",
            "/apex/com.android.art/javalib/core-libart.jar",
            "/apex/com.android.art/javalib/okhttp.jar",
            "/apex/com.android.art/javalib/bouncycastle.jar",
            "/apex/com.android.art/javalib/apache-xml.jar",
            "/apex/com.android.i18n/javalib/core-icu4j.jar",
        ].into_iter().map(String::from).collect();
        let fragment = system.join("etc/classpaths/bootclasspath.pb");
        let data = std::fs::read(&fragment)
            .map_err(|e| format!("cannot read required boot classpath fragment {}: {e}", fragment.display()))?;
        let configured = jar_paths_in_bootclasspath_pb(&data)
            .map_err(|e| format!("invalid boot classpath fragment {}: {e}", fragment.display()))?;
        let framework_locations: Vec<String> = configured.into_iter()
            .filter(|p| p.starts_with("/system/framework/")).collect();
        if framework_locations.is_empty() {
            return Err(format!(
                "required boot classpath fragment {} contains no /system/framework/ jars",
                fragment.display()
            ));
        }
        for location in framework_locations {
            if !boot_locations.contains(&location) {
                boot_locations.push(location);
            }
        }

        // Module fragments extend the boot classpath beyond the image's jars.
        for location in apex_bootclasspath_fragments(&system)? {
            if !location.starts_with("/apex/") {
                return Err(format!("unsupported APEX boot classpath location: {location:?}"));
            }
            if !boot_locations.contains(&location) {
                boot_locations.push(location);
            }
        }
        let boot_jars = boot_locations.iter().map(|location| boot_jar_path(&system, location))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Layout {
            root: root.to_path_buf(),
            lib_dirs,
            art_root,
            framework: framework.clone(),
            boot_images: framework.join("x86_64"),
            boot_jars,
            boot_locations,
            apex_manifest: system.join("apex/com.android.art/apex_manifest.pb"),
            android_root: system,
        })
    }

    /// Files ART needs, with a description.
    pub fn required(&self) -> Vec<(String, PathBuf)> {
        vec![
            ("ART runtime".into(), self.art_root.join("lib64/libart.so")),
            ("ART base".into(), self.art_root.join("lib64/libartbase.so")),
            ("DEX support".into(), self.art_root.join("lib64/libdexfile.so")),
            ("JNI helpers".into(), self.art_root.join("lib64/libnativehelper.so")),
            ("Bionic libc".into(), self.lib_dirs[0].join("libc.so")),
            ("boot image".into(), self.boot_images.join("boot.art")),
            ("boot image (oat)".into(), self.boot_images.join("boot.oat")),
            ("framework classes".into(), self.framework.join("framework.jar")),
            ("ART APEX manifest".into(), self.apex_manifest.clone()),
            ("boot classpath fragment".into(), self.android_root.join("etc/classpaths/bootclasspath.pb")),
        ]
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
    println!("layout:   android (Android 17 / API 37 x86_64)");
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
/// Bionic sonames the shim stands in for, so the real library is never copied
/// into the scratch tree.
const SHIM_SONAMES: &[&str] = &["libc.so", "libm.so", "libdl.so", "liblog.so", "libdl_android.so"];

/// Stage a sysroot's library closure into a scratch directory and return it.
///
/// `nativelib --sysroot` uses this so a library whose dependencies are Android
/// `.so`s (packed relocations, Android symbol versions) can be `dlopen`ed.
pub(crate) fn stage_libraries(root: &Path) -> Result<PathBuf, String> {
    let layout = Layout::detect(root)?;
    let work = std::env::temp_dir().join(format!("pengudroid-stage-{}", std::process::id()));
    std::fs::create_dir_all(&work).map_err(|e| format!("cannot create {}: {e}", work.display()))?;
    prepare_art_libs(&layout, &work)
}

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

const DEFAULT_NATIVE_BRIDGE: &str = "libberberis_arm64.so";
const GUEST_STAGING_MANIFEST: &str = "pengudroid-arm64-guest.json";
const GUEST_LINKER_HOOKS: &[&str] = &[
    "__native_bridge_config_static_tls", "__native_bridge_get_host_pthread",
];
const GUEST_VDSO_HOOKS: &[&str] = &[
    "native_bridge_trace", "native_bridge_intercept_symbol", "native_bridge_post_init",
    "native_bridge_call_guest",
];

/// Use a distinct native-bridge runtime without replacing host ART libraries.
/// The shim uses this same root for only `/system/{bin,lib64}/arm64` and the
/// guest linker configuration. No missing-file fallback to a stock GSI exists.
fn bridge_guest_root(host: &Path, configured: Option<&std::ffi::OsStr>) -> Result<PathBuf, String> {
    let Some(configured) = configured else { return Ok(host.to_path_buf()); };
    let path = Path::new(configured);
    if !path.is_absolute() || path == Path::new("/") {
        return Err("PENGUDROID_ARM64_GUEST_ROOT must name an absolute native-bridge bundle directory, not /".into());
    }
    let path = std::fs::canonicalize(path)
        .map_err(|error| format!("cannot resolve ARM64 guest root {}: {error}", path.display()))?;
    if !path.is_dir() { return Err(format!("ARM64 guest root {} is not a directory", path.display())); }
    Ok(path)
}

/// Tool schema 1 contains stock guest binaries; schema 2 is explicitly staged-only.
/// Neither schema defines an executable/compatible state. Refuse any such marker
/// (including malformed ones), rather than inventing a JSON-compatible status.
fn validate_bridge_guest(root: &Path) -> Result<(), String> {
    let manifest = root.join(GUEST_STAGING_MANIFEST);
    match std::fs::symlink_metadata(&manifest) {
        Ok(_) => return Err(format!(
            "ARM64 guest {} is staged-only (including legacy schema-1 stock staging): \
             stock GSI linker64/libc are not native-bridge variants. \
             Staging manifests do not authorize bridge execution; supply a separate \
             native-bridge guest bundle, not an edited staging manifest", manifest.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("cannot inspect ARM64 guest manifest {}: {error}", manifest.display())),
    }
    // These are necessary source-defined contracts, not proof that a bundle can
    // boot. Manifest-free, manually supplied native-bridge bundles remain usable.
    guest_elf_contract(&root.join("system/bin/arm64/linker64"), GUEST_LINKER_HOOKS, &[])?;
    guest_elf_contract(&root.join("system/lib64/arm64/libc.so"), &["__libc_init", "pthread_self"],
                       &["native_bridge_intercept_symbol"])?;
    guest_elf_contract(&root.join("system/lib64/arm64/libnative_bridge_vdso.so"), GUEST_VDSO_HOOKS, &[])?;
    guest_elf_contract(&root.join("system/bin/arm64/app_process64"), &[], &[])?;
    Ok(())
}

fn guest_elf_contract(path: &Path, exports: &[&str], imports: &[&str]) -> Result<(), String> {
    const MAX_GUEST_ELF: u64 = 64 * 1024 * 1024;
    let file = std::fs::File::open(path)
        .map_err(|error| format!("ARM64 native-bridge guest requires {}: {error}", path.display()))?;
    let mut data = Vec::new();
    file.take(MAX_GUEST_ELF + 1).read_to_end(&mut data)
        .map_err(|error| format!("cannot read ARM64 guest {}: {error}", path.display()))?;
    if data.len() as u64 > MAX_GUEST_ELF {
        return Err(format!("ARM64 guest ELF {} exceeds the 64 MiB inspection limit", path.display()));
    }
    let elf = pengudroid_elf::parse(&data)
        .map_err(|error| format!("invalid ARM64 guest ELF {}: {error}", path.display()))?;
    if elf.class != pengudroid_elf::Class::Elf64 || elf.machine != 183
        || !matches!(data.get(16..18), Some([2, 0] | [3, 0]))
    {
        return Err(format!("ARM64 native-bridge guest {} must be an ELF64 AArch64 executable/library", path.display()));
    }
    for (kind, required, symbols) in [("export", exports, &elf.exports), ("import", imports, &elf.imports)] {
        for symbol in required {
            if !symbols.iter().any(|name| name == symbol) {
                return Err(format!(
                    "ARM64 native-bridge guest {} is missing required dynamic {kind} {symbol}; \
                     stock GSI linker/libc are not native-bridge variants", path.display()
                ));
            }
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct BridgeLibrary {
    path: PathBuf,
    name: String,
}

/// An explicit file is authoritative; a directory must contain the requested
/// library. Without a source, leave soname lookup to the dynamic loader.
fn select_bridge_library(name: &str, source: Option<&Path>) -> Result<BridgeLibrary, String> {
    let (dir, name) = match source {
        Some(source) if source.is_file() => {
            let name = source.file_name().and_then(|name| name.to_str())
                .ok_or_else(|| format!("invalid native bridge filename: {}", source.display()))?;
            let dir = source.parent().filter(|dir| !dir.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            (Some(dir), name)
        }
        Some(source) if source.is_dir() => (Some(source), name),
        Some(source) => return Err(format!(
            "native bridge source {} is not a file or directory", source.display()
        )),
        None => (None, name),
    };
    // The cached API-37 LoadNativeBridge checks an ASCII letter first, then
    // letters/digits/._-. In particular it rejects slash-containing paths.
    if !name.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
        || !name.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        || Path::new(name).file_name().and_then(|name| name.to_str()) != Some(name)
        || Path::new(name).components().count() != 1
    {
        return Err(format!(
            "invalid native bridge library name {name:?}; expected a filename, \
             or use --bridge-lib for an explicit file"
        ));
    }
    let path = dir.map(|dir| dir.join(name)).unwrap_or_else(|| PathBuf::from(name));
    if source.is_some() && !path.is_file() {
        return Err(format!("requested native bridge translator {} is not a file", path.display()));
    }
    Ok(BridgeLibrary { path, name: name.to_string() })
}

struct PreloadedBridge {
    // The VM/guest threads retain bridge pointers for the process lifetime.
    // Keep both loader references, even when the owning Rust frame returns.
    handle: *mut c_void,
    name_handle: *mut c_void,
    translator: std::cell::RefCell<Translator>,
}

fn verify_bridge_handles(selected: *mut c_void, by_name: *mut c_void) -> Result<(), String> {
    if selected.is_null() || by_name.is_null() || selected != by_name {
        return Err("native bridge basename does not resolve to the explicitly preloaded translator; \
                    refusing LoadNativeBridge and a mismatched direct self-test".into());
    }
    Ok(())
}

fn preload_bridge_library(library: &BridgeLibrary, explicit: bool) -> Result<PreloadedBridge, String> {
    use std::os::unix::fs::MetadataExt;
    let path = if explicit {
        std::fs::canonicalize(&library.path)
            .map_err(|error| format!("cannot resolve native bridge {}: {error}", library.path.display()))?
    } else {
        library.path.clone()
    };
    let selected = if explicit {
        Some(std::fs::metadata(&path).map_err(|error| format!("cannot inspect {}: {error}", path.display()))?)
    } else { None };
    let cpath = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| "native bridge path contains a NUL byte".to_string())?;
    let cname = CString::new(library.name.as_str())
        .map_err(|_| "native bridge name contains a NUL byte".to_string())?;
    let handle = unsafe { libc::dlopen(cpath.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
    if handle.is_null() {
        return Err(format!("dlopen selected translator {} failed: {}", path.display(), dlerror()));
    }
    let symbol = unsafe { libc::dlsym(handle, c"NativeBridgeItf".as_ptr()) };
    let mut origin: libc::Dl_info = unsafe { std::mem::zeroed() };
    if symbol.is_null() || unsafe { libc::dladdr(symbol, &mut origin) } == 0 || origin.dli_fname.is_null() {
        return Err(format!("selected translator {} has no attributable NativeBridgeItf", path.display()));
    }
    if let Some(selected) = selected {
        use std::os::unix::ffi::OsStrExt;
        let loaded_path = Path::new(std::ffi::OsStr::from_bytes(unsafe { CStr::from_ptr(origin.dli_fname) }.to_bytes()));
        let loaded = std::fs::metadata(loaded_path)
            .map_err(|error| format!("cannot identify loaded translator {}: {error}", loaded_path.display()))?;
        if (selected.dev(), selected.ino()) != (loaded.dev(), loaded.ino()) {
            return Err(format!("NativeBridgeItf came from {}, not selected translator {}", loaded_path.display(), path.display()));
        }
    }
    // NOLOAD must find this already-loaded object under the slash-free name
    // that AOSP will use. Never load an alternative candidate to satisfy it.
    let name_handle = unsafe { libc::dlopen(cname.as_ptr(), libc::RTLD_NOW | libc::RTLD_NOLOAD) };
    verify_bridge_handles(handle, name_handle)?;
    if unsafe { libc::dlsym(name_handle, c"NativeBridgeItf".as_ptr()) } != symbol {
        return Err("native bridge basename resolves a different NativeBridgeItf".into());
    }
    // Retain the validated table now. Self-tests must not reopen a later path.
    let translator = Translator::open(Path::new(&library.name)).map_err(|error| error.to_string())?;
    Ok(PreloadedBridge { handle, name_handle, translator: std::cell::RefCell::new(translator) })
}

fn requested_bridge_isa(init: Option<&str>, selftest: Option<&str>) -> Result<Option<&'static str>, String> {
    if let Some(isa) = init {
        if isa != pengudroid_nativebridge::ARM64_ISA {
            return Err(format!(
                "unsupported native bridge ISA {isa:?}; expected arm64 for Android 17 / API 37 x86_64"
            ));
        }
    }
    if let Some(spec) = selftest {
        if !matches!(spec.split_once('#'), Some((lib, symbol)) if !lib.is_empty() && !symbol.is_empty()) {
            return Err("--bridge-selftest expects <lib>#<symbol> with both parts nonempty".into());
        }
    }
    Ok((init.is_some() || selftest.is_some()).then_some(pengudroid_nativebridge::ARM64_ISA))
}

fn require_bridge_success(operation: &str, success: bool) -> Result<(), String> {
    if success { Ok(()) } else { Err(format!("native bridge {operation} failed")) }
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
            "--native-bridge" => {
                let Some(name) = it.next() else {
                    eprintln!("pengudroid: --native-bridge requires a library name");
                    return 2;
                };
                native_bridge = Some(name.clone());
            }
            "--bridge-lib" => {
                let Some(source) = it.next() else {
                    eprintln!("pengudroid: --bridge-lib requires a file or directory");
                    return 2;
                };
                bridge_lib = Some(source.clone());
            }
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
    let configured_guest = std::env::var_os("PENGUDROID_ARM64_GUEST_ROOT");
    let guest_requested = passthrough.iter().any(|arg| matches!(arg.as_str(), "--init-bridge" | "--bridge-selftest"));
    let guest_root = if guest_requested {
        let selected = match bridge_guest_root(&layout.root, configured_guest.as_deref()) {
            Ok(root) => root,
            Err(error) => { eprintln!("pengudroid: {error}"); return 1; }
        };
        if let Err(error) = validate_bridge_guest(&selected) {
            eprintln!("pengudroid: {error}");
            return 1;
        }
        Some(selected)
    } else { None };
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
    if configured_guest.is_some() {
        if let Some(root) = &guest_root {
            cmd.env("PENGUDROID_ARM64_GUEST_ROOT", root);
        } else {
            cmd.env_remove("PENGUDROID_ARM64_GUEST_ROOT");
        }
    }
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
        let name = native_bridge.clone()
            .or_else(|| std::env::var("PENGUDROID_PROP_ro.dalvik.vm.native.bridge").ok())
            .unwrap_or_else(|| DEFAULT_NATIVE_BRIDGE.to_string());
        let source = match select_bridge_library(&name, Some(Path::new(dir))) {
            Ok(source) => source,
            Err(error) => {
                eprintln!("pengudroid: {error}");
                return 1;
            }
        };
        // Prepare the translator's directory so its sibling dependencies are
        // available too, but never substitute another translator on failure.
        let bridge_dir = source.path.parent().unwrap_or_else(|| Path::new("."));
        match de_bionic_dir(bridge_dir, &prepared) {
            Ok(n) => println!("art: prepared {n} bridge libraries in {}", prepared.display()),
            Err(e) => {
                eprintln!("pengudroid: {e}");
                return 1;
            }
        }
        let prepared_bridge = match select_bridge_library(&source.name, Some(&prepared)) {
            Ok(bridge) => bridge,
            Err(error) => {
                eprintln!("pengudroid: prepared translator is unavailable: {error}");
                return 1;
            }
        };
        cmd.env("PENGUDROID_BRIDGE_LIB_PATH", &prepared_bridge.path);
        native_bridge = Some(prepared_bridge.name);
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
    // Extra preloads for diagnostics are loaded *after* the VM instead (see
    // `art_main`), because ART installs its fault handler during Runtime::Init
    // and a preloaded handler would be replaced by it.
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
        .env("PENGUDROID_SUPPRESS_DESTRUCTORS", "1")
        // One surface geometry for the whole tree: the shim reports these through
        // `ANativeWindow_getWidth`/`_getHeight`, the gleshim sizes its FBO from
        // them, and the host lays the window frame out against the same numbers.
        .env("PENGUDROID_SURFACE_W", "720")
        .env("PENGUDROID_SURFACE_H", "1280");
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
            "--init-bridge" => {
                let Some(isa) = it.next() else {
                    eprintln!("art: --init-bridge requires the arm64 ISA");
                    return 2;
                };
                init_bridge = Some(isa.clone());
            }
            "--bridge-selftest" => {
                let Some(spec) = it.next() else {
                    eprintln!("art: --bridge-selftest expects <lib>#<symbol>");
                    return 2;
                };
                bridge_selftest = Some(spec.clone());
            }
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
    let bridge_isa = match requested_bridge_isa(init_bridge.as_deref(), bridge_selftest.as_deref()) {
        Ok(isa) => isa,
        Err(error) => {
            eprintln!("art: {error}");
            return 2;
        }
    };
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
    if bridge_isa.is_some() {
        let configured = std::env::var_os("PENGUDROID_ARM64_GUEST_ROOT");
        let guest_root = match bridge_guest_root(&layout.root, configured.as_deref()) {
            Ok(root) => root,
            Err(error) => { diag(&format!("art: {error}")); return 1; }
        };
        if let Err(error) = validate_bridge_guest(&guest_root) {
            diag(&format!("art: {error}"));
            return 1;
        }
        diag(&format!("art: native-bridge guest root = {}", guest_root.display()));
        // glibc consumed its loader environment at exec time. Keep that cached
        // host search path, but do not hand its x86_64 directories/preloads to
        // the guest linker, which parses environ afresh in linker_main.cpp.
        // Do this before loading ART/starting VM threads, not during bootstrap.
        std::env::set_var("LD_LIBRARY_PATH", "/system/lib64/arm64/bootstrap:/system/lib64/arm64");
        std::env::remove_var("LD_PRELOAD");
        std::env::remove_var("LD_AUDIT");
        diag("art: guest loader environment uses ARM64 paths (host glibc search cache unchanged)");
    }
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
        // Measured on the API-37 GSI: `-Ximage:` is a *location* whose directory
        // gets `<arch>/<basename>` appended, so `framework/boot.art` resolves to
        // `framework/x86_64/boot.art` (the file this image really has). Passing
        // `framework/x86_64/boot.art` instead makes ART look for
        // `framework/x86_64/x86_64/boot.art` and miss.
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

    let bridge_setup = if let Some(isa) = bridge_isa {
        let data_dir = std::env::var_os("PENGUDROID_BRIDGE_DATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("pengudroid-bridge"));
        if let Err(error) = std::fs::create_dir_all(&data_dir) {
            diag(&format!("art: cannot create bridge data directory {}: {error}", data_dir.display()));
            return 1;
        }
        match setup_native_bridge(libs.as_deref(), &data_dir, isa) {
            Ok(setup) => Some(setup),
            Err(error) => {
                diag(&format!("art: {error}"));
                return 1;
            }
        }
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
            // A requested bridge must be ready before JNI_OnLoad or any app
            // work can run. Direct self-tests still perform the guest bootstrap.
            if let Some(setup) = &bridge_setup {
                diag(&format!("art: guest bootstrap LD_LIBRARY_PATH = {:?}; LD_PRELOAD = {:?}",
                    std::env::var_os("LD_LIBRARY_PATH"), std::env::var_os("LD_PRELOAD")));
                let result = if let Some(spec) = bridge_selftest.as_deref() {
                    require_bridge_success("selftest", run_bridge_selftest(setup, &env, spec))
                } else {
                    finish_native_bridge(setup, &env)
                };
                if let Err(error) = result {
                    diag(&format!("art: {error}"));
                    return 1;
                }
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
            // `AudioSystem`/`AudioTrack` register through `RegisterMethodsOrDie`
            // *inside* `libandroid_runtime`, so the shim's exported
            // `registerNativeMethods` interposer never sees them. Install the
            // descriptor-checked API-37 adapter only after the real groups ran: it
            // must replace exactly those natives, and a missing contract has to
            // fail here instead of leaving a C++ pointer path reachable.
            if register_libs.iter().any(|lib| lib.contains("libandroid_runtime")) {
                let symbol = unsafe {
                    libc::dlsym(
                        libc::RTLD_DEFAULT,
                        c"__pengudroid_register_audio_api37".as_ptr(),
                    )
                };
                if symbol.is_null() {
                    eprintln!("audio: the shim does not export the API-37 audio adapter");
                    return 1;
                }
                type Register = unsafe extern "C" fn(*mut c_void) -> c_int;
                let register: Register = unsafe { std::mem::transmute(symbol) };
                let rc = unsafe { register(env.0) };
                if rc != 0 {
                    eprintln!("audio: API-37 audio adapter installation failed ({rc})");
                    return 1;
                }
            }
            if version != crate::jni::JNI_VERSION_1_6 {
                eprintln!("art: unexpected JNI version; the function table is misaligned");
                return 1;
            }
            if let Ok(name) = std::env::var("PENGUDROID_PROBE_SERVICE") {
                probe_service(&env, &name);
            }
            if let Some(extra) = std::env::var_os("PENGUDROID_EXTRA_PRELOAD") {
                // Load diagnostics *after* the VM: ART installs its fault handler
                // during `Runtime::Init`, and a preloaded handler would be
                // replaced by it. Loading here puts ours last.
                for path in extra.to_string_lossy().split(':') {
                    if path.is_empty() {
                        continue;
                    }
                    if let Ok(c) = CString::new(path) {
                        let h = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
                        if h.is_null() {
                            diag(&format!("art: dlopen({path}) failed: {}", dlerror()));
                        } else {
                            diag(&format!("art: loaded diagnostic {path}"));
                        }
                    }
                }
            }
            // Android's LTO inlines `AndroidRuntime::getJNIEnv()` in framework
            // translation units, which then read the static `mJavaVM` member
            // directly. Publish our VM there so those paths do not fault.
            if let Ok(name) = CString::new("__pengudroid_publish_java_vm") {
                let f = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
                if f.is_null() {
                    diag("art: the shim does not export the JavaVM publisher");
                } else {
                    let publish: unsafe extern "C" fn() -> bool = unsafe { std::mem::transmute(f) };
                    if unsafe { publish() } {
                        diag("art: published JavaVM into AndroidRuntime::mJavaVM");
                    } else {
                        diag("art: could not publish JavaVM into AndroidRuntime::mJavaVM");
                    }
                }
            }
            if std::env::var_os("PENGUDROID_CRASH_BT").is_some() {
                // Take the fault handler slot away from ART, which otherwise
                // reports `HandleUnexpectedSignal reenter` and nothing else.
                if let Ok(name) = CString::new("__pengudroid_install_crash_handler") {
                    let f = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
                    if f.is_null() {
                        diag("art: the shim does not export the crash handler");
                    } else {
                        let install: unsafe extern "C" fn() = unsafe { std::mem::transmute(f) };
                        unsafe { install() };
                        diag("art: installed the pengudroid fault handler");
                    }
                }
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
                    if !crate::apphost::prepare_main_thread(&env) {
                        eprintln!("apphost: main-thread preparation failed; not binding the app");
                        return 1;
                    }
                    if !bind_application_now(&env, &classpath) {
                        eprintln!("apphost: application bind failed; not entering the main Looper");
                        return 1;
                    }
                    crate::apphost::loop_forever(&env);
                    return 0;
                }
                return run_main(&env, &class, &app_args);
            }
            if std::env::var_os("PENGUDROID_PROBE_MAIN_THREAD").is_some()
                && !crate::apphost::prepare_main_thread(&env) {
                eprintln!("apphost: main-thread probe failed");
                return 1;
            }
            if std::env::var_os("PENGUDROID_PROBE_PROCESS_IDENTITY").is_some()
                && !crate::process_identity::probe(&env) {
                eprintln!("apphost: process-identity probe failed");
                return 1;
            }
            if std::env::var_os("PENGUDROID_PROBE_ACTIVITY_REQUESTS").is_some()
                && !crate::apphost::probe_activity_requests(&env) {
                eprintln!("apphost: activity-request probe failed");
                return 1;
            }
            if std::env::var_os("PENGUDROID_PROBE_MAIN_DISPATCH").is_some()
                && !crate::main_thread_probe::run(&env) {
                eprintln!("apphost: main-thread dispatch probe failed");
                return 1;
            }
            if std::env::var_os("PENGUDROID_PROBE_MEDIA_LIST").is_some() {
                if !crate::apphost::probe_media_codec_list(&env) {
                    eprintln!("media: API-37 codec enumeration failed");
                    return 1;
                }
            }
            if std::env::var_os("PENGUDROID_PROBE_NATIVE_DISPLAY").is_some() {
                if !crate::apphost::probe_native_display(&env) { return 1; }
            }
            if std::env::var_os("PENGUDROID_PROBE_BLAST_LIFECYCLE").is_some() {
                if !crate::apphost::probe_blast_lifecycle(&env) { return 1; }
            }
            if std::env::var_os("PENGUDROID_PROBE_PLATFORM_STATE").is_some() {
                if !crate::apphost::probe_platform_state(&env) { return 1; }
            }
            if std::env::var_os("PENGUDROID_PROBE_SETTINGS").is_some() {
                if !crate::apphost::probe_settings(&env) {
                    eprintln!("settings: Binder/Parcel probe failed");
                    return 1;
                }
            }
            if std::env::var_os("PENGUDROID_PROBE_AUDIO_API37").is_some() {
                if !crate::apphost::probe_audio_api37(&env) {
                    eprintln!("audio: API-37 AudioTrack gate failed");
                    return 1;
                }
            }
            if std::env::var_os("PENGUDROID_PROBE_AUDIO_FOCUS").is_some() {
                if !crate::apphost::probe_audio_focus(&env) {
                    eprintln!("audio: AIDL focus probe failed");
                    return 1;
                }
            }
            if std::env::var_os("PENGUDROID_PROBE_WINDOW_CONFIG").is_some() {
                if !crate::apphost::probe_window_configuration(&env) {
                    eprintln!("window: local layout probe failed");
                    return 1;
                }
            }
            if std::env::var_os("PENGUDROID_PROBE_KEYBOARD").is_some() {
                if !crate::apphost::probe_keyboard(&env) {
                    eprintln!("keyboard: native InputManager/KeyCharacterMap probe failed");
                    return 1;
                }
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

/// The JNI hooks the bridge calls to walk a class's native methods. The type
/// and its layout belong to `pengudroid-nativebridge`; the CLI only fills it in.
///
/// `LoadNativeBridge` does **not** copy the table; it stores the pointer and
/// hands it back to the translator on every later JNI call. A stack-local table
/// would leave the translator dereferencing overwritten stack bytes (measured:
/// `GetJMethodShorty` retains and jumps through it). It therefore has to
/// outlive the whole process, so it is a `static`.
static BRIDGE_CALLBACKS: RuntimeCallbacks = RuntimeCallbacks {
    get_method_shorty: bridge_get_method_shorty,
    get_native_method_count: bridge_get_native_method_count,
    get_native_methods: bridge_get_native_methods,
};

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
    /// Exact preloaded object and retained table, shared with the direct self-test.
    translator: PreloadedBridge,
    initialize: unsafe extern "C" fn(*mut c_void, *const c_char) -> bool,
    isa: String,
}

/// Steps 1 and 2 of the bridge state machine, run before the VM exists.
///
/// `app_process` does `LoadNativeBridge` and `PreInitializeNativeBridge` before
/// `JNI_CreateJavaVM`, and `InitializeNativeBridge` afterwards; the order is
/// load-bearing because each step asserts the previous one happened.
fn setup_native_bridge(libs: Option<&Path>, data_dir: &Path, isa: &str) -> Result<BridgeSetup, String> {
    requested_bridge_isa(Some(isa), None)?;
    let name = std::env::var("PENGUDROID_PROP_ro.dalvik.vm.native.bridge")
        .unwrap_or_else(|_| DEFAULT_NATIVE_BRIDGE.to_string());
    let explicit = std::env::var_os("PENGUDROID_BRIDGE_LIB_PATH").map(PathBuf::from);
    let translator = select_bridge_library(&name, explicit.as_deref())?;
    let cname = CString::new(translator.name.as_str())
        .map_err(|_| "native bridge library name contains a NUL byte".to_string())?;

    let cdata = CString::new(data_dir.as_os_str().as_encoded_bytes())
        .map_err(|_| format!("bridge data directory {} contains a NUL byte", data_dir.display()))?;
    let cisa = CString::new(isa).map_err(|_| "native bridge ISA contains a NUL byte".to_string())?;
    let path = libs.map(|dir| dir.join("libnativebridge.so"))
        .unwrap_or_else(|| PathBuf::from("libnativebridge.so"));
    let cpath = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| format!("native bridge loader path {} contains a NUL byte", path.display()))?;
    let handle = unsafe { libc::dlopen(cpath.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
    if handle.is_null() {
        return Err(format!("dlopen {} failed: {}", path.display(), dlerror()));
    }
    let load = unsafe { libc::dlsym(handle, c"LoadNativeBridge".as_ptr()) };
    let pre = unsafe { libc::dlsym(handle, c"PreInitializeNativeBridge".as_ptr()) };
    let init = unsafe { libc::dlsym(handle, c"InitializeNativeBridge".as_ptr()) };
    for (name, symbol) in [("LoadNativeBridge", load), ("PreInitializeNativeBridge", pre), ("InitializeNativeBridge", init)] {
        if symbol.is_null() {
            return Err(format!("{} does not export {name}", path.display()));
        }
    }
    let preloaded = preload_bridge_library(&translator, explicit.is_some())?;
    verify_bridge_handles(preloaded.handle, preloaded.name_handle)?;
    type Load = unsafe extern "C" fn(*const c_char, *const RuntimeCallbacks) -> bool;
    let f: Load = unsafe { std::mem::transmute(load) };
    let loaded = unsafe { f(cname.as_ptr(), &BRIDGE_CALLBACKS) };
    diag(&format!("art: LoadNativeBridge({}) = {loaded}", translator.name));
    require_bridge_success(&format!("LoadNativeBridge({})", translator.name), loaded)?;

    type PreInit = unsafe extern "C" fn(*const c_char, *const c_char) -> bool;
    let f: PreInit = unsafe { std::mem::transmute(pre) };
    let pre_ok = unsafe { f(cdata.as_ptr(), cisa.as_ptr()) };
    diag(&format!(
        "art: PreInitializeNativeBridge({}, {isa}) = {pre_ok}",
        data_dir.display()
    ));
    require_bridge_success("PreInitializeNativeBridge", pre_ok)?;

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
    Ok(BridgeSetup {
        handle,
        translator: preloaded,
        initialize: unsafe { std::mem::transmute(init) },
        isa: isa.to_string(),
    })
}

/// Drive the translator's `NativeBridgeItf` table directly.
///
/// This bypasses libnativebridge's state machine, not the guest bootstrap:
/// Digitalis initialization still requires its modified ARM64 linker, libc,
/// and `/system/bin/arm64/app_process64`.
///
/// ```text
/// NativeBridgeItf.initialize(runtime_cbs, private_dir, "arm64")
/// NativeBridgeItf.loadLibrary("<lib>/lib/arm64/libfixture.so", RTLD_NOW)
/// NativeBridgeItf.getTrampoline(handle, sym, shorty, len)
/// ```
///
/// The table layout, the argument order and the path rule live in
/// `pengudroid-nativebridge`; this function only orchestrates. A self-test that
/// logs a failure and returns success would be worse than no test at all, so
/// every step must produce a callable entry point and a completed call.
fn run_bridge_selftest(setup: &BridgeSetup, env: &crate::jni::JniEnv, spec: &str) -> bool {
    let (lib, symbol) = match spec.split_once('#') {
        Some(v) => v,
        None => {
            eprintln!("art: --bridge-selftest expects <lib>#<symbol>");
            return false;
        }
    };
    let mut translator = setup.translator.translator.borrow_mut();
    diag(&format!(
        "art: NativeBridgeItf.version = {} ({})",
        translator.version(),
        translator.path().display()
    ));

    let data_dir = std::env::var_os("PENGUDROID_BRIDGE_DATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("pengudroid-bridge"));
    if let Err(error) = std::fs::create_dir_all(&data_dir) {
        diag(&format!("art: cannot create bridge data directory {}: {error}", data_dir.display()));
        return false;
    }
    if let Err(error) = translator.initialize(&BRIDGE_CALLBACKS, &data_dir, &setup.isa) {
        diag(&format!("art: {error}"));
        return false;
    }
    diag(&format!(
        "art: Itf.initialize({}, {}) = true",
        data_dir.display(),
        setup.isa
    ));

    let library = match translator.load_library(Path::new(lib)) {
        Ok(library) => library,
        Err(error) => {
            diag(&format!("art: {error}"));
            return false;
        }
    };
    diag(&format!("art: Itf.loadLibrary({lib}) = {:p}", library.handle()));

    // The shorty is the JNI shorty (`I` for a static `()I`), not the method
    // descriptor `()I`; the type rejects the descriptor before the bridge sees
    // it, because a descriptor produces a wrongly marshalled trampoline.
    let shorty = match Shorty::parse("I") {
        Ok(shorty) => shorty,
        Err(error) => {
            diag(&format!("art: {error}"));
            return false;
        }
    };
    let function = match translator.guest_function(&library, symbol, &shorty) {
        Ok(function) => function,
        Err(error) => {
            diag(&format!("art: {error}"));
            return false;
        }
    };
    diag(&format!(
        "art: Itf.getTrampoline({symbol}, shorty=I) = {:p}",
        function.address()
    ));
    diag("art: calling the translated guest function");
    let value = unsafe { function.call_int(env.0, &[]) };
    diag(&format!("art: guest {symbol}() returned {value}"));
    true
}

type BridgeSignalHandler = unsafe extern "C" fn(c_int, *mut libc::siginfo_t, *mut c_void) -> bool;
type GetBridgeSignalHandler = unsafe extern "C" fn(c_int) -> Option<BridgeSignalHandler>;
type InstallBridgeSignalHandler = unsafe extern "C" fn(c_int, Option<BridgeSignalHandler>) -> c_int;

/// NativeBridgeGetSignalHandler returns a bool callback, never a sigaction.
/// Null means the bridge does not request chaining for that signal.
fn chain_bridge_handlers(get: GetBridgeSignalHandler, install: Option<InstallBridgeSignalHandler>) -> Result<(), String> {
    for sig in [libc::SIGILL, libc::SIGBUS, libc::SIGFPE, libc::SIGSEGV] {
        let callback = unsafe { get(sig) };
        if callback.is_none() {
            continue;
        }
        let install = install.ok_or("native bridge requires signal chaining, but the shim has no safe chaining adapter")?;
        let rc = unsafe { install(sig, callback) };
        if rc != 0 {
            return Err(format!("native bridge cannot safely chain signal {sig} (adapter rc={rc}); ART handlers were not replaced"));
        }
    }
    Ok(())
}

fn install_bridge_handlers(handle: *mut c_void) -> Result<(), String> {
    let get = unsafe { libc::dlsym(handle, c"NativeBridgeGetSignalHandler".as_ptr()) };
    if get.is_null() {
        return Err("libnativebridge has no NativeBridgeGetSignalHandler; cannot validate signal chaining".into());
    }
    let install = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"__pengudroid_install_bridge_handler".as_ptr()) };
    let install = if install.is_null() { None } else {
        Some(unsafe { std::mem::transmute::<*mut c_void, InstallBridgeSignalHandler>(install) })
    };
    chain_bridge_handlers(unsafe { std::mem::transmute::<*mut c_void, GetBridgeSignalHandler>(get) }, install)
}

/// Step 3: hand the live VM to the bridge.
fn finish_native_bridge(setup: &BridgeSetup, env: &crate::jni::JniEnv) -> Result<(), String> {
    let cisa = CString::new(setup.isa.as_str())
        .map_err(|_| "native bridge ISA contains a NUL byte".to_string())?;
    let ok = unsafe { (setup.initialize)(env.0, cisa.as_ptr()) };
    diag(&format!("art: InitializeNativeBridge({}) = {ok}", setup.isa));
    require_bridge_success("InitializeNativeBridge", ok)?;

    // Preserve ART's chain; non-null native-bridge callbacks need a real chain
    // API, not a replacement kernel sigaction.
    install_bridge_handlers(setup.handle)?;
    let available = unsafe { libc::dlsym(setup.handle, c"NativeBridgeAvailable".as_ptr()) };
    if !available.is_null() {
        let g: unsafe extern "C" fn() -> bool = unsafe { std::mem::transmute(available) };
        let ok = unsafe { g() };
        diag(&format!("art: NativeBridgeAvailable() = {ok}"));
        require_bridge_success("NativeBridgeAvailable", ok)?;
    }
    let version = unsafe { libc::dlsym(setup.handle, c"NativeBridgeGetVersion".as_ptr()) };
    if !version.is_null() {
        let g: unsafe extern "C" fn() -> u32 = unsafe { std::mem::transmute(version) };
        diag(&format!("art: NativeBridgeGetVersion() = {}", unsafe { g() }));
    }
    Ok(())
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
    let head = [
        "register_android_util_Log", "register_android_util_EventLog",
        // API 37 DisplayManagerGlobal initializes a sysprop-backed static
        // field while its native group is registered. Properties must precede
        // every group that can initialize a framework class.
        "register_android_os_SystemProperties",
    ];
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
        // `BLASTBufferQueue`'s natives are registered from *inside*
        // `libandroid_runtime` (its `RegisterMethodsOrDie` is statically linked
        // there), so the shim's `registerNativeMethods` hook never sees them.
        // The shim registers that class itself instead.
        if name.contains("register_android_graphics_BLASTBufferQueue") {
            let ours = unsafe {
                libc::dlsym(
                    libc::RTLD_DEFAULT,
                    c"__pengudroid_register_blast_buffer_queue".as_ptr(),
                )
            };
            if ours.is_null() {
                failed.push((name.clone(), -1));
                continue;
            }
            let f: Register = unsafe { std::mem::transmute(ours) };
            let rc = unsafe { f(env.0) };
            if rc == 0 {
                ok += 1;
            } else {
                failed.push((name.clone(), rc));
            }
            continue;
        }
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
        // `android.view.Surface` registers its whole table from inside
        // `libandroid_runtime`, so the shim's `registerNativeMethods` hook never
        // sees it. Every entry is needed except
        // `nativeGetFromBlastBufferQueue`, which must be ours: the framework
        // implementation casts the queue pointer to a real `BLASTBufferQueue*`.
        // Registering a one-entry table afterwards replaces just that method.
        if name.contains("register_android_view_SurfaceE") {
            let ours = unsafe {
                libc::dlsym(
                    libc::RTLD_DEFAULT,
                    c"__pengudroid_register_surface_overrides".as_ptr(),
                )
            };
            if !ours.is_null() {
                let f: Register = unsafe { std::mem::transmute(ours) };
                let _ = unsafe { f(env.0) };
            }
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

/// Default missing metadata, not a declared per-app compatibility level.
fn app_target_sdk(target_sdk: Option<u32>) -> i32 {
    target_sdk.map(|sdk| sdk as i32).unwrap_or(37)
}

/// Bind an APK on the prepared ActivityThread's main Looper thread.
/// Failure must propagate before the caller enters the application's main loop.
fn bind_application_now(env: &crate::jni::JniEnv, classpath: &[String]) -> bool {
    let Some(apk) = classpath.first().cloned() else {
        eprintln!("apphost: --bind-app needs --classpath <apk>");
        return false;
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
        return false;
    }
    let info = match crate::apk::read_apk(&apk) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("apphost: {e}");
            return false;
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
        let libs = match crate::apk::native_libs(apk) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("apphost: cannot list native libraries of {apk}: {e}");
                continue;
            }
        };
        if std::env::var_os("PENGUDROID_TRACE_ROOTFS").is_some() {
            let abis: std::collections::BTreeSet<&str> =
                libs.iter().map(|l| l.abi.as_str()).collect();
            eprintln!("apphost: {apk}: {} libraries, abis {abis:?}", libs.len());
            eprintln!("apphost: native lib dir = {}", lib_dir.display());
        }
        for lib in libs.iter().filter(|l| l.abi == "x86_64") {
            let dest = lib_dir.join(&lib.file);
            if dest.exists() {
                continue;
            }
            match crate::apk::extract_named(apk, &lib.entry) {
                Ok(bytes) => match std::fs::write(&dest, &bytes) {
                    Ok(()) => extracted += 1,
                    Err(e) => eprintln!(
                        "apphost: cannot write {} ({} bytes): {e}",
                        dest.display(),
                        bytes.len()
                    ),
                },
                Err(e) => eprintln!("apphost: cannot extract {}: {e}", lib.entry),
            }
        }
    }
    if extracted > 0 {
        println!("apphost: extracted {extracted} x86_64 native libraries");
    } else if std::fs::read_dir(&lib_dir).map(|d| d.count()).unwrap_or(0) == 0 {
        // Only when the directory really ends up empty: this runs more than once
        // per launch, and on the second call every file already exists, which is
        // not a problem. An empty `lib/` is, because `System.loadLibrary` then
        // fails silently and every `native*` method is an UnsatisfiedLinkError.
        eprintln!(
            "apphost: no x86_64 native libraries in {} (classpath: {})",
            lib_dir.display(),
            classpath.join(":")
        );
    }
    // Every `--classpath` after the first is a split of the same bundle, so it
    // belongs on the app's class path, not just in the native library search.
    let splits: Vec<std::path::PathBuf> = classpath
        .iter()
        .skip(1)
        .map(|p| std::path::PathBuf::from(p))
        .collect();
    // Drop providers whose class the APK does not ship. A missing one makes
    // `installContentProviders` throw, which aborts the whole
    // `handleBindApplication`; a split-based bundle can legitimately lack a
    // feature split that its manifest still names.
    let usable_providers: Vec<crate::apk::Component> = info
        .providers
        .iter()
        .filter(|p| {
            let present = classpath.iter().any(|a| crate::apk::contains_class(a, &p.name));
            if !present {
                println!(
                    "apphost: skipping provider {} (class not in the APK)",
                    p.name
                );
            }
            present
        })
        .cloned()
        .collect();
    let spec = crate::apphost::AppSpec {
        package: &package,
        apk: &apk_path,
        splits: &splits,
        data_dir: &data_dir,
        native_lib_dir: &lib_dir,
        target_sdk: app_target_sdk(info.target_sdk),
        min_sdk: info.min_sdk.map(|v| v as i32).unwrap_or(23),
        app_class: info.application_class.as_deref(),
        providers: &usable_providers,
        // A hosted `startActivity` builds the target `ActivityInfo` from the
        // manifest entry, so every activity is available to the host, not just
        // the initial one.
        activities: &info.activities,
        application_theme: info.application_theme,
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
    crate::apphost::bind_application(env, thread, &spec)
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
    fn binding_without_classpath_fails_before_accessing_jni() {
        let env = crate::jni::JniEnv(std::ptr::null_mut());
        assert!(!bind_application_now(&env, &[]));
    }

    // Synthetic metadata and ELF headers only. No test loads an Android runtime
    // or uses either of the user's .toolchain sysroot directories as a fixture.
    struct Fixture(PathBuf);

    impl Fixture {
        fn empty() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let dir = std::env::temp_dir().join(format!("pd-layout-{}-{stamp}-{id}", std::process::id()));
            std::fs::create_dir(&dir).unwrap();
            Self(dir)
        }

        fn android17() -> Self {
            let fixture = Self::empty();
            fixture.write("system/build.prop", b"ro.build.version.sdk=37\nro.build.version.release=17\n");
            fixture.write("system/apex/com.android.art/lib64/libart.so", &elf_header(2, 1, 0x3e));
            for name in ["core-oj", "core-libart", "okhttp", "bouncycastle", "apache-xml"] {
                fixture.write(&format!("system/apex/com.android.art/javalib/{name}.jar"), b"jar");
            }
            fixture.write("system/apex/com.android.i18n/javalib/core-icu4j.jar", b"jar");
            fixture.write("system/framework/framework.jar", b"jar");
            fixture.fragment("system/etc/classpaths/bootclasspath.pb", &["/system/framework/framework.jar"]);
            fixture
        }

        fn write(&self, path: &str, data: &[u8]) {
            let path = self.0.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, data).unwrap();
        }

        fn fragment(&self, path: &str, jars: &[&str]) {
            let bytes: Vec<u8> = jars.iter().flat_map(|jar| pb_entry(jar)).collect();
            self.write(path, &bytes);
        }

        fn error(&self) -> String {
            Layout::detect(&self.0).unwrap_err()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn elf_header(class: u8, data: u8, machine: u16) -> Vec<u8> {
        let mut header = vec![0u8; 64];
        header[..4].copy_from_slice(b"\x7fELF");
        header[4] = class;
        header[5] = data;
        header[6] = 1;
        header[18..20].copy_from_slice(&machine.to_le_bytes());
        header
    }

    fn pb_varint(mut value: u64, out: &mut Vec<u8>) {
        while value >= 0x80 {
            out.push((value as u8 & 0x7f) | 0x80);
            value >>= 7;
        }
        out.push(value as u8);
    }

    fn pb_bytes(field: u64, value: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        pb_varint((field << 3) | 2, &mut out);
        pb_varint(value.len() as u64, &mut out);
        out.extend_from_slice(value);
        out
    }

    /// Serialize the fragment's field-1 Jar and its field-1 path.
    fn pb_entry(path: &str) -> Vec<u8> {
        pb_bytes(1, &pb_bytes(1, path.as_bytes()))
    }

    #[test]
    fn bridge_directory_selects_the_requested_translator_not_the_directory_name() {
        let fixture = Fixture::empty();
        fixture.write("bridge-libs/libcustom_arm64.so", b"translator");
        fixture.write("bridge-libs/libberberis_arm64.so", b"not requested");
        let source = select_bridge_library("libcustom_arm64.so", Some(&fixture.0.join("bridge-libs"))).unwrap();
        assert_eq!(source.path, fixture.0.join("bridge-libs/libcustom_arm64.so"));
        assert_eq!(source.name, "libcustom_arm64.so");

        fixture.write("prepared/libcustom_arm64.so", b"prepared translator");
        let prepared = select_bridge_library(&source.name, Some(&fixture.0.join("prepared"))).unwrap();
        assert_eq!(prepared.path, fixture.0.join("prepared/libcustom_arm64.so"));
        assert_eq!(prepared.name, source.name);
    }

    #[test]
    fn explicit_bridge_file_is_used_consistently_instead_of_the_default_or_requested_name() {
        let fixture = Fixture::empty();
        fixture.write("source/libexplicit.so", b"translator");
        for name in [DEFAULT_NATIVE_BRIDGE, "libcustom_arm64.so"] {
            let source = select_bridge_library(name, Some(&fixture.0.join("source/libexplicit.so"))).unwrap();
            assert_eq!(source.path, fixture.0.join("source/libexplicit.so"));
            assert_eq!(source.name, "libexplicit.so");
            fixture.write("prepared/libexplicit.so", b"prepared translator");
            let prepared = select_bridge_library(&source.name, Some(&fixture.0.join("prepared"))).unwrap();
            assert_eq!(prepared.path, fixture.0.join("prepared/libexplicit.so"));
            assert_eq!(prepared.name, source.name);
        }
    }

    #[test]
    fn bridge_soname_lookup_does_not_guess_an_art_library_path() {
        let selected = select_bridge_library("libcustom_arm64.so", None).unwrap();
        assert_eq!(selected.path, Path::new("libcustom_arm64.so"));
        assert_eq!(selected.name, "libcustom_arm64.so");
    }

    #[test]
    fn missing_requested_or_prepared_bridge_never_falls_back() {
        let fixture = Fixture::empty();
        fixture.write("source/libberberis_arm64.so", b"not requested");
        fixture.write("source/libnb.so", b"legacy translator");
        let error = select_bridge_library("libmissing.so", Some(&fixture.0.join("source"))).unwrap_err();
        assert!(error.contains("source/libmissing.so") && error.contains("not a file"), "{error}");

        fixture.write("prepared/libnb.so", b"legacy translator");
        let error = select_bridge_library(DEFAULT_NATIVE_BRIDGE, Some(&fixture.0.join("prepared"))).unwrap_err();
        assert!(error.contains("prepared/libberberis_arm64.so"), "{error}");
        let error = select_bridge_library(DEFAULT_NATIVE_BRIDGE, Some(&fixture.0.join("missing.so"))).unwrap_err();
        assert!(error.contains("missing.so") && error.contains("not a file or directory"), "{error}");

        std::fs::create_dir(fixture.0.join("source/libdirectory.so")).unwrap();
        let error = select_bridge_library("libdirectory.so", Some(&fixture.0.join("source"))).unwrap_err();
        assert!(error.contains("libdirectory.so") && error.contains("not a file"), "{error}");
    }

    #[test]
    fn bridge_names_cannot_escape_the_prepared_directory_or_be_empty() {
        let fixture = Fixture::empty();
        for name in ["", ".", "..", "../libbridge.so", "/libbridge.so", "nested/libbridge.so", "libbridge.so/", "lib\0bridge.so"] {
            for source in [None, Some(fixture.0.as_path())] {
                let error = select_bridge_library(name, source).unwrap_err();
                assert!(error.contains("invalid native bridge library name"), "{name:?}: {error}");
            }
        }
    }

    #[test]
    fn unrequested_bridge_remains_disabled_for_x86_64_apps() {
        assert_eq!(requested_bridge_isa(None, None).unwrap(), None);
    }

    #[test]
    fn bridge_requests_accept_only_arm64_and_selftest_implies_arm64() {
        assert_eq!(requested_bridge_isa(Some("arm64"), None).unwrap(), Some("arm64"));
        assert_eq!(requested_bridge_isa(None, Some("/lib/arm64/fixture.so#answer")).unwrap(), Some("arm64"));
        assert_eq!(requested_bridge_isa(Some("arm64"), Some("fixture.so#answer")).unwrap(), Some("arm64"));
        for isa in ["", "arm", "arm64-v8a", "ARM64", "x86", "x86_64", "riscv64", "arm64\0"] {
            for selftest in [None, Some("fixture.so#answer")] {
                let error = requested_bridge_isa(Some(isa), selftest).unwrap_err();
                assert!(error.contains("expected arm64") && error.contains("API 37"), "{isa:?}: {error}");
            }
        }
    }

    #[test]
    fn malformed_bridge_selftest_is_rejected_before_runtime_setup() {
        for spec in ["", "fixture.so", "#answer", "fixture.so#", "#"] {
            let error = requested_bridge_isa(None, Some(spec)).unwrap_err();
            assert!(error.contains("--bridge-selftest expects <lib>#<symbol>"), "{spec:?}: {error}");
        }
    }

    #[test]
    fn every_requested_bridge_stage_fails_closed_on_false() {
        for stage in ["LoadNativeBridge", "PreInitializeNativeBridge", "InitializeNativeBridge", "NativeBridgeAvailable", "selftest"] {
            assert!(require_bridge_success(stage, true).is_ok());
            let error = require_bridge_success(stage, false).unwrap_err();
            assert_eq!(error, format!("native bridge {stage} failed"));
        }
    }

    #[test]
    fn detects_android17_x86_64_layout() {
        let fixture = Fixture::android17();
        // A legacy flat library must not override the supported layout.
        fixture.write("lib64/libart.so", b"not an Android runtime");
        let android = Layout::detect(&fixture.0).unwrap();
        assert_eq!(android.root, std::fs::canonicalize(&fixture.0).unwrap());
        assert_eq!(android.lib_dirs.len(), 2);
        assert_eq!(android.boot_images, android.android_root.join("framework/x86_64"));
        assert!(android.required().iter().any(|(_, path)|
            path == &android.android_root.join("etc/classpaths/bootclasspath.pb")));
        for apex in ["com.android.runtime", "com.android.i18n"] {
            std::fs::create_dir_all(fixture.0.join("system/apex").join(apex).join("lib64")).unwrap();
        }
        let android = Layout::detect(&fixture.0).unwrap();
        assert_eq!(android.lib_dirs.len(), 4);
        assert!(android.android_root.ends_with("system"));
        assert_eq!(android.boot_jars.len(), android.boot_locations.len());
        for (jar, location) in android.boot_jars.iter().zip(&android.boot_locations) {
            assert_eq!(jar, &boot_jar_path(&android.android_root, location).unwrap());
        }
    }

    #[test]
    fn rejects_empty_and_legacy_flat_layouts() {
        let fixture = Fixture::empty();
        let error = fixture.error();
        assert!(error.contains("Android 17 / API 37 x86_64"), "{error}");
        fixture.write("lib64/libart.so", &elf_header(2, 1, 0x3e));
        let error = fixture.error();
        assert!(error.contains("legacy flat sysroots are not supported"), "{error}");
    }

    #[test]
    fn rejects_non37_missing_and_ambiguous_sdk_metadata() {
        let fixture = Fixture::android17();
        for props in [
            "ro.build.version.sdk=33\n", "ro.build.version.sdk=36\n",
            "ro.build.version.sdk=38\n", "ro.build.version.sdk=invalid\n",
            "ro.build.version.sdk=\n", "ro.build.version.release=17\n",
            "ro.build.version.sdk=37\nro.build.version.sdk=33\n",
        ] {
            fixture.write("system/build.prop", props.as_bytes());
            let error = fixture.error();
            assert!(error.contains("ro.build.version.sdk=37"), "{props:?}: {error}");
            assert!(error.contains("system/build.prop"), "{error}");
        }
        std::fs::remove_file(fixture.0.join("system/build.prop")).unwrap();
        let error = fixture.error();
        assert!(error.contains("requires") && error.contains("system/build.prop"), "{error}");
    }

    #[test]
    fn rejects_non_x86_64_or_invalid_art_without_layout_retry() {
        let fixture = Fixture::android17();
        for (class, data, machine) in [(2, 1, 0xb7), (1, 1, 3), (2, 2, 0x3e), (1, 1, 0x3e)] {
            fixture.write("system/apex/com.android.art/lib64/libart.so", &elf_header(class, data, machine));
            let error = fixture.error();
            assert!(error.contains("x86_64 ELF"), "{error}");
            assert!(error.contains(&format!("machine={machine}")), "{error}");
        }
        for bytes in [b"not an ELF".as_slice(), &[0u8; 64]] {
            fixture.write("system/apex/com.android.art/lib64/libart.so", bytes);
            let error = fixture.error();
            assert!(error.contains("x86_64"), "{error}");
        }
        fixture.write("lib64/libart.so", &elf_header(2, 1, 0x3e));
        std::fs::remove_file(fixture.0.join("system/apex/com.android.art/lib64/libart.so")).unwrap();
        let error = fixture.error();
        assert!(error.contains("requires an ART ELF header"), "{error}");
    }

    #[test]
    fn requires_framework_fragment_without_hardcoded_fallback() {
        let fixture = Fixture::android17();
        for bytes in [Vec::new(), pb_entry("/apex/com.android.art/javalib/core-oj.jar"), vec![0x0a, 0x80]] {
            fixture.write("system/etc/classpaths/bootclasspath.pb", &bytes);
            let error = fixture.error();
            assert!(error.contains("bootclasspath.pb"), "{error}");
        }
        std::fs::remove_file(fixture.0.join("system/etc/classpaths/bootclasspath.pb")).unwrap();
        let error = fixture.error();
        assert!(error.contains("cannot read required boot classpath fragment"), "{error}");
        assert!(error.contains("system/etc/classpaths/bootclasspath.pb"), "{error}");
    }

    #[test]
    fn uses_fragment_framework_order_and_deduplicates_without_extra_jars() {
        let fixture = Fixture::android17();
        fixture.write("system/framework/framework-location.jar", b"jar");
        fixture.write("system/framework/ext.jar", b"unlisted jar");
        fixture.fragment("system/etc/classpaths/bootclasspath.pb", &[
            "/system/framework/framework-location.jar",
            "/system/framework/framework.jar",
            "/system/framework/framework-location.jar",
        ]);
        let layout = Layout::detect(&fixture.0).unwrap();
        assert_eq!(&layout.boot_locations[6..], &[
            "/system/framework/framework-location.jar", "/system/framework/framework.jar",
        ]);
        assert_eq!(layout.boot_jars[6], layout.framework.join("framework-location.jar"));
    }

    #[test]
    fn rejects_missing_declared_framework_art_and_apex_jars() {
        let fixture = Fixture::android17();
        fixture.fragment("system/etc/classpaths/bootclasspath.pb", &["/system/framework/missing.jar"]);
        let error = fixture.error();
        assert!(error.contains("required boot classpath jar /system/framework/missing.jar"), "{error}");
        fixture.fragment("system/etc/classpaths/bootclasspath.pb", &["/system/framework/framework.jar"]);
        let core = fixture.0.join("system/apex/com.android.art/javalib/core-oj.jar");
        std::fs::remove_file(&core).unwrap();
        let error = fixture.error();
        assert!(error.contains("core-oj.jar is missing"), "{error}");
        std::fs::write(core, b"jar").unwrap();
        fixture.fragment("system/apex/com.android.test/etc/classpaths/bootclasspath.pb", &[
            "/apex/com.android.test/javalib/missing.jar",
        ]);
        let error = fixture.error();
        assert!(error.contains("/apex/com.android.test/javalib/missing.jar is missing"), "{error}");
    }

    #[test]
    fn rejects_malformed_published_apex_fragment() {
        let fixture = Fixture::android17();
        fixture.write("system/apex/com.android.test/etc/classpaths/bootclasspath.pb", &[0x0a, 0x80]);
        let error = fixture.error();
        assert!(error.contains("invalid boot classpath fragment"), "{error}");
        assert!(error.contains("com.android.test/etc/classpaths/bootclasspath.pb"), "{error}");
    }

    #[test]
    fn rejects_unsupported_or_escaping_boot_jar_locations() {
        let fixture = Fixture::android17();
        for location in [
            "system/framework/framework.jar", "/vendor/framework/framework.jar",
            "/system/framework/../framework/framework.jar", "/apex/../../outside.jar",
            "/system/framework/a:b.jar", "/system/framework/a\0b.jar",
        ] {
            assert!(boot_jar_path(&fixture.0.join("system"), location).is_err(), "{location:?}");
        }
    }

    #[test]
    fn parses_a_bootclasspath_fragment() {
        let mut data = Vec::new();
        data.extend_from_slice(&pb_entry("/apex/com.android.tethering/javalib/framework-connectivity.jar"));
        data.extend_from_slice(&pb_entry("/apex/com.android.tethering/javalib/framework-tethering.jar"));
        assert_eq!(
            jar_paths_in_bootclasspath_pb(&data).unwrap(),
            vec![
                "/apex/com.android.tethering/javalib/framework-connectivity.jar",
                "/apex/com.android.tethering/javalib/framework-tethering.jar",
            ]
        );
    }

    #[test]
    fn parses_multibyte_lengths_and_skips_unknown_protobuf_fields() {
        let path = format!("/system/framework/{}.jar", "a".repeat(160));
        let mut jar = vec![0x10, 0x01]; // Uninterpreted varint field.
        jar.extend(pb_bytes(1, path.as_bytes()));
        jar.extend([0x1d, 0, 0, 0, 0]); // Uninterpreted fixed32 field.
        jar.extend([0x21, 0, 0, 0, 0, 0, 0, 0, 0]); // Uninterpreted fixed64 field.
        let mut data = pb_bytes(7, b"unknown outer field");
        data.extend(pb_bytes(1, &jar));
        data.push(0x40);
        pb_varint(u64::MAX, &mut data);
        assert_eq!(jar_paths_in_bootclasspath_pb(&data).unwrap(), vec![path]);
    }

    #[test]
    fn rejects_malformed_protobuf_without_returning_a_partial_classpath() {
        let mut duplicate = pb_bytes(1, b"/system/framework/a.jar");
        duplicate.extend(pb_bytes(1, b"/system/framework/b.jar"));
        let mut overflowing = vec![0x0a];
        overflowing.extend([0xff; 10]);
        for bad in [
            vec![0x0a, 0x80], vec![0x0a, 0x05, 0x0a, 0x01, b'x'],
            // The path must not consume bytes outside its enclosing Jar.
            vec![0x0a, 0x02, 0x0a, 0x04, b'a', b'b', b'c', b'd'],
            vec![0x00], vec![0x0f], vec![0x15, 0x01], vec![0x08, 0x01],
            pb_bytes(1, &[0x08, 0x01]), pb_bytes(1, &[0x0a, 0x01, 0xff]),
            pb_bytes(1, &pb_bytes(2, b"no path")), pb_bytes(1, &pb_bytes(1, b"")),
            pb_bytes(1, &duplicate), overflowing,
        ] {
            let mut data = pb_entry("/system/framework/framework.jar");
            data.extend(&bad);
            assert!(jar_paths_in_bootclasspath_pb(&data).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn collects_fragments_from_every_apex_in_stable_order() {
        let fixture = Fixture::android17();
        fixture.write("system/apex/packed.apex", b"not an expanded APEX directory");
        for (apex, jar) in [
            ("com.android.tethering", "framework-connectivity.jar"),
            ("com.android.conscrypt", "conscrypt.jar"),
        ] {
            let path = format!("/apex/{apex}/javalib/{jar}");
            fixture.write(&format!("system/apex/{apex}/javalib/{jar}"), b"jar");
            fixture.fragment(&format!("system/apex/{apex}/etc/classpaths/bootclasspath.pb"), &[&path, &path]);
        }
        let found = apex_bootclasspath_fragments(&fixture.0.join("system")).unwrap();
        assert_eq!(found, vec![
            "/apex/com.android.conscrypt/javalib/conscrypt.jar",
            "/apex/com.android.tethering/javalib/framework-connectivity.jar",
        ]);
        let layout = Layout::detect(&fixture.0).unwrap();
        assert_eq!(&layout.boot_locations[7..], found);
    }

    #[test]
    fn defaults_only_missing_app_target_sdk_to_37() {
        assert_eq!(app_target_sdk(None), 37);
        for target in [1, 23, 33, 35, 36, 37, 38, 10_000] {
            assert_eq!(app_target_sdk(Some(target)), target as i32);
        }
    }

    #[test]
    fn native_bridge_guest_root_is_separate_explicit_and_absolute() {
        let host = Fixture::android17();
        let guest = Fixture::empty();
        assert_eq!(bridge_guest_root(&host.0, None).unwrap(), host.0);
        assert_eq!(bridge_guest_root(&host.0, Some(guest.0.as_os_str())).unwrap(), std::fs::canonicalize(&guest.0).unwrap());
        for path in ["", "/", "relative/guest", "/pengudroid-nonexistent-guest-root"] {
            assert!(bridge_guest_root(&host.0, Some(std::ffi::OsStr::new(path))).is_err(), "{path}");
        }
        guest.write("not-a-directory", b"file");
        assert!(bridge_guest_root(&host.0, Some(guest.0.join("not-a-directory").as_os_str())).is_err());
    }

    #[test]
    fn legacy_and_staged_only_guest_roots_remain_refused() {
        let guest = Fixture::empty();
        for body in [b"{\"schema\":1}".as_slice(), b"{\"schema\":2,\"bridge_compatibility\":{\"state\":\"staged-only\"}}", b"not valid JSON"] {
            guest.write(GUEST_STAGING_MANIFEST, body);
            assert!(validate_bridge_guest(&guest.0).unwrap_err().contains("staged-only"));
        }
    }

    /// `NativeBridgeGetSignalHandler` yields a callback, not a sigaction. A
    /// non-null callback must never be installed without a real chain adapter,
    /// and an adapter failure must be reported instead of logged.
    #[test]
    fn bridge_signal_chain_fails_closed_without_a_safe_adapter() {
        unsafe extern "C" fn callback(_: c_int, _: *mut libc::siginfo_t, _: *mut c_void) -> bool {
            true
        }
        unsafe extern "C" fn get(sig: c_int) -> Option<BridgeSignalHandler> {
            (sig == libc::SIGSEGV).then_some(callback)
        }
        assert!(chain_bridge_handlers(get, None).is_err());
    }

    #[test]
    fn bridge_signal_chain_installs_only_non_null_callbacks_and_propagates_failures() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        static INSTALLED: AtomicUsize = AtomicUsize::new(0);
        static FAIL: AtomicBool = AtomicBool::new(false);
        static SAW_CALLBACK: AtomicBool = AtomicBool::new(false);
        unsafe extern "C" fn callback(_: c_int, _: *mut libc::siginfo_t, _: *mut c_void) -> bool {
            SAW_CALLBACK.store(true, Ordering::SeqCst);
            true
        }
        unsafe extern "C" fn get(sig: c_int) -> Option<BridgeSignalHandler> {
            (sig == libc::SIGBUS).then_some(callback)
        }
        unsafe extern "C" fn install(sig: c_int, cb: Option<BridgeSignalHandler>) -> c_int {
            assert_eq!(sig, libc::SIGBUS);
            assert!(cb.is_some());
            INSTALLED.fetch_add(1, Ordering::SeqCst);
            if FAIL.load(Ordering::SeqCst) { -1 } else { 0 }
        }
        unsafe extern "C" fn no_handlers(_: c_int) -> Option<BridgeSignalHandler> {
            None
        }

        assert!(chain_bridge_handlers(no_handlers, None).is_ok());
        assert_eq!(INSTALLED.load(Ordering::SeqCst), 0);
        assert!(chain_bridge_handlers(get, Some(install)).is_ok());
        assert_eq!(INSTALLED.load(Ordering::SeqCst), 1);
        FAIL.store(true, Ordering::SeqCst);
        assert!(chain_bridge_handlers(get, Some(install)).is_err());
        assert_eq!(INSTALLED.load(Ordering::SeqCst), 2);
    }
}
