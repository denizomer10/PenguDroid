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

#[test]
fn real_art_keyboard_and_local_window_layout() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap();
    let sysroot = std::env::var_os("PENGUDROID_TEST_SYSROOT").map(PathBuf::from)
        .unwrap_or_else(|| workspace.join(".toolchain/sysroot"));
    if !sysroot.join("system/framework/framework.jar").exists() {
        eprintln!("skipping: no Android sysroot (set PENGUDROID_TEST_SYSROOT)");
        return;
    }
    let binary = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("pengudroid");
    let dir = std::env::temp_dir().join(format!("pd-framework-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("probe.log");
    let output = std::fs::File::create(&log).unwrap();
    let child = Command::new(binary)
        .args(["art-test", "--sysroot"]).arg(&sysroot)
        .args(["--register-natives", "libandroid_runtime.so,libhwui.so", "--vm-option", "-Xint"])
        .env("PENGUDROID_PROBE_KEYBOARD", "1")
        .env("PENGUDROID_PROBE_WINDOW_CONFIG", "1")
        .env("PENGUDROID_PROBE_AUDIO_FOCUS", "1")
        .env("PENGUDROID_PROBE_BLAST_LIFECYCLE", "1")
        .env("PENGUDROID_TRACE_VIDEO", "1")
        .env("XDG_RUNTIME_DIR", &dir).env_remove("PENGUDROID_SOCKET")
        .stdin(Stdio::null()).stdout(output.try_clone().unwrap()).stderr(output)
        .spawn().unwrap();
    let mut run = Run { child, dir };
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = run.child.try_wait().unwrap() { break status; }
        assert!(Instant::now() < deadline, "ART probe timed out:\n{}", std::fs::read_to_string(&log).unwrap_or_default());
        std::thread::sleep(Duration::from_millis(10));
    };
    let text = std::fs::read_to_string(log).unwrap();
    assert!(status.success(), "ART probe failed ({status}):\n{text}");
    assert!(text.contains("blast: real framework create/destroy/recreate passed"), "{text}");
    assert_eq!(text.matches("[blast] video: consumer stopped").count(), 2, "{text}");
    assert!(text.contains("window: local WindowLayout.computeFrames = 720x1280"), "{text}");
    assert!(text.contains("native KeyCharacterMap: 6 Unicode cases passed"), "{text}");
    assert!(text.contains("audio: AIDL focus granted, current gain verified, abandon cleared"), "{text}");
}
