//! Uses the optional local Android sysroot to check real framework behavior.
//! Ordinary unit tests cannot detect an empty WindowConfiguration or a native
//! KeyCharacterMap parcel mismatch. No compositor or APK is needed here.
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Run { child: Child, dir: PathBuf }
impl Drop for Run {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn run_framework_probe(sysroot: &Path, label: &str, blast: bool, native_display: bool,
    extra_env: &[(&str, &str)]) -> String {
    let binary = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("pengudroid");
    let dir = std::env::temp_dir().join(format!("pd-framework-{}-{label}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("probe.log");
    let output = std::fs::File::create(&log).unwrap();
    let mut command = Command::new(binary);
    command
        .args(["art-test", "--sysroot"]).arg(&sysroot)
        .args(["--register-natives", "libandroid_runtime.so,libhwui.so", "--vm-option", "-Xint", "--vm-option", "-Xcheck:jni"])
        .env("PENGUDROID_PROBE_KEYBOARD", "1")
        .env("PENGUDROID_PROBE_WINDOW_CONFIG", "1")
        .env("PENGUDROID_PROBE_AUDIO_FOCUS", "1")
        .env("PENGUDROID_PROBE_SETTINGS", "1")
        .env("PENGUDROID_TRACE_VIDEO", "1")
        .env("XDG_RUNTIME_DIR", &dir)
        .env("XDG_CACHE_HOME", dir.join("cache"))
        .env_remove("PENGUDROID_SOCKET")
        .stdin(Stdio::null()).stdout(output.try_clone().unwrap()).stderr(output);
    if native_display { command.env("PENGUDROID_PROBE_NATIVE_DISPLAY", "1"); }
    else { command.env_remove("PENGUDROID_PROBE_NATIVE_DISPLAY"); }
    if blast { command.env("PENGUDROID_PROBE_BLAST_LIFECYCLE", "1"); }
    else {
        command.env_remove("PENGUDROID_PROBE_BLAST_LIFECYCLE")
            .env("PENGUDROID_PROBE_PLATFORM_STATE", "1");
    }
    command.envs(extra_env.iter().copied());
    let child = command.spawn().unwrap();
    let mut run = Run { child, dir };
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = run.child.try_wait().unwrap() { break status; }
        assert!(Instant::now() < deadline, "ART probe timed out:\n{}", std::fs::read_to_string(&log).unwrap_or_default());
        std::thread::sleep(Duration::from_millis(10));
    };
    let text = std::fs::read_to_string(log).unwrap();
    assert!(status.success(), "ART probe failed ({status}):\n{text}");
    text
}

fn optional_android17_sysroot() -> Option<PathBuf> {
    let explicit = std::env::var_os("PENGUDROID_TEST_ANDROID17_SYSROOT");
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap();
    let root = explicit.as_ref().map(PathBuf::from)
        .unwrap_or_else(|| workspace.join(".toolchain/sysroot-android17"));
    if !root.join("system/framework/framework.jar").exists() {
        assert!(explicit.is_none(), "explicit API-37 sysroot is incomplete or missing: {}", root.display());
        eprintln!("skipping: no API-37 runtime (set PENGUDROID_TEST_ANDROID17_SYSROOT)");
        return None;
    }
    Some(root)
}

#[test]
fn android17_blast_lifecycle_under_checkjni() {
    let Some(sysroot) = optional_android17_sysroot() else { return; };
    let text = run_framework_probe(&sysroot, "api37-blast", true, true, &[]);
    assert!(text.contains("display: native internal 720x1280 @ 60 Hz, SDR RGBA_8888 passed"), "{text}");
    assert!(text.contains("binder: framework ServiceManager codes = [1, 3, 2, 4]"), "{text}");
    assert!(text.contains("blast: real framework create/destroy/recreate passed"), "{text}");
    assert_eq!(text.matches("[blast] video: consumer stopped").count(), 2, "{text}");
}

/// This used to abort in Codec2Client's manager cast, or retry its manifest
/// query forever. An absent backend must be a successful empty discovery, not
/// a fake codec list, an incompatible manager or an unsupported transaction.
#[test]
fn android17_media_discovery_without_codec2_under_checkjni() {
    let Some(sysroot) = optional_android17_sysroot() else { return; };
    let text = run_framework_probe(&sysroot, "api37-media", false, false, &[
        ("PENGUDROID_PROBE_MEDIA_LIST", "1"),
        ("PENGUDROID_NO_CODEC2_SERVICE", "1"),
        ("PENGUDROID_TRACE_BINDER", "1"),
    ]);
    assert!(text.contains("media: API-37 codec enumeration repeated 16 times:"), "{text}");
    assert!(text.contains("hidl instances code=13 android.hardware.media.c2@1.0::IComponentStore: []"), "{text}");
    assert!(!text.contains("received incompatible service"), "{text}");
    assert!(!text.contains("Retrying..."), "{text}");
}

/// Core API-37 migration gate. BLAST lifecycle has its own gate; neither test
/// establishes Android-17 app rendering or media compatibility.
#[test]
fn android17_core_services_under_checkjni() {
    let Some(sysroot) = optional_android17_sysroot() else { return; };
    let text = run_framework_probe(&sysroot, "api37-core", false, true, &[
        ("PENGUDROID_PROBE_MAIN_THREAD", "1"),
    ]);
    let installed = text.find("binder: framework ServiceManager codes = [1, 3, 2, 4]").unwrap();
    let attached = text.find("apphost: ActivityThread attached on the main thread")
        .unwrap_or_else(|| panic!("ActivityThread.attach did not succeed:\n{text}"));
    assert!(installed < attached, "ServiceManager must be installed before attach:\n{text}");
    assert!(!text.contains("IServiceManager codes not installed"), "{text}");
    for expected in [
        "display: native internal 720x1280 @ 60 Hz, SDR RGBA_8888 passed",
        "platform: feature cache native shared-memory roundtrip passed",
        "media: framework session/metrics transaction codes = [",
        "binder: framework android/os/IUserManager$Stub transaction codes = [",
        "binder: framework android/os/IThermalService$Stub transaction codes = [",
        "audio: probe framework SDK_INT=37, focusEnvironmentToken=true",
        "binder: framework ServiceManager codes = [1, 3, 2, 4]",
        "settings: Binder/Parcel scalar value, missing key and unsupported list passed",
        "audio: AIDL focus granted, current gain verified, abandon cleared",
        "window: local WindowLayout.computeFrames = 720x1280",
        "window: API-37 resized parcel typed result and flags passed",
        "native KeyCharacterMap: 6 Unicode cases passed",
    ] { assert!(text.contains(expected), "missing {expected}:\n{text}"); }
}
