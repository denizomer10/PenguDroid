//! Starting and stopping an app.
//!
//! The GUI does not reimplement the launch path: it runs `pengudroid launch`,
//! which already knows about the sysroot, the ART backend and the Wayland
//! session. Keeping one launcher means the GUI and the CLI cannot drift.

use crate::registry::{pengudroid_binary, Entry};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

/// Start an app and return its pid.
pub fn launch(entry: &Entry) -> Result<u32, String> {
    let mut cmd = Command::new(pengudroid_binary());
    cmd.arg("launch").arg(&entry.apk);
    if let Some(sysroot) = sysroot() {
        cmd.arg("--sysroot").arg(sysroot);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child: Child = cmd
        .spawn()
        .map_err(|e| format!("cannot start pengudroid launch: {e}"))?;
    Ok(child.id())
}

/// Stop an app: `SIGTERM`, then `SIGKILL` if it ignores it.
pub fn stop(entry: &Entry) -> Result<(), String> {
    let Some(pid) = entry.pid else {
        return Ok(());
    };
    let pid = pid as libc::pid_t;
    // SAFETY: `kill` with a plain signal number has no memory effects.
    unsafe {
        if libc::kill(pid, libc::SIGTERM) != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ESRCH) {
                return Ok(()); // already gone
            }
            return Err(format!("kill({pid}) failed: {err}"));
        }
    }
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        // SAFETY: signal 0 only checks for existence.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        if !alive {
            return Ok(());
        }
    }
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    Ok(())
}

/// The Android tree to run against.
///
/// `PENGUDROID_SYSROOT` wins, then the in-repo tree, then the per-user install
/// location. Returning `None` lets `pengudroid launch` print its own guidance.
pub fn sysroot() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("PENGUDROID_SYSROOT") {
        let path = PathBuf::from(explicit);
        if path.exists() {
            return Some(path);
        }
    }
    let mut candidates = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join(".toolchain/sysroot"));
    }
    if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
        candidates.push(PathBuf::from(data).join("pengudroid/sysroot"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join(".local/share/pengudroid/sysroot"));
    }
    candidates.into_iter().find(|p| p.exists())
}
