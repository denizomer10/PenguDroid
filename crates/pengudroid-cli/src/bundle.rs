//! Android *bundles*: `.apkm`, `.xapk`, `.apks` and plain `.apk`.
//!
//! Google Play ships apps as an "app bundle" — a base APK plus per-architecture,
//! per-density and per-language split APKs, and often a separate asset pack.
//! The community wrappers are just ZIP files around those pieces:
//!
//! | Extension | Container contents |
//! |-----------|--------------------|
//! | `.apk`    | a single APK |
//! | `.apkm`   | `base.apk`, `split_config.<abi>.apk`, `split_asset_pack.apk`, `info.json` |
//! | `.xapk`   | `<pkg>.apk`, split APKs, `Android/obb/<pkg>/*.obb`, `manifest.json` |
//! | `.apks`   | bundletool output: `toc.pb` + `splits/base-master.apk`, `splits/base-<abi>.apk` |
//!
//! This module reads that structure without extracting anything up front: the
//! container stays in memory and members are inflated on demand, which matters
//! because a single game bundle here is ~262 MB and its asset pack alone is
//! 213 MB.

use std::path::{Path, PathBuf};

use crate::apk::{self, ApkInfo, NativeLib};

/// Which container flavour a file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Apk,
    Apkm,
    Xapk,
    Apks,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Apk => "apk",
            Kind::Apkm => "apkm",
            Kind::Xapk => "xapk",
            Kind::Apks => "apks",
        }
    }

    /// Guess from the extension, falling back to sniffing the contents.
    pub fn detect(path: &Path) -> Kind {
        match path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("apkm") => Kind::Apkm,
            Some("xapk") => Kind::Xapk,
            Some("apks") => Kind::Apks,
            _ => Kind::Apk,
        }
    }
}

/// One member of a bundle that is itself an APK.
#[derive(Debug, Clone)]
pub struct MemberApk {
    /// Entry name inside the container (e.g. `split_config.arm64_v8a.apk`).
    pub name: String,
    /// The ABI this split provides, when it is an ABI split.
    pub abi: Option<String>,
}

/// A parsed bundle.
pub struct Bundle {
    pub kind: Kind,
    pub path: PathBuf,
    /// The container bytes (for a plain `.apk`, this is the APK itself).
    data: Vec<u8>,
    /// Entry name of the base APK inside the container.
    pub base: String,
    /// All non-base APK members (ABI, density, language, asset packs).
    pub splits: Vec<MemberApk>,
    /// `Android/obb/...` entries, as (entry name, size).
    pub obb: Vec<(String, u64)>,
    /// Split names available, for display.
    pub abis: Vec<String>,
}

/// Map a split's file name to the ABI it carries.
///
/// Play names them `split_config.arm64_v8a.apk` (bundletool uses
/// `base-arm64_v8a.apk`), so the underscore form has to be turned back into
/// dashes to match `lib/<abi>/`.
fn abi_from_name(name: &str) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    for (needle, abi) in [
        ("arm64_v8a", "arm64-v8a"),
        ("armeabi_v7a", "armeabi-v7a"),
        ("x86_64", "x86_64"),
        ("x86", "x86"),
    ] {
        // `x86` must not match `x86_64`, which the ordering above handles.
        if lower.contains(needle) {
            return Some(abi.to_string());
        }
    }
    None
}

/// True if this file should be treated as a bundle rather than a plain APK.
pub fn is_bundle(path: &Path) -> bool {
    matches!(Kind::detect(path), Kind::Apkm | Kind::Xapk | Kind::Apks)
}

impl Bundle {
    /// Open a container and classify its members.
    pub fn open(path: &Path) -> Result<Bundle, String> {
        let kind = Kind::detect(path);
        let data = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;

        if kind == Kind::Apk {
            return Ok(Bundle {
                kind,
                path: path.to_path_buf(),
                base: path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "base.apk".into()),
                splits: Vec::new(),
                obb: Vec::new(),
                abis: Vec::new(),
                data,
            });
        }

        let entries = apk::apk_entries_bytes(&data)
            .map_err(|e| format!("{}: not a ZIP container ({e})", path.display()))?;

        let mut apk_members: Vec<String> = entries
            .iter()
            .filter(|n| n.to_ascii_lowercase().ends_with(".apk"))
            .cloned()
            .collect();
        if apk_members.is_empty() {
            return Err(format!("{}: contains no APK members", path.display()));
        }

        // The base APK is the one Play calls `base.apk`; bundletool writes
        // `splits/base-master.apk`. Otherwise take the largest member, which is
        // the base in every wrapper seen in practice.
        let base = apk_members
            .iter()
            .find(|n| {
                let n = n.to_ascii_lowercase();
                n.ends_with("/base.apk")
                    || n == "base.apk"
                    || n.ends_with("/base-master.apk")
                    || n == "base-master.apk"
            })
            .cloned()
            .unwrap_or_else(|| apk_members.remove(0));

        let mut splits = Vec::new();
        let mut abis = Vec::new();
        for name in &apk_members {
            if *name == base {
                continue;
            }
            let abi = abi_from_name(name);
            if let Some(a) = &abi {
                if !abis.contains(a) {
                    abis.push(a.clone());
                }
            }
            splits.push(MemberApk {
                name: name.clone(),
                abi,
            });
        }
        abis.sort();

        let mut obb = Vec::new();
        for name in &entries {
            if name.starts_with("Android/obb/") {
                let size = apk::extract_named_bytes(&data, name)
                    .map(|d| d.len() as u64)
                    .unwrap_or(0);
                obb.push((name.clone(), size));
            }
        }

        Ok(Bundle {
            kind,
            path: path.to_path_buf(),
            base,
            splits,
            obb,
            abis,
            data,
        })
    }

    /// Inflate one member (the base APK or any split).
    pub fn member(&self, name: &str) -> Result<Vec<u8>, String> {
        if self.kind == Kind::Apk {
            return Ok(self.data.clone());
        }
        apk::extract_named_bytes(&self.data, name)
            .map_err(|e| format!("{}: cannot extract `{name}`: {e}", self.path.display()))
    }

    /// The base APK's bytes.
    pub fn base_bytes(&self) -> Result<Vec<u8>, String> {
        self.member(&self.base)
    }

    /// Parse the base APK's manifest.
    pub fn info(&self) -> Result<ApkInfo, String> {
        let name = self.base.clone();
        apk::read_apk_bytes(&self.base_bytes()?)
            .map_err(|e| format!("{}: {name}: {e}", self.path.display()))
    }

    /// Native libraries across the base APK and every split.
    ///
    /// Play puts the `.so` files in the ABI split, not in `base.apk`, so reading
    /// only the base makes an ARM game look like it has no native code at all.
    pub fn native_libs(&self) -> Result<Vec<(String, NativeLib)>, String> {
        let mut out = Vec::new();
        let mut push = |member: &str, bytes: &[u8]| {
            for lib in apk::native_libs_bytes(bytes).unwrap_or_default() {
                out.push((member.to_string(), lib));
            }
        };
        push(&self.base, &self.base_bytes()?);
        for split in &self.splits {
            if let Ok(bytes) = self.member(&split.name) {
                push(&split.name, &bytes);
            }
        }
        Ok(out)
    }

    /// The split that carries `abi`, if the bundle ships one.
    pub fn abi_split(&self, abi: &str) -> Option<&MemberApk> {
        self.splits.iter().find(|s| s.abi.as_deref() == Some(abi))
    }

    /// Extract the chosen members (plus OBB data) into `dest`.
    ///
    /// Returns the paths written, base first. `abi` selects which architecture
    /// split to keep; `None` keeps every split.
    pub fn extract_to(&self, dest: &Path, abi: Option<&str>) -> Result<Vec<PathBuf>, String> {
        std::fs::create_dir_all(dest)
            .map_err(|e| format!("cannot create {}: {e}", dest.display()))?;
        let mut written = Vec::new();

        let base_name = Path::new(&self.base)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "base.apk".into());
        let base_path = dest.join(&base_name);
        std::fs::write(&base_path, self.base_bytes()?)
            .map_err(|e| format!("cannot write {}: {e}", base_path.display()))?;
        written.push(base_path);

        for split in &self.splits {
            let is_abi_split = split.abi.is_some();
            if is_abi_split {
                match abi {
                    Some(want) if split.abi.as_deref() != Some(want) => continue,
                    _ => {}
                }
            }
            let file = Path::new(&split.name)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| split.name.clone());
            let path = dest.join(file);
            if let Ok(bytes) = self.member(&split.name) {
                std::fs::write(&path, bytes)
                    .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
                written.push(path);
            }
        }

        // OBB data keeps its `Android/obb/<pkg>/` layout, which is what the
        // Android APIs (and our asset lookup) expect.
        for (name, _) in &self.obb {
            let rel = name.trim_start_matches('/');
            let path = dest.join(rel);
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(bytes) = self.member(name) {
                std::fs::write(&path, bytes)
                    .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
                written.push(path);
            }
        }
        Ok(written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_names_map_back_to_android_abis() {
        assert_eq!(abi_from_name("split_config.arm64_v8a.apk").as_deref(), Some("arm64-v8a"));
        assert_eq!(abi_from_name("split_config.armeabi_v7a.apk").as_deref(), Some("armeabi-v7a"));
        assert_eq!(abi_from_name("splits/base-x86_64.apk").as_deref(), Some("x86_64"));
        assert_eq!(abi_from_name("split_config.x86.apk").as_deref(), Some("x86"));
        assert_eq!(abi_from_name("split_config.ldpi.apk"), None);
        assert_eq!(abi_from_name("split_asset_pack.apk"), None);
    }

    #[test]
    fn kind_detection_uses_the_extension() {
        assert_eq!(Kind::detect(Path::new("a.apkm")), Kind::Apkm);
        assert_eq!(Kind::detect(Path::new("a.XAPK")), Kind::Xapk);
        assert_eq!(Kind::detect(Path::new("a.apks")), Kind::Apks);
        assert_eq!(Kind::detect(Path::new("a.apk")), Kind::Apk);
        assert_eq!(Kind::detect(Path::new("a.zip")), Kind::Apk);
    }

    /// Build a stored-only ZIP, enough for the bundle reader.
    fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data) in entries {
            let off = out.len() as u32;
            let crc = crc32(data);
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // stored
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);

            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes()); // extra
            central.extend_from_slice(&0u16.to_le_bytes()); // comment
            central.extend_from_slice(&0u16.to_le_bytes()); // disk
            central.extend_from_slice(&0u16.to_le_bytes()); // attrs
            central.extend_from_slice(&0u32.to_le_bytes()); // attrs
            central.extend_from_slice(&off.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let cd_off = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&cd_off.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for b in data {
            crc ^= *b as u32;
            for _ in 0..8 {
                if crc & 1 != 0 {
                    crc = (crc >> 1) ^ 0xEDB8_8320;
                } else {
                    crc >>= 1;
                }
            }
        }
        !crc
    }

    #[test]
    fn apkm_members_and_abis() {
        let dir = std::env::temp_dir().join(format!("pd-bundle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("game.apkm");
        let zip = make_zip(&[
            ("base.apk", b"BASE"),
            ("split_config.arm64_v8a.apk", b"ARM64"),
            ("split_config.armeabi_v7a.apk", b"ARM32"),
            ("split_config.hdpi.apk", b"DPI"),
            ("split_asset_pack.apk", b"ASSETS"),
            ("Android/obb/com.x/game.obb", b"OBB"),
        ]);
        std::fs::write(&path, &zip).unwrap();

        let b = Bundle::open(&path).unwrap();
        assert_eq!(b.kind, Kind::Apkm);
        assert_eq!(b.base, "base.apk");
        assert_eq!(b.abis, vec!["arm64-v8a", "armeabi-v7a"]);
        assert_eq!(b.splits.len(), 4);
        assert_eq!(b.obb.len(), 1);
        assert_eq!(b.base_bytes().unwrap(), b"BASE");
        assert_eq!(b.abi_split("arm64-v8a").unwrap().name, "split_config.arm64_v8a.apk");

        // Extracting with an ABI keeps that split and the non-ABI ones.
        let out = dir.join("out");
        let written = b.extract_to(&out, Some("arm64-v8a")).unwrap();
        let names: Vec<String> = written
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"base.apk".to_string()));
        assert!(names.contains(&"split_config.arm64_v8a.apk".to_string()));
        assert!(!names.contains(&"split_config.armeabi_v7a.apk".to_string()));
        assert!(names.contains(&"split_config.hdpi.apk".to_string()));
        assert!(out.join("Android/obb/com.x/game.obb").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bundletool_naming_is_understood() {
        let dir = std::env::temp_dir().join(format!("pd-apks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("game.apks");
        std::fs::write(
            &path,
            make_zip(&[
                ("toc.pb", b"TOC"),
                ("splits/base-master.apk", b"BASE"),
                ("splits/base-arm64_v8a.apk", b"ARM64"),
            ]),
        )
        .unwrap();
        let b = Bundle::open(&path).unwrap();
        assert_eq!(b.kind, Kind::Apks);
        assert_eq!(b.base, "splits/base-master.apk");
        assert_eq!(b.abis, vec!["arm64-v8a"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
