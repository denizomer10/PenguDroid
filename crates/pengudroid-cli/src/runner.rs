//! The `run` workflow: parse an APK, make sure the daemon is up, and launch a
//! guest process that owns its own Wayland window.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use pengudroid_proto::default_socket_path;

use crate::apk::{self, ApkInfo};
use crate::client::Client;
use pengudroid_proto::Role;

/// Default window size for a phone-shaped app.
const DEFAULT_W: u32 = 720;
const DEFAULT_H: u32 = 1280;

/// Set when the CLI receives SIGINT/SIGTERM; used to take the guest down with us.
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

/// Ensure a daemon is listening on `socket`, starting one if needed.
pub fn ensure_daemon(socket: &Path) -> Result<(), String> {
    if is_daemon_up(socket) {
        return Ok(());
    }
    let bin = find_daemon_binary()?;
    Command::new(&bin)
        .arg("--socket")
        .arg(socket)
        .arg("--daemonize")
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", bin.display()))?;

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        if is_daemon_up(socket) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(format!("daemon did not start within 3s (tried {})", bin.display()))
}

fn is_daemon_up(socket: &Path) -> bool {
    Client::connect_to(socket)
        .and_then(|c| c.handshake(Role::Cli))
        .is_ok()
}

/// Locate the `pengudroid-daemon` binary: explicit override, then a sibling of
/// this executable, then `PATH`.
pub fn find_daemon_binary() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("PENGUDROID_DAEMON") {
        return Ok(PathBuf::from(p));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("pengudroid-daemon");
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }
    Ok(PathBuf::from("pengudroid-daemon"))
}

/// Parse the APK (best-effort) and launch a guest.
pub fn run(apk_path: &str, component: Option<&str>, extra: &[String]) -> Result<(), String> {
    let info = match apk::read_apk(apk_path) {
        Ok(info) => info,
        Err(e) => {
            eprintln!("warning: {e}");
            eprintln!("warning: continuing with defaults derived from the file name");
            ApkInfo { package: package_from_filename(apk_path), ..Default::default() }
        }
    };
    print_apk_summary(&info);

    let socket = default_socket_path();
    ensure_daemon(&socket)?;

    let exe = std::env::current_exe().map_err(|e| format!("cannot find own exe: {e}"))?;
    let resolved_component = component
        .map(|c| info.resolve(c))
        .or_else(|| info.main_activity_name().map(str::to_string))
        .unwrap_or_default();
    let title = info.label.clone().unwrap_or_else(|| {
        if info.package.is_empty() {
            "PenguDroid app".to_string()
        } else {
            info.package.clone()
        }
    });

    let mut cmd = Command::new(exe);
    cmd.arg("__guest")
        .arg("--socket")
        .arg(&socket)
        .arg("--package")
        .arg(&info.package)
        .arg("--component")
        .arg(&resolved_component)
        .arg("--title")
        .arg(&title)
        .arg("--width")
        .arg(DEFAULT_W.to_string())
        .arg("--height")
        .arg(DEFAULT_H.to_string())
        .args(extra);

    let mut child = cmd.spawn().map_err(|e| format!("failed to start guest: {e}"))?;
    println!(
        "started guest: pid {} ({}{})",
        child.id(),
        info.package,
        if resolved_component.is_empty() {
            String::new()
        } else {
            format!(" :: {resolved_component}")
        }
    );
    println!("the window is live; press Ctrl-C to stop the guest");

    // Take the guest down when we are interrupted, and exit when it does.
    // SAFETY: `on_signal` has the correct handler signature.
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
    loop {
        if STOP.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            println!("stopped guest");
            return Ok(());
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                println!("guest exited: {status}");
                return Ok(());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(format!("waiting on guest failed: {e}")),
        }
    }
}

fn package_from_filename(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().replace(['-', ' '], "_"))
        .unwrap_or_else(|| "unknown".to_string())
}

fn print_apk_summary(info: &ApkInfo) {
    println!("package:      {}", if info.package.is_empty() { "<unknown>" } else { &info.package });
    if let Some(v) = &info.version_name {
        println!("version:      {v}");
    }
    if let Some(sdk) = info.target_sdk {
        println!("targetSdk:    {sdk}");
    }
    if let Some(a) = info.main_activity_name() {
        println!("main activity:{a}");
    }
    if !info.permissions.is_empty() {
        println!("permissions:  {}", info.permissions.len());
    }
}
