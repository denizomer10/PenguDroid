//! What an app needs from the platform, and whether this tree provides it.
//!
//! `doctor` used to stop at "needs ART". That was true when it was written and
//! says nothing useful now: ART is hosted, ARM64 code runs through the native
//! bridge, and what actually decides whether a game gets to its title screen is
//! the long tail — which engine it is, which system libraries its ARM64 code
//! links, whether its login screen is a `WebView`.
//!
//! Everything here is read from the package: library names, the strings inside
//! the libraries (a `DT_NEEDED` entry and a `dlopen("libvulkan.so")` argument
//! are both just the soname in the file), and type descriptors in the DEX. No
//! claim is made about behaviour that only a run can show.

/// How well the current tree covers one requirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Implemented and exercised by an app that runs today.
    Ready,
    /// Implemented in part, or implemented but not yet exercised by an app of
    /// this kind.
    Partial,
    /// Not implemented; whatever the app does through this will fail.
    Missing,
}

impl Status {
    pub fn tag(self) -> &'static str {
        match self {
            Status::Ready => "ok",
            Status::Partial => "partial",
            Status::Missing => "MISSING",
        }
    }
}

/// One thing the app needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requirement {
    pub name: String,
    pub status: Status,
    pub note: String,
}

/// The game engine, recognised by the libraries it ships.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    UnityIl2cpp,
    UnityMono,
    Unreal,
    Godot,
    Cocos,
    Sdl,
}

impl Engine {
    pub fn name(self) -> &'static str {
        match self {
            Engine::UnityIl2cpp => "Unity (IL2CPP)",
            Engine::UnityMono => "Unity (Mono)",
            Engine::Unreal => "Unreal Engine",
            Engine::Godot => "Godot",
            Engine::Cocos => "Cocos2d-x",
            Engine::Sdl => "SDL",
        }
    }

    /// Recognise an engine from the native library file names.
    pub fn detect<S: AsRef<str>>(libs: &[S]) -> Option<Engine> {
        let has = |name: &str| libs.iter().any(|l| l.as_ref() == name);
        let prefixed = |prefix: &str| libs.iter().any(|l| l.as_ref().starts_with(prefix));
        if has("libunity.so") {
            return Some(if has("libil2cpp.so") {
                Engine::UnityIl2cpp
            } else {
                Engine::UnityMono
            });
        }
        if has("libUE4.so") || has("libUnreal.so") {
            return Some(Engine::Unreal);
        }
        if has("libgodot_android.so") {
            return Some(Engine::Godot);
        }
        if prefixed("libcocos2d") {
            return Some(Engine::Cocos);
        }
        if prefixed("libSDL2") || prefixed("libSDL3") {
            return Some(Engine::Sdl);
        }
        None
    }
}

/// What the package contains, as far as requirements are concerned.
#[derive(Debug, Default)]
pub struct Facts {
    /// The ABI whose libraries would be executed.
    pub abi: String,
    /// True when `abi` is the host's own, so no translator is involved.
    pub host_abi: bool,
    /// File names of the libraries for `abi`.
    pub libs: Vec<String>,
    /// System sonames found inside those libraries (linked or `dlopen`ed).
    pub system_libs: Vec<&'static str>,
    pub uses_webview: bool,
    pub uses_play_services: bool,
    /// `android:screenOrientation` of the launcher activity.
    pub screen_orientation: Option<i32>,
    pub has_keymap_preset: bool,
}

/// NDK system libraries that are not part of an app and must come from the
/// platform. `libc`/`libm`/`libdl`/`liblog`/`libz` are omitted: every app links
/// them and they are covered on both paths.
const SYSTEM_LIBS: &[&str] = &[
    "libEGL.so",
    "libGLESv1_CM.so",
    "libGLESv2.so",
    "libGLESv3.so",
    "libandroid.so",
    "libvulkan.so",
    "libOpenSLES.so",
    "libaaudio.so",
    "libmediandk.so",
    "libjnigraphics.so",
    "libnativewindow.so",
    "libcamera2ndk.so",
    "libOpenMAXAL.so",
    "libbinder_ndk.so",
    "libneuralnetworks.so",
    "libamidi.so",
];

/// Guest (ARM64) system libraries that have a native-bridge stub forwarding to
/// the host — `STUB_LIBRARIES` in `tools/build-guest-stubs.py`. Any other
/// system library an ARM64 app links resolves to the guest's stock Android
/// build, which expects system services that do not exist here.
const BRIDGED_GUEST_LIBS: &[&str] = &[
    "libEGL.so",
    "libGLESv1_CM.so",
    "libGLESv2.so",
    "libGLESv3.so",
    "libandroid.so",
];

/// Find which of `needles` occur in `hay`, in one pass.
fn scan(hay: &[u8], needles: &[&[u8]], found: &mut [bool]) {
    let mut firsts = [false; 256];
    for needle in needles {
        if let Some(first) = needle.first() {
            firsts[*first as usize] = true;
        }
    }
    for (offset, byte) in hay.iter().enumerate() {
        if !firsts[*byte as usize] {
            continue;
        }
        let rest = &hay[offset..];
        for (index, needle) in needles.iter().enumerate() {
            if !found[index] && rest.starts_with(needle) {
                found[index] = true;
            }
        }
    }
}

/// System sonames referenced anywhere in a native library.
///
/// The soname has to be a whole string (NUL-terminated), so `libandroid.so`
/// does not match inside `libandroid.software.so`.
pub fn system_libs_in(library: &[u8], found: &mut Vec<&'static str>) {
    let terminated: Vec<Vec<u8>> = SYSTEM_LIBS
        .iter()
        .map(|name| {
            let mut bytes = name.as_bytes().to_vec();
            bytes.push(0);
            bytes
        })
        .collect();
    let needles: Vec<&[u8]> = terminated.iter().map(|n| n.as_slice()).collect();
    let mut hits = vec![false; needles.len()];
    scan(library, &needles, &mut hits);
    for (name, hit) in SYSTEM_LIBS.iter().zip(hits) {
        if hit && !found.contains(name) {
            found.push(name);
        }
    }
}

/// `(uses WebView, uses Google Play services)` for one DEX file.
pub fn dex_uses(dex: &[u8]) -> (bool, bool) {
    let needles: [&[u8]; 2] = [b"Landroid/webkit/WebView;", b"Lcom/google/android/gms/"];
    let mut hits = [false; 2];
    scan(dex, &needles, &mut hits);
    (hits[0], hits[1])
}

fn requirement(name: &str, status: Status, note: &str) -> Requirement {
    Requirement { name: name.into(), status, note: note.into() }
}

/// The requirement list for an app, most fundamental first.
pub fn assess(facts: &Facts) -> Vec<Requirement> {
    let mut out = Vec::new();
    let translated = !facts.libs.is_empty() && !facts.host_abi;

    if translated {
        out.push(if facts.abi == "arm64-v8a" {
            requirement(
                "ARM64 code on x86_64",
                Status::Partial,
                "runs through the Berberis native bridge (Beach Buggy Racing 2 does); build it \
                 with tools/build-nativebridge.py and set PENGUDROID_NATIVE_BRIDGE_LIB",
            )
        } else {
            requirement(
                &format!("{} code on x86_64", facts.abi),
                Status::Missing,
                "only an ARM64 guest runtime is built; 32-bit ARM is not translated",
            )
        });
    }

    match Engine::detect(&facts.libs) {
        Some(Engine::UnityIl2cpp | Engine::UnityMono) if translated => out.push(requirement(
            "Unity player under translation",
            Status::Partial,
            "no Unity title has been run yet; UnityPlayer renders into a SurfaceView through \
             the bridged libEGL/libGLESv3, the path the SDL game already uses",
        )),
        Some(Engine::Unreal) if translated => out.push(requirement(
            "Unreal Engine under translation",
            Status::Partial,
            "no Unreal title has been run yet",
        )),
        _ => {}
    }

    for lib in &facts.system_libs {
        if translated {
            if BRIDGED_GUEST_LIBS.contains(lib) {
                continue;
            }
            let note = match *lib {
                "libvulkan.so" => {
                    "no ARM64 stub and no Android window-system integration \
                     (vkCreateAndroidSurfaceKHR); the app must fall back to OpenGL ES"
                }
                "libOpenSLES.so" => {
                    "no ARM64 stub: native audio (Unity/FMOD/Wwise output) reaches the guest's \
                     stock library, which has no AudioFlinger to talk to"
                }
                "libaaudio.so" => "no ARM64 stub and no AAudio implementation",
                "libmediandk.so" => "no ARM64 stub: native MediaCodec (video cutscenes) is unavailable",
                _ => "no ARM64 native-bridge stub for this library",
            };
            out.push(requirement(lib, Status::Missing, note));
        } else {
            match *lib {
                "libvulkan.so" => out.push(requirement(
                    lib,
                    Status::Partial,
                    "maps to the host loader, but there is no Android surface \
                     (vkCreateAndroidSurfaceKHR), so nothing can be presented",
                )),
                "libaaudio.so" => {
                    out.push(requirement(lib, Status::Missing, "AAudio is not implemented"))
                }
                _ => {}
            }
        }
    }

    if facts.uses_webview {
        out.push(requirement(
            "WebView",
            Status::Missing,
            "no WebView provider is hosted; login, captcha and in-game notice pages that use \
             one cannot open (see docs/STATUS.md, \"a real WebView provider is not hosted\")",
        ));
    }
    if facts.uses_play_services {
        out.push(requirement(
            "Google Play services",
            Status::Missing,
            "not present; sign-in, billing and push through it are unavailable (apps that only \
             use it for ads or analytics usually carry on without it)",
        ));
    }
    if crate::display::wants_landscape(facts.screen_orientation) {
        out.push(requirement(
            "Landscape display",
            Status::Ready,
            "the launcher creates a 1280x720 display for this activity \
             (PENGUDROID_DISPLAY=WxH picks another size)",
        ));
    }
    if facts.has_keymap_preset {
        out.push(requirement(
            "Keyboard and mouse controls",
            Status::Partial,
            "a built-in keymap exists (WASD joystick, mouse-look, multi-touch); print it with \
             `pengudroid keymap-preset`. Positions are approximate until checked in the game",
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(reqs: &[Requirement], status: Status) -> Vec<&str> {
        reqs.iter().filter(|r| r.status == status).map(|r| r.name.as_str()).collect()
    }

    #[test]
    fn detects_engines_from_library_names() {
        assert_eq!(Engine::detect(&["libunity.so", "libil2cpp.so"]), Some(Engine::UnityIl2cpp));
        assert_eq!(Engine::detect(&["libunity.so", "libmono.so"]), Some(Engine::UnityMono));
        assert_eq!(Engine::detect(&["libUE4.so"]), Some(Engine::Unreal));
        assert_eq!(Engine::detect(&["libSDL2.so", "libmain.so"]), Some(Engine::Sdl));
        assert_eq!(Engine::detect(&["libfoo.so"]), None);
        assert_eq!(Engine::detect::<&str>(&[]), None);
    }

    #[test]
    fn finds_whole_sonames_only() {
        let mut library = b"\0libEGL.so\0junk libOpenSLES.so\0".to_vec();
        // A longer name that merely starts like a system library.
        library.extend_from_slice(b"libandroid.so.backup\0libvulkan.so");
        let mut found = Vec::new();
        system_libs_in(&library, &mut found);
        // `libvulkan.so` at the very end has no terminator, so it is not a string.
        assert_eq!(found, vec!["libEGL.so", "libOpenSLES.so"]);
        // A second library adds to the set without duplicating.
        system_libs_in(b"libEGL.so\0libvulkan.so\0", &mut found);
        assert_eq!(found, vec!["libEGL.so", "libOpenSLES.so", "libvulkan.so"]);
    }

    #[test]
    fn dex_descriptors_reveal_webview_and_play_services() {
        assert_eq!(dex_uses(b"...Landroid/webkit/WebView;..."), (true, false));
        assert_eq!(dex_uses(b"Lcom/google/android/gms/common/api/Api;"), (false, true));
        assert_eq!(dex_uses(b"Landroid/webkit/WebViewClient;"), (false, false));
        assert_eq!(dex_uses(b""), (false, false));
    }

    /// The shape of an ARM64 Unity game with a web login: the report must name
    /// each blocker rather than a blanket "cannot run".
    #[test]
    fn arm64_unity_game_lists_its_real_blockers() {
        let facts = Facts {
            abi: "arm64-v8a".into(),
            host_abi: false,
            libs: vec!["libunity.so".into(), "libil2cpp.so".into(), "libmain.so".into()],
            system_libs: vec!["libEGL.so", "libGLESv3.so", "libandroid.so", "libOpenSLES.so", "libvulkan.so"],
            uses_webview: true,
            uses_play_services: false,
            screen_orientation: Some(6),
            has_keymap_preset: true,
        };
        let reqs = assess(&facts);
        assert_eq!(names(&reqs, Status::Missing), vec!["libOpenSLES.so", "libvulkan.so", "WebView"]);
        assert_eq!(names(&reqs, Status::Ready), vec!["Landscape display"]);
        assert_eq!(
            names(&reqs, Status::Partial),
            vec!["ARM64 code on x86_64", "Unity player under translation", "Keyboard and mouse controls"]
        );
    }

    #[test]
    fn host_abi_app_has_no_translation_requirements() {
        let facts = Facts {
            abi: "x86_64".into(),
            host_abi: true,
            libs: vec!["libmain.so".into()],
            system_libs: vec!["libEGL.so", "libOpenSLES.so", "libvulkan.so"],
            ..Default::default()
        };
        let reqs = assess(&facts);
        assert!(names(&reqs, Status::Missing).is_empty());
        assert_eq!(names(&reqs, Status::Partial), vec!["libvulkan.so"]);
    }

    #[test]
    fn thirty_two_bit_arm_is_not_translated() {
        let facts = Facts {
            abi: "armeabi-v7a".into(),
            libs: vec!["libmain.so".into()],
            ..Default::default()
        };
        assert_eq!(names(&assess(&facts), Status::Missing), vec!["armeabi-v7a code on x86_64"]);
    }

    #[test]
    fn a_pure_java_app_needs_nothing_listed() {
        assert!(assess(&Facts::default()).is_empty());
    }
}
