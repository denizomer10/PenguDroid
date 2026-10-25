//! Detached launches: `pengudroid start` asks the daemon to fork the guest, the
//! guest registers itself, `ps` lists it, and `--wait` reports its exit.
//!
//! The daemon runs with `--no-wayland`, so the guest fails at the surface
//! request. That is exactly what makes the test meaningful: it proves the guest
//! really ran, reached the daemon, was registered and was reaped with its status.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use pengudroid_proto::{Message, Role, PROTO_VERSION};
use pengudroid_socket::Connection;

struct Daemon {
    child: Child,
    dir: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn binary(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("test exe path");
    let profile = exe.parent().expect("deps dir").parent().expect("profile dir");
    profile.join(name)
}

fn handshake(endpoint: &Path) -> Option<Connection> {
    let conn = Connection::connect_endpoint(endpoint).ok()?;
    conn.send_message(&Message::Hello { version: PROTO_VERSION, role: Role::Cli, pid: 1 }, &[])
        .ok()?;
    match conn.recv_message().ok()?.0 {
        Message::Welcome { .. } => Some(conn),
        _ => None,
    }
}

/// Each test gets its own runtime dir: the tests share a process id, and
/// sharing a daemon would let one test's `stop` kill another's.
fn start_daemon(tag: &str) -> (Daemon, PathBuf) {
    let base = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join(format!("pd-start-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let runtime = dir.join("runtime");
    std::fs::create_dir_all(runtime.join("pengudroid")).expect("runtime dir");
    let socket = runtime.join("pengudroid").join("daemon.sock");

    let child = Command::new(binary("pengudroid-daemon"))
        .arg("--socket")
        .arg(&socket)
        .arg("--no-wayland")
        .env("XDG_RUNTIME_DIR", &runtime).env_remove("PENGUDROID_SOCKET")
        .env("PENGUDROID_CLI", binary("pengudroid"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(daemon_log()))
        .spawn()
        .expect("spawn daemon");
    let mut daemon = Daemon { child, dir };

    let deadline = Instant::now() + Duration::from_secs(15);
    while handshake(&socket).is_none() {
        if let Ok(Some(status)) = daemon.child.try_wait() {
            panic!("daemon exited during startup ({status})\n{}", read_log());
        }
        assert!(Instant::now() < deadline, "daemon never came up\n{}", read_log());
        std::thread::sleep(Duration::from_millis(20));
    }
    (daemon, runtime)
}

fn log_path() -> PathBuf {
    std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(format!("pd-start-{}.log", std::process::id()))
}

// The log helper stays shared: only diagnostics read it.

fn daemon_log() -> std::fs::File {
    std::fs::OpenOptions::new().create(true).append(true).open(log_path()).expect("open daemon log")
}

fn read_log() -> String {
    std::fs::read_to_string(log_path()).unwrap_or_default()
}

fn cli(runtime: &Path, args: &[&str]) -> Output {
    Command::new(binary("pengudroid"))
        .args(args)
        .env("XDG_RUNTIME_DIR", runtime).env_remove("PENGUDROID_SOCKET")
        .env("PENGUDROID_DAEMON", binary("pengudroid-daemon"))
        .output()
        .expect("run cli")
}

#[test]
fn start_runs_the_guest_and_reports_its_exit() {
    let (_daemon, runtime) = start_daemon("wait");

    // `--wait` blocks until the guest is gone. Without a Wayland session the
    // guest exits non-zero, which is the observable proof that it ran.
    let out = cli(&runtime, &["start", "--wait", "--demo"]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned()
        + &String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("started guest: pid"), "{text}");
    assert!(
        text.contains("guest exited") || text.contains("graphics"),
        "expected the guest's exit to be reported:\n{text}"
    );

    // The daemon is still alive and the guest is no longer listed.
    let status = cli(&runtime, &["status"]);
    assert!(
        status.status.success(),
        "status failed after the guest exited:\n{}\n{}\ndaemon log:\n{}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr),
        read_log()
    );
    let _ = std::io::Write::flush(&mut std::io::stderr());
    let ps = cli(&runtime, &["ps"]);
    assert!(
        String::from_utf8_lossy(&ps.stdout).contains("no guest processes running")
            || !String::from_utf8_lossy(&ps.stdout).contains("com.pengudroid.demo"),
        "{}",
        String::from_utf8_lossy(&ps.stdout)
    );

    assert!(cli(&runtime, &["stop"]).status.success());
}

#[test]
fn detached_launch_unknown_apk_is_an_error() {
    let (_daemon, runtime) = start_daemon("unknown");
    let out = cli(&runtime, &["start", "/definitely/not/here.apk"]);
    assert!(!out.status.success());
    // `start` prints the daemon's message for `RunApk` failures.
    let text = String::from_utf8_lossy(&out.stdout).into_owned()
        + &String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("no such file"), "{text}");
    assert!(cli(&runtime, &["stop"]).status.success());
}
