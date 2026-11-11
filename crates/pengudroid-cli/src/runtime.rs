//! Per-user Android runtime discovery and provisioning.
//!
//! Apps that need ART cannot run without Android's own runtime binaries, so the
//! extracted API-37 tree is a real dependency. What must not be a dependency is
//! *the user remembering where it is*: this module owns the one cache location
//! (`$XDG_CACHE_HOME/pengudroid`, else `~/.cache/pengudroid`), finds an
//! already-prepared tree there, and can prepare one on demand through
//! `tools/prepare-android17.py` — the same verified, offline extractor the
//! developers use. `pengudroid launch` and the desktop app both resolve the
//! runtime through this, so `--sysroot` is only ever needed to override it.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Directory name of the prepared runtime inside the cache.
pub const RUNTIME_DIR: &str = "android17-x86_64";
/// Cache directory name this project owns.
const CACHE_NAME: &str = "pengudroid";
/// Relative location of the preparation tool inside a checkout.
const PREPARE_SCRIPT: &str = "tools/prepare-android17.py";

fn non_empty(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// `$XDG_CACHE_HOME/pengudroid`, else `$HOME/.cache/pengudroid`.
pub fn cache_root() -> Option<PathBuf> {
    if let Some(dir) = non_empty("XDG_CACHE_HOME") {
        return Some(dir.join(CACHE_NAME));
    }
    non_empty("HOME").map(|home| home.join(".cache").join(CACHE_NAME))
}

/// The canonical prepared-runtime location, whether or not it exists yet.
pub fn cached_runtime() -> Option<PathBuf> {
    cache_root().map(|base| base.join("runtime").join(RUNTIME_DIR))
}

/// Where the runtime is, in priority order: explicit env overrides, then the
/// checkout the user is standing in, then the per-user cache.
pub fn locate() -> Option<PathBuf> {
    let existing = |path: PathBuf| path.is_dir().then_some(path);
    if let Some(explicit) = non_empty("PENGUDROID_ART_SYSROOT").and_then(&existing) {
        return Some(explicit);
    }
    if let Some(explicit) = non_empty("PENGUDROID_SYSROOT").and_then(&existing) {
        return Some(explicit);
    }
    if let Ok(cwd) = std::env::current_dir() {
        if let Some(tree) = existing(cwd.join(".toolchain/sysroot-android17")) {
            return Some(tree);
        }
    }
    cached_runtime().and_then(existing)
}

fn ancestors(start: &Path) -> impl Iterator<Item = PathBuf> + '_ {
    let mut current = Some(start);
    std::iter::from_fn(move || {
        let path = current?;
        current = path.parent();
        Some(path.to_path_buf())
    })
}

/// The preparation tool: `$PENGUDROID_PREPARE_SCRIPT`, else a checkout found by
/// walking up from the executable and from the working directory.
pub fn prepare_script() -> Option<PathBuf> {
    if let Some(explicit) = non_empty("PENGUDROID_PREPARE_SCRIPT") {
        return explicit.is_file().then_some(explicit);
    }
    let mut roots = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            roots.extend(ancestors(dir).take(4));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        roots.extend(ancestors(&cwd).take(4));
    }
    roots
        .into_iter()
        .map(|root| root.join(PREPARE_SCRIPT))
        .find(|script| script.is_file())
}

/// Locate an interpreter: `$PENGUDROID_PYTHON`, else `python3` on `PATH`.
fn python() -> String {
    non_empty("PENGUDROID_PYTHON")
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "python3".to_string())
}

/// What a provisioning run needs to know, separated from the environment so it
/// can be built and asserted without touching process globals.
pub struct Provisioning {
    pub script: PathBuf,
    pub python: String,
    pub cache_root: PathBuf,
    pub download: bool,
}

impl Provisioning {
    /// Resolve everything a provisioning run needs, or explain what is missing.
    pub fn resolve(download: bool) -> Result<Provisioning, String> {
        let script = prepare_script().ok_or_else(|| {
            format!(
                "cannot find {PREPARE_SCRIPT}; run from a PenguDroid checkout or set \
                 PENGUDROID_PREPARE_SCRIPT"
            )
        })?;
        let cache_root = cache_root().ok_or_else(|| {
            "cannot resolve the cache directory (HOME and XDG_CACHE_HOME are unset); pass \
             --sysroot <dir> explicitly"
                .to_string()
        })?;
        Ok(Provisioning { script, python: python(), cache_root, download })
    }

    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.python);
        command
            .arg("-B")
            .arg(&self.script)
            .arg("--cache")
            .arg("--output")
            .arg(self.cache_root.join("runtime").join(RUNTIME_DIR));
        if self.download {
            command.arg("--download");
        }
        command
    }

    /// Run the extractor; the caller's terminal carries its progress output.
    pub fn run(&self) -> Result<PathBuf, String> {
        let status = self
            .command()
            .status()
            .map_err(|e| format!("cannot run {}: {e}", self.python))?;
        if !status.success() {
            return Err(format!(
                "runtime preparation failed ({status}); the extractor's message above says why"
            ));
        }
        let runtime = self.cache_root.join("runtime").join(RUNTIME_DIR);
        if !runtime.is_dir() {
            return Err(format!("preparation reported success but {} is missing", runtime.display()));
        }
        Ok(runtime)
    }
}

/// Make sure a runtime exists, preparing (and optionally downloading) one if
/// the cache is empty. Returns the tree to run against.
pub fn provision(download: bool) -> Result<PathBuf, String> {
    if let Some(cached) = cached_runtime().filter(|p| p.is_dir()) {
        return Ok(cached);
    }
    Provisioning::resolve(download)?.run()
}

/// `pengudroid runtime [--status] [--download]`
pub fn runtime(args: &[String]) -> i32 {
    let mut download = false;
    let mut status_only = false;
    for arg in args {
        match arg.as_str() {
            "--download" => download = true,
            "--status" | "--check" => status_only = true,
            other if other.starts_with('-') => {
                eprintln!("pengudroid runtime: unknown option {other}");
                return 2;
            }
            other => {
                eprintln!("pengudroid runtime: unexpected argument {other}");
                return 2;
            }
        }
    }
    let cached = cached_runtime();
    if let Some(tree) = locate() {
        println!("runtime: {}", tree.display());
        if let Some(cached) = cached.as_ref().filter(|cached| **cached != tree) {
            println!("cache:   {}", cached.display());
        }
        println!("apps run without --sysroot");
        return 0;
    }
    println!("runtime: none found");
    if let Some(cached) = &cached {
        println!("cache:   {} (not prepared)", cached.display());
    }
    if status_only {
        return 1;
    }
    match provision(download) {
        Ok(tree) => {
            println!("runtime: {}", tree.display());
            println!("apps now launch without --sysroot");
            0
        }
        Err(error) => {
            eprintln!("pengudroid runtime: {error}");
            if !download {
                eprintln!("hint: `pengudroid runtime --download` fetches the pinned image once");
            }
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_root_prefers_xdg_then_home() {
        // Pure path helpers, asserted through the same env lookup the process uses.
        let root = cache_root();
        if let Some(xdg) = non_empty("XDG_CACHE_HOME") {
            assert_eq!(root, Some(xdg.join(CACHE_NAME)));
        } else if let Some(home) = non_empty("HOME") {
            assert_eq!(root, Some(home.join(".cache").join(CACHE_NAME)));
        }
        if let Some(cached) = cached_runtime() {
            assert!(cached.ends_with(format!("runtime/{RUNTIME_DIR}")));
        }
    }

    #[test]
    fn provisioning_uses_the_cache_and_only_downloads_when_asked() {
        let plan = Provisioning {
            script: PathBuf::from("/checkout/tools/prepare-android17.py"),
            python: "python3".into(),
            cache_root: PathBuf::from("/cache/pengudroid"),
            download: false,
        };
        let args: Vec<String> = plan
            .command()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "-B",
                "/checkout/tools/prepare-android17.py",
                "--cache",
                "--output",
                "/cache/pengudroid/runtime/android17-x86_64",
            ]
        );
        let download = Provisioning { download: true, ..plan };
        let args: Vec<String> = download
            .command()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args.last().map(String::as_str), Some("--download"));
    }

    #[test]
    fn ancestors_climb_out_of_the_binary_directory() {
        let seen: Vec<_> = ancestors(Path::new("/repo/target/debug")).map(|p| p.display().to_string()).collect();
        assert_eq!(seen, ["/repo/target/debug", "/repo/target", "/repo", "/"]);
    }
}
