//! Static Android-facing compatibility metadata, not a hardware identity.
//!
//! This profile neither changes host privileges nor supplies runtime API/CPU
//! identity, fingerprints, verified-boot claims, or attestation evidence.

#![forbid(unsafe_code)]

/// Pixel 11 display metadata with explicitly synthetic PenguDroid device names.
///
/// The release-like values are compatibility strings, not security assertions.
/// Iteration order is fixed; every property appears exactly once.
pub const PIXEL_11_PROFILE: &[(&str, &str)] = &[
    ("ro.product.model", "Pixel 11"),
    ("ro.product.manufacturer", "Google"),
    ("ro.product.brand", "google"),
    ("ro.product.name", "pixel11"),
    ("ro.product.device", "pixel11"),
    ("ro.product.board", "pixel11"),
    ("ro.product.system.model", "Pixel 11"),
    ("ro.product.system.manufacturer", "Google"),
    ("ro.product.system.brand", "google"),
    ("ro.product.system.name", "pixel11"),
    ("ro.product.system.device", "pixel11"),
    ("ro.product.system.board", "pixel11"),
    ("ro.product.vendor.model", "Pixel 11"),
    ("ro.product.vendor.manufacturer", "Google"),
    ("ro.product.vendor.brand", "google"),
    ("ro.product.vendor.name", "pixel11"),
    ("ro.product.vendor.device", "pixel11"),
    ("ro.product.vendor.board", "pixel11"),
    ("ro.product.product.model", "Pixel 11"),
    ("ro.product.product.manufacturer", "Google"),
    ("ro.product.product.brand", "google"),
    ("ro.product.product.name", "pixel11"),
    ("ro.product.product.device", "pixel11"),
    ("ro.product.product.board", "pixel11"),
    ("ro.product.odm.model", "Pixel 11"),
    ("ro.product.odm.manufacturer", "Google"),
    ("ro.product.odm.brand", "google"),
    ("ro.product.odm.name", "pixel11"),
    ("ro.product.odm.device", "pixel11"),
    ("ro.product.odm.board", "pixel11"),
    ("ro.product.system_ext.model", "Pixel 11"),
    ("ro.product.system_ext.manufacturer", "Google"),
    ("ro.product.system_ext.brand", "google"),
    ("ro.product.system_ext.name", "pixel11"),
    ("ro.product.system_ext.device", "pixel11"),
    ("ro.product.system_ext.board", "pixel11"),
    ("ro.hardware", "pengudroid"),
    ("ro.debuggable", "0"),
    ("ro.secure", "1"),
    ("ro.adb.secure", "1"),
    ("ro.build.type", "user"),
    ("ro.build.tags", "release-keys"),
    ("service.adb.root", "0"),
];

/// The default compatibility profile; an alias of [`PIXEL_11_PROFILE`].
/// Callers can reuse the slice directly or use `.iter().copied()`.
pub const DEFAULT_PROFILE: &[(&str, &str)] = PIXEL_11_PROFILE;

/// Classify profile-owned keys for caller-enforced override locking.
///
/// Matching is exact and case-sensitive after removing any number of
/// `ro.appcompat_override.` wrappers. This does not lock or write properties;
/// callers must enforce it at each override entry point, using the raw store
/// rather than an appcompat view. Runtime platform keys need a separate guard.
pub fn is_protected_profile_key(mut key: &str) -> bool {
    while let Some(base) = key.strip_prefix("ro.appcompat_override.") {
        key = base;
    }
    DEFAULT_PROFILE.iter().any(|&(name, _)| name == key)
}
