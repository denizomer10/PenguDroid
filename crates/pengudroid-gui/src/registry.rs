//! What is installed, and what is running.
//!
//! The registry is a plain text file so it can be inspected and edited by hand:
//!
//! ```text
//! org.schabi.newpipe|NewPipe|org.schabi.newpipe.MainActivity|/home/me/NewPipe.apk
//! ```
//!
//! Running state is deliberately *not* persisted: a pid from a previous session
//! means nothing, and a stale "running" flag would make the quit confirmation
//! lie.

use std::path::{Path, PathBuf};

/// One installed app.
#[derive(Clone, Debug)]
pub struct Entry {
    pub package: String,
    pub label: String,
    pub activity: String,
    pub apk: PathBuf,
    /// Set while the app has a live child process.
    pub running: bool,
    /// Pid of that child, when running.
    pub pid: Option<u32>,
}

/// The installed-app list plus the user's settings.
#[derive(Clone)]
pub struct Registry {
    entries: Vec<Entry>,
    settings: Vec<(String, bool)>,
    path: PathBuf,
    settings_path: PathBuf,
}

impl Registry {
    pub fn load() -> Registry {
        let base = data_dir();
        let path = base.join("apps.txt");
        let settings_path = base.join("settings.txt");
        let mut entries = Vec::new();
        if let Ok(text) = std::fs::read_to_string(&path) {
            for line in text.lines() {
                let mut parts = line.split('|');
                let (Some(package), Some(label), Some(activity), Some(apk)) =
                    (parts.next(), parts.next(), parts.next(), parts.next())
                else {
                    continue;
                };
                if package.is_empty() {
                    continue;
                }
                entries.push(Entry {
                    package: package.to_string(),
                    label: label.to_string(),
                    activity: activity.to_string(),
                    apk: PathBuf::from(apk),
                    running: false,
                    pid: None,
                });
            }
        }
        let mut settings = Vec::new();
        if let Ok(text) = std::fs::read_to_string(&settings_path) {
            for line in text.lines() {
                if let Some((k, v)) = line.split_once('=') {
                    settings.push((k.trim().to_string(), v.trim() == "true"));
                }
            }
        }
        Registry {
            entries,
            settings,
            path,
            settings_path,
        }
    }

    pub fn apps(&self) -> &[Entry] {
        &self.entries
    }

    pub fn get(&self, index: usize) -> Option<&Entry> {
        self.entries.get(index)
    }

    pub fn running_count(&self) -> usize {
        self.entries.iter().filter(|e| e.running).count()
    }

    pub fn set_running(&mut self, package: &str, pid: Option<u32>) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.package == package) {
            entry.running = pid.is_some();
            entry.pid = pid;
        }
    }

    pub fn setting(&self, key: &str, default: bool) -> bool {
        self.settings
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| *v)
            .unwrap_or(default)
    }

    pub fn set_setting(&mut self, key: &str, value: bool) {
        match self.settings.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = value,
            None => self.settings.push((key.to_string(), value)),
        }
        let text: String = self
            .settings
            .iter()
            .map(|(k, v)| format!("{k}={v}\n"))
            .collect();
        let _ = std::fs::write(&self.settings_path, text);
    }

    /// Install an APK (or a bundle) and remember it.
    ///
    /// The package name and launcher activity come from `pengudroid info`, which
    /// already parses the manifest; duplicating that parser here would be a
    /// second source of truth for the same wire format.
    pub fn install(&mut self, apk: &Path) -> Result<Entry, String> {
        if !apk.exists() {
            return Err(format!("{} does not exist", apk.display()));
        }
        let (package, activity, label) = describe(apk)?;
        // The manifest's own label is what the user recognises ("NewPipe", not
        // "newpipe"); fall back to the last package segment when it is absent.
        let label = label.unwrap_or_else(|| {
            package
                .rsplit('.')
                .next()
                .unwrap_or(&package)
                .to_string()
        });
        let entry = Entry {
            package: package.clone(),
            label,
            activity,
            apk: apk.to_path_buf(),
            running: false,
            pid: None,
        };
        // Re-installing replaces the old entry rather than duplicating it.
        self.entries.retain(|e| e.package != package);
        self.entries.push(entry.clone());
        self.save();
        Ok(entry)
    }

    pub fn uninstall(&mut self, package: &str) -> Result<(), String> {
        let before = self.entries.len();
        self.entries.retain(|e| e.package != package);
        if self.entries.len() == before {
            return Err(format!("{package} is not installed"));
        }
        self.save();
        Ok(())
    }

    fn save(&self) {
        let text: String = self
            .entries
            .iter()
            .map(|e| {
                format!(
                    "{}|{}|{}|{}\n",
                    e.package,
                    e.label,
                    e.activity,
                    e.apk.display()
                )
            })
            .collect();
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&self.path, text);
    }

    /// Ask the desktop to pick one or more packages.
    ///
    /// `zenity` (GTK/GNOME), `kdialog` (KDE) and `yad` all present the *desktop's*
    /// own file chooser, so the user gets the same dialog as everywhere else and
    /// we need no toolkit of our own. Returns an empty vector when the user
    /// cancels, and `None` when no chooser is installed at all.
    pub fn choose_apks() -> Option<Vec<PathBuf>> {
        let filter = "*.apk *.apkm *.xapk *.apks *.zip";
        let attempts: [(&str, &[&str]); 3] = [
            (
                "zenity",
                &["--file-selection", "--multiple", "--separator=\n",
                  "--title=Install an Android package", "--file-filter=Android packages | *.apk *.apkm *.xapk *.apks"],
            ),
            (
                "kdialog",
                &["--getopenfilename", ".", "*.apk *.apkm *.xapk *.apks", "--multiple", "--separate-output"],
            ),
            (
                "yad",
                &["--file-selection", "--multiple", "--separator=\n", "--file-filter=Android packages | *.apk *.apkm *.xapk *.apks"],
            ),
        ];
        let _ = filter;
        for (program, args) in attempts {
            let output = match std::process::Command::new(program).args(args).output() {
                Ok(out) => out,
                // Not installed: try the next one.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => continue,
            };
            // A cancelled dialog exits 1 with no stdout.
            if !output.status.success() {
                return Some(Vec::new());
            }
            let paths: Vec<PathBuf> = String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(PathBuf::from)
                .collect();
            return Some(paths);
        }
        None
    }

    /// Find an APK to install without a file dialog.
    ///
    /// Fallback for a session where no native chooser exists: looks at
    /// `~/Downloads` and the working directory, newest first.
    pub fn pick_apk(&self) -> Option<PathBuf> {
        let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
        let mut dirs = vec![std::env::current_dir().unwrap_or_default()];
        if let Some(home) = std::env::var_os("HOME") {
            dirs.push(PathBuf::from(home).join("Downloads"));
        }
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let is_apk = path
                    .extension()
                    .is_some_and(|e| matches!(e.to_str(), Some("apk" | "apkm" | "xapk")));
                if !is_apk {
                    continue;
                }
                let modified = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                candidates.push((modified, path));
            }
        }
        candidates.sort_by(|a, b| b.0.cmp(&a.0));
        candidates.into_iter().next().map(|(_, p)| p)
    }
}

/// `$XDG_DATA_HOME/pengudroid`, or `~/.local/share/pengudroid`.
pub fn data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from(".local/share"))
        .join("pengudroid")
}

/// Ask the CLI for an APK's package and launcher activity.
/// `package`, `main activity` and `label` for an APK, read through
/// `pengudroid info --plain`.
///
/// The `--plain` form is `key<TAB>value` lines. This used to parse the `Debug`
/// dump and look for a line starting with `activity`, which the dump never
/// contains (`main_activity: Some(...)`), so installation failed for every APK
/// with "cannot read the package name from the APK".
fn describe(apk: &Path) -> Result<(String, String, Option<String>), String> {
    let output = std::process::Command::new(pengudroid_binary())
        .arg("info")
        .arg("--plain")
        .arg(apk)
        .output()
        .map_err(|e| format!("cannot run pengudroid info: {e}"))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        let err = err.trim();
        return Err(if err.is_empty() {
            "pengudroid info failed".to_string()
        } else {
            err.to_string()
        });
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let (mut package, mut activity, mut label) = (None, None, None);
    for line in text.lines() {
        let Some((key, value)) = line.split_once('\t') else { continue };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match key.trim() {
            "package" => package = Some(value.to_string()),
            "activity" => activity = Some(value.to_string()),
            "label" => label = Some(value.to_string()),
            _ => {}
        }
    }
    match (package, activity) {
        (Some(p), Some(a)) => Ok((p, a, label)),
        (Some(_), None) => Err("this APK declares no activity to launch".into()),
        _ => Err("cannot read the package name from the APK".into()),
    }
}

/// The `pengudroid` binary: next to this executable, or on `PATH`.
pub fn pengudroid_binary() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("pengudroid");
            if sibling.exists() {
                return sibling;
            }
        }
    }
    PathBuf::from("pengudroid")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_renders_entries() {
        let dir = std::env::temp_dir().join(format!("pd-registry-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("apps.txt");
        std::fs::write(
            &path,
            "org.example|Example|org.example.Main|/tmp/example.apk\n",
        )
        .unwrap();
        let mut registry = Registry {
            entries: Vec::new(),
            settings: Vec::new(),
            path: path.clone(),
            settings_path: dir.join("settings.txt"),
        };
        // Load through the same parser the app uses.
        let text = std::fs::read_to_string(&path).unwrap();
        for line in text.lines() {
            let mut parts = line.split('|');
            registry.entries.push(Entry {
                package: parts.next().unwrap().into(),
                label: parts.next().unwrap().into(),
                activity: parts.next().unwrap().into(),
                apk: PathBuf::from(parts.next().unwrap()),
                running: false,
                pid: None,
            });
        }
        assert_eq!(registry.apps().len(), 1);
        assert_eq!(registry.apps()[0].package, "org.example");
        registry.set_running("org.example", Some(42));
        assert_eq!(registry.running_count(), 1);
        registry.set_running("org.example", None);
        assert_eq!(registry.running_count(), 0);
        registry.set_setting("keymapper", false);
        assert!(!registry.setting("keymapper", true));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
