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

    if has_x86_64_lib && (has_native_activity || has_dex) {
        Ok((
            info,
            Plan {
                backend: Backend::Native,
                reason: if has_native_activity {
                    "x86_64 NativeActivity payload (libmain.so)".into()
                } else {
                    "x86_64 native libraries".into()
                },
            },
        ))
    } else if has_dex {
        Ok((
            info,
            Plan {
                backend: Backend::Art,
                reason: "DEX only, needs the Android runtime".into(),
            },
        ))
    } else {
        Err(format!(
            "{apk_path} has neither an x86_64 native payload nor classes.dex"
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
            cmd.arg("native").arg(&apk_path);
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
                .arg(&apk_path)
                // The framework's JNI natives live in libandroid_runtime.so and
                // are registered by calling every exported `register_*` entry
                // point, which is what app_process does.
                .arg("--register-natives")
                .arg("libandroid_runtime.so");
            let main = component
                .clone()
                .or_else(|| info.main_activity_name().map(str::to_string))
                .unwrap_or_else(|| "android.app.ActivityThread".to_string());
            cmd.arg("--main").arg(main);
            if !app_args.is_empty() {
                cmd.arg("--").args(&app_args);
            }
            apply_env(&mut cmd, &env);
            run_child(cmd)
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
