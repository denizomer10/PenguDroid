//! Android system properties (`ro.*`, `persist.*`, ...).
//!
//! The daemon is the single source of truth; guests fetch properties over the
//! control socket (the `LD_PRELOAD` shim also carries a small cache for
//! pre-connection use). Values are seeded with Android 16 defaults so that
//! `Build.VERSION.SDK_INT` and friends report what guests expect.

use std::collections::BTreeMap;
use std::path::Path;

/// Default properties advertised to guests. `ro.` values are read-only by
/// convention; we allow writes but log them.
pub fn defaults() -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    let pairs = [
        ("ro.build.version.sdk", pengudroid_proto::abi::ANDROID_API_LEVEL.to_string()),
        ("ro.build.version.release", pengudroid_proto::abi::ANDROID_RELEASE.to_string()),
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

    pub fn set(&mut self, name: &str, value: &str) {
        self.map.insert(name.to_string(), value.to_string());
        self.serial = self.serial.wrapping_add(1);
    }

    pub fn serial(&self) -> u32 {
        self.serial
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
                    self.map.insert(k.to_string(), v.to_string());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdk_is_android_16() {
        let p = Props::new();
        assert_eq!(p.get("ro.build.version.sdk"), Some("36"));
        assert_eq!(p.get("ro.build.version.release"), Some("16"));
    }

    #[test]
    fn set_bumps_serial() {
        let mut p = Props::new();
        let before = p.serial();
        p.set("persist.test", "1");
        assert_eq!(p.get("persist.test"), Some("1"));
        assert_eq!(p.serial(), before + 1);
    }
}
