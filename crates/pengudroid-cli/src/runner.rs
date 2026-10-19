//! The `run` workflow: parse an APK, make sure the daemon is up, and launch a
//! guest process that owns its own Wayland window.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use pengudroid_proto::default_socket_path;

use crate::apk::{self, ApkInfo};
use crate::client::Client;
use pengudroid_proto::{Message, Role};

/// Default window size for a phone-shaped app.
const DEFAULT_W: u32 = 720;
const DEFAULT_H: u32 = 1280;

/// Set when the CLI receives SIGINT/SIGTERM; used to take the guest down with us.
static STOP: AtomicBool = AtomicBool::new(false);

/// Package name and title of the built-in demo guest (`pengudroid demo`).
pub const DEMO_PACKAGE: &str = "com.pengudroid.demo";
pub const DEMO_TITLE: &str = "PenguDroid demo";

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

/// Ensure a daemon is listening on `socket`, starting one if needed.
pub fn ensure_daemon(socket: &Path) -> Result<(), String> {
    if is_daemon_up(socket) {
        return Ok(());
    }
    let bin = find_daemon_binary()?;
    // The daemon logs to stderr. Keeping it attached to ours means a failure to
    // bind (sandboxed or read-only runtime dir) is explained where the user is
    // looking; `--daemonize` redirects its own stdio to /dev/null afterwards.
    let bin_arg = bin.display().to_string();
    let socket_arg = socket.display().to_string();
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("exec \"$0\" --socket \"$1\" --daemonize")
        .arg(&bin_arg)
        .arg(&socket_arg)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .map_err(|e| format!("cannot start {bin_arg}: {e}"))?;

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if is_daemon_up(socket) {
            // `--daemonize` double-forks; reap the intermediate child so this
            // process does not leave a zombie behind.
            let _ = child.try_wait();
            return Ok(());
        }
        if let Ok(Some(status)) = child.try_wait() {
            if !status.success() {
                return Err(format!("{} exited during startup ({status})", bin.display()));
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    Err(format!("daemon did not start within 5s (tried {})", bin.display()))
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

/// Parse the APK and launch a guest.
///
/// A missing or unreadable APK is an error: pretending to run an app that does
/// not exist only produces a confusing "window is live" and an immediate exit.
pub fn run(apk_path: &str, component: Option<&str>, extra: &[String]) -> Result<(), String> {
    let info = apk::read_apk(apk_path)?;
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

    launch_guest(cmd, &info.package, &resolved_component)
}

/// Launch the built-in demo guest: a window with a live animation, no APK.
///
/// This is the smoke test for the whole graphics path (daemon → Wayland →
/// shared buffer → guest painting) and the fastest way to see PenguDroid work.
pub fn run_demo(extra: &[String]) -> Result<(), String> {
    let socket = default_socket_path();
    ensure_daemon(&socket)?;
    let exe = std::env::current_exe().map_err(|e| format!("cannot find own exe: {e}"))?;

    let mut cmd = Command::new(exe);
    cmd.arg("__guest")
        .arg("--socket")
        .arg(&socket)
        .arg("--package")
        .arg(DEMO_PACKAGE)
        .arg("--component")
        .arg(format!("{DEMO_PACKAGE}/.DemoActivity"))
        .arg("--title")
        .arg(DEMO_TITLE)
        .arg("--width")
        .arg(DEFAULT_W.to_string())
        .arg("--height")
        .arg(DEFAULT_H.to_string())
        .args(extra);

    launch_guest(cmd, DEMO_PACKAGE, &format!("{DEMO_PACKAGE}/.DemoActivity"))
}

/// `pengudroid start <app.apk>`: ask the daemon itself to fork the guest, so it
/// survives the CLI exiting and `pengudroid ps` tracks it.
pub fn start_detached(apk_path: &str, component: Option<&str>, extra: &[String]) -> Result<i32, String> {
    run_detached(apk_path, component, extra)
}

/// `pengudroid start --demo`: the detached variant of [`run_demo`].
///
/// The demo has no APK, so it is addressed by a marker path the daemon
/// recognises; `__guest` then takes the package/component from `--component`.
pub fn start_demo_detached(extra: &[String]) -> Result<i32, String> {
    run_detached(DEMO_APK, Some(&format!("{DEMO_PACKAGE}/.DemoActivity")), extra)
}

/// Marker path used to launch the built-in demo through the daemon.
pub const DEMO_APK: &str = "-";

fn run_detached(
    apk_path: &str,
    component: Option<&str>,
    extra: &[String],
) -> Result<i32, String> {
    let socket = default_socket_path();
    ensure_daemon(&socket)?;
    let client = Client::connect_to(&socket)?;
    client.handshake(Role::Cli)?;
    let resolved = component.map(str::to_string);
    let reply = client.request(&Message::RunApk {
        path: apk_path.to_string(),
        component: resolved,
        args: extra.to_vec(),
    });
    // Say goodbye before dropping the socket: the daemon then logs a clean
    // close instead of observing a reset, and it reaps the connection eagerly.
    let _ = client.send(&Message::Goodbye);
    match reply {
        Ok((Message::RunStarted { pid, .. }, _)) => Ok(pid),
        Ok((Message::Error { message, .. }, _)) => Err(message),
        Ok((other, _)) => Err(format!("unexpected reply: {other:?}")),
        Err(e) => Err(e),
    }
}

/// The daemon polls each connection for server-initiated messages with this
/// read timeout; keep it in sync with `pengudroid-daemon`'s `INBOX_POLL`.
const SOCKET_POLL_MS: Duration = Duration::from_millis(5);

/// Wait for a guest started with [`start_detached`] to exit.
pub fn wait_detached(pid: i32) -> Result<String, String> {
    let socket = default_socket_path();
    let client = Client::connect_to(&socket)?;
    client.handshake(Role::Cli)?;
    match wait_for_run(&client, pid) {
        Ok(how) => {
            let _ = client.send(&Message::Goodbye);
            Ok(how)
        }
        Err(e) => Err(e),
    }
}

fn wait_for_run(client: &Client, pid: i32) -> Result<String, String> {
    loop {
        match client.request(&Message::QueryRun { pid })? {
            (Message::RunStatus { running: true, .. }, _) => {
                // Sleep longer than the daemon's per-read timeout: if we closed
                // the socket while it sat in a timed-out read, Linux would send
                // RST and the daemon could lose a queued reply.
                std::thread::sleep(SOCKET_POLL_MS + Duration::from_millis(20));
            }
            (Message::RunStatus { exit_code, signal, .. }, _) => {
                return Ok(if signal != 0 {
                    format!("signal {signal}")
                } else {
                    format!("exit code {exit_code}")
                });
            }
            (Message::Error { message, .. }, _) => return Err(message),
            (other, _) => return Err(format!("unexpected reply: {other:?}")),
        }
    }
}

/// Launch a fully prepared `__guest` command and supervise it.
fn launch_guest(mut cmd: Command, package: &str, component: &str) -> Result<(), String> {
    let mut child = cmd.spawn().map_err(|e| format!("failed to start guest: {e}"))?;
    println!(
        "started guest: pid {} ({}{})",
        child.id(),
        package,
        if component.is_empty() {
            String::new()
        } else {
            format!(" :: {component}")
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
                if status.success() {
                    println!("guest exited: {status}");
                    return Ok(());
                }
                // A guest that dies before drawing almost always means the
                // daemon has no graphics path; name the next diagnostic step
                // instead of leaving a bare exit code.
                eprintln!(
                    "hint: check the daemon's graphics backend with \
                     `pengudroid-daemon --wayland-selftest`"
                );
                return Err(format!("guest exited: {status}"));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(format!("waiting on guest failed: {e}")),
        }
    }
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
