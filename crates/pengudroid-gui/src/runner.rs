//! Starting and stopping an app.
//!
//! The GUI does not reimplement the launch path: it runs `pengudroid launch`,
//! which already knows about the sysroot, the ART backend and the Wayland
//! session. Keeping one launcher means the GUI and the CLI cannot drift.

use crate::registry::{pengudroid_binary, Entry};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

/// An owned launcher and its dedicated process group. Keeping Child lets us
/// reap exits and prevents stale running state and zombie launcher processes.
pub struct RunningApp {
    child: Child,
    group: i32,
}

impl RunningApp {
    pub fn id(&self) -> u32 { self.child.id() }

    pub fn launch(entry: &Entry, fps: bool) -> Result<Self, String> {
        use std::os::unix::process::CommandExt;
        let log_dir = crate::registry::data_dir().join("logs");
        std::fs::create_dir_all(&log_dir).map_err(|e| e.to_string())?;
        let log = std::fs::File::create(log_dir.join(format!("{}.log", entry.package)))
            .map_err(|e| e.to_string())?;
        let mut cmd = Command::new(pengudroid_binary());
        cmd.arg("launch").arg(&entry.apk);
        if let Some(sysroot) = sysroot() { cmd.arg("--sysroot").arg(sysroot); }
        cmd.env("PENGUDROID_APP_LABEL", &entry.label)
            .env("PENGUDROID_FPS_METER", if fps { "1" } else { "0" })
            .stdin(Stdio::null()).stdout(log.try_clone().map_err(|e| e.to_string())?)
            .stderr(log).process_group(0);
        let child = cmd.spawn().map_err(|e| format!("cannot launch app: {e}"))?;
        Ok(Self { group: child.id() as i32, child })
    }

    pub fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        let status = self.child.try_wait()?;
        if status.is_some() {
            // The launcher can fail before one of its children notices. Retire
            // that entire group rather than orphaning the Android/audio helper.
            unsafe { libc::kill(-self.group, libc::SIGKILL); }
            self.group = 0;
        }
        Ok(status)
    }

    /// Called on a worker, never on the UI thread. Signal the whole process
    /// group even if the launcher exits before its hung Android child does.
    pub fn stop(mut self) -> Result<(), String> {
        let signal = |sig| {
            if unsafe { libc::kill(-self.group, sig) } == 0 { return Ok(()); }
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::ESRCH) { Ok(()) } else { Err(e.to_string()) }
        };
        signal(libc::SIGTERM)?;
        std::thread::sleep(std::time::Duration::from_millis(300));
        signal(libc::SIGKILL)?;
        self.child.wait().map_err(|e| e.to_string())?;
        self.group = 0;
        Ok(())
    }
}

impl Drop for RunningApp {
    fn drop(&mut self) {
        if self.group > 1 { unsafe { libc::kill(-self.group, libc::SIGKILL); } }
        // After SIGKILL the owned launcher must be reaped, including during
        // window shutdown. Do not wait for guest cooperation.
        let _ = self.child.wait();
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::os::unix::process::CommandExt;

    #[test]
    fn stopping_launcher_also_kills_a_hung_android_child() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "trap '' TERM; sleep 30 & echo $!; wait"])
            .stdout(Stdio::piped()).process_group(0).spawn().unwrap();
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
        let descendant: u32 = line.trim().parse().unwrap();
        let group = child.id() as i32;
        let running = RunningApp { child, group };
        let began = std::time::Instant::now();
        running.stop().unwrap();
        assert!(began.elapsed() < std::time::Duration::from_secs(3));
        // A dead descendant can briefly be a zombie awaiting its host reaper;
        // it must never remain an executing/sleeping Android helper.
        let state = std::fs::read_to_string(format!("/proc/{descendant}/stat")).ok()
            .and_then(|text| text.rsplit_once(") ").map(|(_, fields)| fields.chars().next().unwrap()));
        assert!(state.is_none() || state == Some('Z'), "descendant is still alive: {state:?}");
    }
}
