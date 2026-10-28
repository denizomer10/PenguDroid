#![forbid(unsafe_code)]

use pengudroid_device::{is_protected_profile_key, DEFAULT_PROFILE, PIXEL_11_PROFILE};

#[test]
fn default_profile_has_exact_consistent_properties_in_stable_order() {
    let mut expected = Vec::new();
    for partition in ["", "system.", "vendor.", "product.", "odm.", "system_ext."] {
        for (field, value) in [
            ("model", "Pixel 11"),
            ("manufacturer", "Google"),
            ("brand", "google"),
            ("name", "pixel11"),
            ("device", "pixel11"),
            ("board", "pixel11"),
        ] {
            expected.push((format!("ro.product.{partition}{field}"), value));
        }
    }
    expected.extend(
        [
            ("ro.hardware", "pengudroid"),
            ("ro.debuggable", "0"),
            ("ro.secure", "1"),
            ("ro.adb.secure", "1"),
            ("ro.build.type", "user"),
            ("ro.build.tags", "release-keys"),
            ("service.adb.root", "0"),
        ]
        .map(|(key, value)| (key.to_owned(), value)),
    );
    let actual: Vec<_> = DEFAULT_PROFILE
        .iter()
        .map(|&(key, value)| (key.to_owned(), value))
        .collect();
    assert_eq!(actual, expected);
    assert_eq!(DEFAULT_PROFILE, PIXEL_11_PROFILE);
}

#[test]
fn every_profile_key_is_protected_including_nested_appcompat_aliases() {
    for &(key, _) in DEFAULT_PROFILE {
        assert!(is_protected_profile_key(key), "{key}");
        let wrapped = format!("ro.appcompat_override.{key}");
        assert!(is_protected_profile_key(&wrapped), "{wrapped}");
        let nested = format!("ro.appcompat_override.{wrapped}");
        assert!(is_protected_profile_key(&nested), "{nested}");
    }
}

#[test]
fn protection_matches_exact_keys_not_prefixes_or_similar_names() {
    for &(key, _) in DEFAULT_PROFILE {
        for other in [
            format!("{key}.extra"),
            format!("x{key}"),
            format!("{key} "),
            key.to_ascii_uppercase(),
        ] {
            assert!(!is_protected_profile_key(&other), "{other}");
        }
    }
    for key in [
        "",
        "ro.product.",
        "ro.product.unknown.model",
        "ro.product.systematic.model",
        "persist.example",
        "ro.appcompat_override.",
        "ro.appcompat_override.persist.example",
    ] {
        assert!(!is_protected_profile_key(key), "{key}");
    }
}

#[test]
fn runtime_api_cpu_and_fingerprint_keys_are_neither_emitted_nor_owned() {
    for &(key, _) in DEFAULT_PROFILE {
        for fragment in ["cpu", "sdk", "api_level", "fingerprint", "build.version."] {
            assert!(!key.contains(fragment), "{key}");
        }
    }
    for key in [
        "ro.build.version.sdk",
        "ro.build.version.sdk_full",
        "ro.build.version.release",
        "ro.product.first_api_level",
        "ro.board.api_level",
        "ro.product.cpu.abi",
        "ro.product.cpu.abilist",
        "ro.product.cpu.abilist32",
        "ro.product.cpu.abilist64",
        "ro.build.fingerprint",
        "ro.system.build.fingerprint",
    ] {
        assert!(!is_protected_profile_key(key), "{key}");
        let wrapped = format!("ro.appcompat_override.ro.appcompat_override.{key}");
        assert!(!is_protected_profile_key(&wrapped), "{wrapped}");
    }
}

#[test]
fn no_hardware_security_or_attestation_claims_are_supplied() {
    for &(key, value) in DEFAULT_PROFILE {
        for fragment in [
            "attest",
            "verifiedboot",
            "vbmeta",
            "locked",
            "serial",
            "imei",
            "security_patch",
            ".soc.",
            "board.platform",
        ] {
            assert!(!key.contains(fragment), "{key}");
        }
        assert!(!["green", "locked", "arm64-v8a", "Tensor"].contains(&value));
    }
    for key in [
        "ro.boot.verifiedbootstate",
        "ro.boot.flash.locked",
        "ro.boot.vbmeta.device_state",
        "ro.build.version.security_patch",
        "ro.serialno",
        "ro.boot.serialno",
        "ro.soc.model",
    ] {
        assert!(!is_protected_profile_key(key), "{key}");
    }
}
