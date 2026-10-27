//! Android system properties (`ro.*`, `persist.*`, ...).
//!
//! The daemon is the single source of truth; guests fetch properties over the
//! control socket (the `LD_PRELOAD` shim also carries a small cache for
//! pre-connection use). Values are seeded with Android 17 defaults so that
//! `Build.VERSION.SDK_INT` and friends report what guests expect.

use std::collections::BTreeMap;
use std::path::Path;
use pengudroid_proto::abi::{validate_platform_property_override, ANDROID_API_LEVEL, ANDROID_RELEASE};

/// Default properties advertised to guests. Runtime identity is immutable;
/// ordinary configuration properties remain writable.
pub fn defaults() -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    let pairs = [
        ("ro.build.version.sdk", ANDROID_API_LEVEL.to_string()),
        ("ro.build.version.sdk_full", format!("{ANDROID_API_LEVEL}.0")),
        ("ro.build.version.release", ANDROID_RELEASE.to_string()),
        ("ro.build.version.release_or_codename", ANDROID_RELEASE.to_string()),
        ("ro.build.version.release_or_preview_display", ANDROID_RELEASE.to_string()),
        ("ro.product.cpu.abi", "x86_64".to_string()),
        ("ro.product.cpu.abilist", "x86_64".to_string()),
        ("ro.product.cpu.abilist64", "x86_64".to_string()),
        ("ro.product.cpu.abilist32", String::new()),
        ("ro.build.version.codename", "REL".to_string()),
        ("ro.build.version.security_patch", "2025-06-05".to_string()),
        ("ro.product.model", "PenguDroid".to_string()),
        ("ro.product.manufacturer", "PenguDroid".to_string()),
        ("ro.product.name", "pengudroid".to_string()),
        ("ro.product.device", "pengudroid".to_string()),
        ("ro.debuggable", "1".to_string()),
        ("ro.secure", "0".to_string()),
        ("ro.kernel.qemu", "0".to_string()),
        ("ro.hardware", "pengudroid".to_string()),
        ("ro.build.characteristics", "default".to_string()),
        ("persist.sys.timezone", "UTC".to_string()),
    ];
    for (k, v) in pairs {
        m.insert(k.to_string(), v);
    }
    // Environment overrides: PENGUDROID_PROP_<name> where <name> cannot contain
    // dots, so this suits simple names like `PENGUDROID_PROP_debuggable`.
    for (k, v) in std::env::vars() {
        if let Some(name) = k.strip_prefix("PENGUDROID_PROP_") {
            validate_platform_property_override(name, &v, m.get(name).map(String::as_str))
                .unwrap_or_else(|error| panic!("invalid environment override {k}: {error}"));
            m.insert(name.to_string(), v);
        }
    }
    m
}

#[derive(Debug, Default)]
pub struct Props {
    map: BTreeMap<String, String>,
    /// NDK `__system_property_serial` is a monotonic counter; we expose the
    /// number of mutations.
    serial: u32,
}

impl Props {
    pub fn new() -> Props {
        Props { map: defaults(), serial: 0 }
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.map.get(name).map(|s| s.as_str())
    }

    pub fn set(&mut self, name: &str, value: &str) -> Result<(), String> {
        validate_platform_property_override(name, value, self.get(name))?;
        self.map.insert(name.to_string(), value.to_string());
        self.serial = self.serial.wrapping_add(1);
        Ok(())
    }

    pub fn serial(&self) -> u32 {
        self.serial
    }

    /// Every property, ordered by name. Used by `pengudroid getprop` with no
    /// argument and by the shim's cache warm-up.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        self.map.iter()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Best-effort persistence so `setprop` survives a daemon restart.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let mut out = String::new();
        for (k, v) in &self.map {
            out.push_str(k);
            out.push('=');
            out.push_str(v);
            out.push('\n');
        }
        std::fs::write(path, out)
    }

    /// Merge persisted values over the defaults.
    pub fn load_into(&mut self, path: &Path) {
        if let Ok(text) = std::fs::read_to_string(path) {
            for line in text.lines() {
                if let Some((k, v)) = line.split_once('=') {
                    match validate_platform_property_override(k, v, self.get(k)) {
                        Ok(()) => { self.map.insert(k.to_string(), v.to_string()); }
                        Err(error) => crate::warn!("{}: rejected persisted property: {error}", path.display()),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdk_is_android_17_x86_64() {
        let p = Props::new();
        assert_eq!(p.get("ro.build.version.sdk"), Some("37"));
        assert_eq!(p.get("ro.build.version.sdk_full"), Some("37.0"));
        assert_eq!(p.get("ro.build.version.release"), Some("17"));
        assert_eq!(p.get("ro.product.cpu.abilist"), Some("x86_64"));
        assert_eq!(p.get("ro.product.cpu.abilist32"), Some(""));
    }

    #[test]
    fn platform_cannot_be_downgraded_by_a_property_write() {
        let mut p = Props::new();
        for (name, value) in [
            ("ro.build.version.sdk", "33"), ("ro.build.version.release", "16"),
            ("ro.product.cpu.abilist32", "x86"),
            ("ro.appcompat_override.ro.build.version.sdk", "33"),
        ] {
            let before = p.serial();
            assert!(p.set(name, value).is_err());
            assert_eq!(p.serial(), before);
        }
        assert_eq!(p.get("ro.build.version.sdk"), Some("37"));
    }

    #[test]
    fn old_persistence_cannot_replace_the_runtime_identity() {
        let dir = std::env::temp_dir().join(format!("pd-platform-props-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("props");
        std::fs::write(&path, "ro.build.version.sdk=33\nro.build.version.release=13\nro.product.cpu.abilist32=x86\nro.appcompat_override.ro.build.version.sdk=33\npersist.example=restored\n").unwrap();
        let mut p = Props::new();
        p.load_into(&path);
        assert_eq!(p.get("ro.build.version.sdk"), Some("37"));
        assert_eq!(p.get("ro.build.version.release"), Some("17"));
        assert_eq!(p.get("ro.product.cpu.abilist32"), Some(""));
        assert_eq!(p.get("ro.appcompat_override.ro.build.version.sdk"), None);
        assert_eq!(p.get("persist.example"), Some("restored"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn set_bumps_serial() {
        let mut p = Props::new();
        let before = p.serial();
        p.set("persist.test", "1").unwrap();
        assert_eq!(p.get("persist.test"), Some("1"));
        assert_eq!(p.serial(), before + 1);
    }
}
