//! Compile an independent Android LP64 C layout and deliver a real Linux signal.
//! A Rust-only conversion roundtrip cannot detect a consistently wrong layout.
use std::path::PathBuf;
use std::process::Command;

#[test]
fn android_lp64_signal_handler_receives_realtime_signal() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let dir = std::env::temp_dir().join(format!("pd-signal-abi-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let executable = dir.join("probe");
    let compiled = Command::new("cc").arg(manifest.join("fixtures/signal_abi.c"))
        .arg("-fstack-protector-all").arg("-ldl").arg("-o").arg(&executable).output().expect("a C compiler is required for the signal ABI probe");
    assert!(compiled.status.success(), "{}", String::from_utf8_lossy(&compiled.stderr));
    let profile = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().to_path_buf();
    let shim = profile.join("libpengudroid_shim.so");
    assert!(shim.is_file(), "build the workspace shared libraries before running the ABI probe");
    let result = Command::new(&executable).env("LD_PRELOAD", shim).env_remove("LD_LIBRARY_PATH")
        .output().unwrap();
    let _ = std::fs::remove_dir_all(dir);
    assert!(result.status.success(), "signal ABI failed ({}): {}{}", result.status,
        String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr));
    assert!(String::from_utf8_lossy(&result.stdout).contains("handler received signal 34"));
    assert!(String::from_utf8_lossy(&result.stdout).contains("waitinfo payload and timed wait passed"));
    assert!(String::from_utf8_lossy(&result.stdout).contains("child stack guard reset passed"));
}
