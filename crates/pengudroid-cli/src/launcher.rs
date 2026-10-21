//! `pengudroid launch` — a Proton-style front end.
//!
//! A "prefix" is a per-application directory that holds the app's writable
//! state and its environment overrides, exactly like Proton's `compatdata`.
//! Launching picks the best backend for the APK:
//!
//! * **native** — an x86_64 `NativeActivity` app. Its own `libmain.so` runs on
//!   the host GPU through the shim, no Java runtime involved.
//! * **art** — a Java application. ART is started from an extracted Android
//!   system tree and the APK's DEX files are put on the system class path.
//!
//! The choice does not depend on the graphics vendor: everything goes through
//! the host's EGL/Vulkan, so NVIDIA, AMD and Intel drivers are handled the same.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::apk::{self, ApkInfo};

/// Which runtime to use.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Backend {
    Native,
    Art,
}

impl Backend {
    fn name(self) -> &'static str {
        match self {
            Backend::Native => "native",
            Backend::Art => "art",
        }
    }
}

/// Why a backend was chosen, for the launch banner.
pub struct Plan {
    pub backend: Backend,
    pub reason: String,
}

/// Decide which backend can actually run `apk`.
///
/// `native` wins whenever the APK ships an x86_64 `NativeActivity` payload,
/// because that path needs no Java runtime at all. Otherwise the presence of
/// `classes.dex` means ART is the only option.
pub fn plan(apk_path: &str) -> Result<(ApkInfo, Plan), String> {
    let path = std::path::Path::new(apk_path);
    if crate::bundle::is_bundle(path) {
        return plan_bundle(path);
    }
    let info = apk::read_apk(apk_path)?;
    let entries = apk::apk_entries(apk_path).unwrap_or_default();
    let has_x86_64_lib = entries
        .iter()
        .any(|e| e.starts_with("lib/x86_64/") && e.ends_with(".so"));
    let has_dex = entries
        .iter()
        .any(|e| e.ends_with(".dex") || e == "classes.dex");
    let has_native_activity = entries
        .iter()
        .any(|e| e.starts_with("lib/x86_64/") && e.ends_with("libmain.so"));

    if has_native_activity {
        Ok((
            info,
            Plan {
                backend: Backend::Native,
                reason: "x86_64 NativeActivity payload (libmain.so)".into(),
            },
        ))
    } else if has_dex {
        // Native libraries alone do not make an app runnable without ART: the
        // Java entry point still has to exist. Only a NativeActivity payload
        // (`lib/<abi>/libmain.so` + the NativeActivity manifest entry) can skip
        // the Java runtime.
        Ok((
            info,
            Plan {
                backend: Backend::Art,
                reason: if has_x86_64_lib {
                    "x86_64 libraries but no NativeActivity entry point".into()
                } else {
                    "DEX only, needs the Android runtime".into()
                },
            },
        ))
    } else {
        Err(format!(
            "{apk_path} has neither an x86_64 native payload nor classes.dex"
        ))
    }
}

/// Decide a backend for a bundle, whose libraries live in an ABI split.
fn plan_bundle(path: &std::path::Path) -> Result<(ApkInfo, Plan), String> {
    let b = crate::bundle::Bundle::open(path)?;
    let info = b.info()?;
    let host = crate::nativelib::host_abi();
    let libs = b.native_libs()?;
    let host_lib = libs.iter().any(|(_, l)| l.abi == host);
    let host_native_activity = libs
        .iter()
        .any(|(_, l)| l.abi == host && l.file == "libmain.so");
    let has_dex = b
        .base_bytes()
        .ok()
        .and_then(|d| apk::extract_named_bytes(&d, "classes.dex").ok())
        .is_some();

    if host_native_activity {
        Ok((
            info,
            Plan {
                backend: Backend::Native,
                reason: format!("bundle ships a {host} NativeActivity payload"),
            },
        ))
    } else if !b.abis.is_empty() && !host_lib {
        Ok((
            info,
            Plan {
                backend: Backend::Art,
                reason: format!(
                    "bundle is {}-only; ART plus an ARM translator is required",
                    b.abis.join("/")
                ),
            },
        ))
    } else if has_dex {
        Ok((
            info,
            Plan {
                backend: Backend::Art,
                reason: if host_lib {
                    format!("bundle has {host} libraries but no NativeActivity entry point")
                } else {
                    "DEX only, needs the Android runtime".into()
                },
            },
        ))
    } else {
        Err(format!(
            "{}: bundle has neither native libraries nor classes.dex",
            path.display()
        ))
    }
}

/// The default prefix for a package: `$XDG_DATA_HOME/pengudroid/prefixes/<pkg>`.
pub fn default_prefix(package: &str) -> PathBuf {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    data.join("pengudroid/prefixes").join(package)
}

/// Create the prefix layout and return the environment it defines.
///
/// The layout mirrors what an app expects to be writable: `files`, `cache`,
/// `code_cache` and a `shared_prefs` directory, plus `pengudroid.env` for the
/// user's own overrides (`KEY=VALUE`, `#` comments).
pub fn create_prefix(prefix: &Path, package: &str) -> Result<Vec<(String, String)>, String> {
    for sub in ["files", "cache", "code_cache", "shared_prefs"] {
        let dir = prefix.join(sub);
        std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let env_file = prefix.join("pengudroid.env");
    if !env_file.exists() {
        let mut f = std::fs::File::create(&env_file)
            .map_err(|e| format!("cannot create {}: {e}", env_file.display()))?;
        let _ = writeln!(
            f,
            "# PenguDroid environment overrides for {package}\n\
             # One KEY=VALUE per line; lines starting with '#' are ignored.\n\
             # Example:\n\
             #   PENGUDROID_WINDOW_WIDTH=1600\n"
        );
    }
    let mut out = vec![
        ("ANDROID_DATA".to_string(), prefix.display().to_string()),
        (
            "PENGUDROID_PREFIX".to_string(),
            prefix.display().to_string(),
        ),
        ("HOME".to_string(), prefix.display().to_string()),
    ];
    let text = std::fs::read_to_string(&env_file).unwrap_or_default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            out.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    Ok(out)
}

/// The ART sysroot to use: `--sysroot`, else `$PENGUDROID_ART_SYSROOT`.
fn art_sysroot(explicit: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(PathBuf::from(p));
    }
    std::env::var_os("PENGUDROID_ART_SYSROOT").map(PathBuf::from)
}

/// `pengudroid launch <app.apk> [options]`
pub fn launch(args: &[String]) -> i32 {
    let mut apk_path: Option<String> = None;
    let mut prefix_arg: Option<String> = None;
    let mut sysroot_arg: Option<String> = None;
    let mut backend_arg: Option<String> = None;
    let mut component: Option<String> = None;
    let mut app_args: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--prefix" => prefix_arg = it.next().cloned(),
            "--sysroot" => sysroot_arg = it.next().cloned(),
            "--backend" => backend_arg = it.next().cloned(),
            "--component" => component = it.next().cloned(),
            "--" => {
                app_args.extend(it.by_ref().cloned());
                break;
            }
            other if other.starts_with('-') => {
                eprintln!("pengudroid launch: unknown option {other}");
                return 2;
            }
            other => apk_path = Some(other.to_string()),
        }
    }
    let Some(apk_path) = apk_path else {
        eprintln!("usage: pengudroid launch <app.apk> [--backend auto|native|art] [--prefix DIR] [--sysroot DIR] [-- ARGS]");
        return 2;
    };

    let (info, planned) = match plan(&apk_path) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("pengudroid launch: {e}");
            return 1;
        }
    };
    let backend = match backend_arg.as_deref() {
        None | Some("auto") => planned.backend,
        Some("native") => Backend::Native,
        Some("art") => Backend::Art,
        Some(other) => {
            eprintln!("pengudroid launch: unknown backend {other}");
            return 2;
        }
    };

    let prefix = prefix_arg
        .map(PathBuf::from)
        .unwrap_or_else(|| default_prefix(&info.package));
    let env = match create_prefix(&prefix, &info.package) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("pengudroid launch: {e}");
            return 1;
        }
    };

    // A bundle is materialised into the prefix once: `base.apk`, the split for
    // the architecture we intend to use, and the OBB payload. Everything after
    // this point works on an ordinary APK path.
    let mut app_path = apk_path.clone();
    if crate::bundle::is_bundle(std::path::Path::new(&apk_path)) {
        match crate::bundle::Bundle::open(std::path::Path::new(&apk_path)) {
            Ok(b) => {
                let dir = prefix.join("bundle");
                let abi = bundle_abi(&b, backend);
                match b.extract_to(&dir, abi.as_deref()) {
                    Ok(written) => {
                        println!("  bundle  : {} ({} members -> {})", b.kind.name(), written.len(), dir.display());
                        if !b.obb.is_empty() {
                            println!("  obb     : {} file(s)", b.obb.len());
                        }
                        if let Some(base) = written.first() {
                            app_path = base.display().to_string();
                        }
                    }
                    Err(e) => {
                        eprintln!("pengudroid launch: {e}");
                        return 1;
                    }
                }
            }
            Err(e) => {
                eprintln!("pengudroid launch: {e}");
                return 1;
            }
        }
    }

    println!("PenguDroid launch");
    println!("  package : {}", info.package);
    if let Some(v) = &info.version_name {
        println!("  version : {v}");
    }
    println!("  apk     : {apk_path}");
    println!("  prefix  : {}", prefix.display());
    println!("  backend : {} ({})", backend.name(), planned.reason);
    if backend != planned.backend {
        println!("  note    : forced with --backend");
    }
    let _ = std::io::Write::flush(&mut std::io::stdout());

    match backend {
        Backend::Native => {
            let mut cmd = std::process::Command::new(std::env::current_exe().unwrap_or_default());
            cmd.arg("native").arg(&app_path);
            if let Some(c) = &component {
                cmd.arg("--component").arg(c);
            }
            apply_env(&mut cmd, &env);
            run_child(cmd)
        }
        Backend::Art => {
            let Some(sysroot) = art_sysroot(sysroot_arg.as_deref()) else {
                eprintln!(
                    "pengudroid launch: this app needs ART; pass --sysroot <dir> or set \
                     PENGUDROID_ART_SYSROOT (see `pengudroid sysroot`)"
                );
                return 1;
            };
            let mut cmd = std::process::Command::new(std::env::current_exe().unwrap_or_default());
            cmd.arg("art-test")
                .arg("--sysroot")
                .arg(&sysroot)
                .arg("--classpath")
                .arg(&app_path)
                // The framework's JNI natives live in libandroid_runtime.so and
                // are registered by calling every exported `register_*` entry
                // point, which is what app_process does. libhwui.so registers the
                // renderer's natives (`ThreadedRenderer`, `RenderNode`), without
                // which hardware drawing cannot start.
                .arg("--register-natives")
                .arg("libandroid_runtime.so,libhwui.so");
            let main = component.clone().unwrap_or_else(|| {
                // An Activity has no `main(String[])`; the closest honest entry
                // point is the app_process one, `ActivityThread.main`, which is
                // what a device's zygote calls for every app.
                "android.app.ActivityThread".to_string()
            });
            // With `--bind-app` the host plays the part `system_server` plays on
            // a device: it calls `handleBindApplication` and then
            // `handleLaunchActivity` for the manifest's launcher activity. That
            // is the only path that reaches `ViewRootImpl.performTraversals` and
            // therefore the only one that draws.
            let activity = info.main_activity_name().map(|s| s.to_string());
            if let Some(activity) = &activity {
                cmd.arg("--bind-app");
                cmd.env("PENGUDROID_LAUNCH_ACTIVITY", activity);
                println!("  activity: {activity}");
            } else {
                println!(
                    "  note    : no launcher activity in the manifest; starting via \
                     ActivityThread.main"
                );
            }
            cmd.arg("--main").arg(main);
            // The JIT crashes in `nterp_op_invoke_virtual` on this framework
            // build, so the interpreter is the only working execution mode.
            cmd.arg("--vm-option").arg("-Xint");
            if !app_args.is_empty() {
                cmd.arg("--").args(&app_args);
            }
            apply_env(&mut cmd, &env);
            run_child(cmd)
        }
    }
}

/// Which ABI split to keep for the chosen backend: the host ABI for a native
/// run, and for ART the architecture the translator can handle best (arm64
/// before armv7, because 32-bit translation needs a 32-bit host process).
fn bundle_abi(b: &crate::bundle::Bundle, backend: Backend) -> Option<String> {
    if b.abis.is_empty() {
        return None;
    }
    match backend {
        Backend::Native => {
            let host = crate::nativelib::host_abi();
            if b.abis.iter().any(|a| a == host) {
                Some(host.to_string())
            } else {
                Some(b.abis[0].clone())
            }
        }
        Backend::Art => {
            if b.abis.iter().any(|a| a == "arm64-v8a") {
                Some("arm64-v8a".to_string())
            } else {
                b.abis.first().cloned()
            }
        }
    }
}

fn apply_env(cmd: &mut std::process::Command, env: &[(String, String)]) {
    for (k, v) in env {
        cmd.env(k, v);
    }
}

fn run_child(mut cmd: std::process::Command) -> i32 {
    match cmd.status() {
        Ok(s) if s.success() => 0,
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("pengudroid launch: cannot start the guest: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_prefix_is_per_package() {
        let p = default_prefix("com.example.game");
        assert!(p.ends_with("com.example.game"));
        assert!(p.to_string_lossy().contains("pengudroid/prefixes"));
    }

    #[test]
    fn prefix_layout_and_env_overrides() {
        let dir = std::env::temp_dir().join(format!("pd-prefix-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let env = create_prefix(&dir, "com.example").unwrap();
        for sub in ["files", "cache", "code_cache", "shared_prefs"] {
            assert!(dir.join(sub).is_dir(), "{sub} missing");
        }
        assert!(env.iter().any(|(k, _)| k == "ANDROID_DATA"));
        assert!(dir.join("pengudroid.env").is_file());

        std::fs::write(
            dir.join("pengudroid.env"),
            "# comment\n\nPENGUDROID_WINDOW_WIDTH=1600\nFOO=bar\n",
        )
        .unwrap();
        let env = create_prefix(&dir, "com.example").unwrap();
        assert!(env.contains(&("PENGUDROID_WINDOW_WIDTH".into(), "1600".into())));
        assert!(env.contains(&("FOO".into(), "bar".into())));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backend_names_are_stable() {
        assert_eq!(Backend::Native.name(), "native");
        assert_eq!(Backend::Art.name(), "art");
    }
}
