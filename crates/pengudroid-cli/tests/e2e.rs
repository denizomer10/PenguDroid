//! End-to-end CLI test against a real `pengudroid-daemon` process.
//!
//! The daemon binds its control socket, the `pengudroid` binary drives it, and
//! the test checks the user-visible contract: `status`, `getprop`, `setprop`,
//! `services`, `ps`, unknown APKs, `demo` and `stop`. Graphics is disabled
//! (`--no-wayland`), so no compositor is required.
//!
//! Both binaries are built by cargo into the profile directory next to this
//! test.

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
    let path = profile.join(name);
    assert!(path.exists(), "{} is missing; build the workspace first", path.display());
    path
}

/// A temp dir short enough for a Unix socket path.
fn workdir(tag: &str) -> PathBuf {
    let base = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    // The daemon name carries the tag: `stop` shuts down *the* daemon for a
    // runtime dir, so two concurrent tests must not share one.
    let dir = base.join(format!("pd-cli-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(dir.with_extension("stale"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn handshake(endpoint: &Path) -> bool {
    let Ok(conn) = Connection::connect_endpoint(endpoint) else { return false };
    if conn
        .send_message(&Message::Hello { version: PROTO_VERSION, role: Role::Cli, pid: 1 }, &[])
        .is_err()
    {
        return false;
    }
    matches!(conn.recv_message(), Ok((Message::Welcome { .. }, _)))
}

fn start_daemon(tag: &str) -> (Daemon, PathBuf, PathBuf) {
    let dir = workdir(tag);
    let runtime = dir.join("runtime");
    std::fs::create_dir_all(runtime.join("pengudroid")).expect("runtime dir");
    let socket = runtime.join("pengudroid").join("daemon.sock");

    let child = Command::new(binary("pengudroid-daemon"))
        .arg("--socket")
        .arg(&socket)
        .arg("--no-wayland")
        .env("XDG_RUNTIME_DIR", &runtime).env_remove("PENGUDROID_SOCKET")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn daemon");

    let mut daemon = Daemon { child, dir };
    let deadline = Instant::now() + Duration::from_secs(15);
    while !handshake(&socket) {
        if let Ok(Some(status)) = daemon.child.try_wait() {
            let mut detail = String::new();
            if let Some(stderr) = daemon.child.stderr.take() {
                use std::io::Read;
                let _ = stderr.take(8192).read_to_string(&mut detail);
            }
            panic!("daemon exited during startup ({status}):\n{detail}");
        }
        assert!(Instant::now() < deadline, "daemon never accepted a connection");
        std::thread::sleep(Duration::from_millis(20));
    }
    (daemon, runtime, socket)
}

fn cli(runtime: &Path, args: &[&str]) -> Output {
    Command::new(binary("pengudroid"))
        .args(args)
        .env("XDG_RUNTIME_DIR", runtime).env_remove("PENGUDROID_SOCKET")
        .env("PENGUDROID_DAEMON", binary("pengudroid-daemon"))
        .output()
        .expect("run cli")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn cli_drives_a_real_daemon() {
    let (_daemon, runtime, socket) = start_daemon("e2e");

    // status: daemon, protocol, services, guests.
    let status = cli(&runtime, &["status"]);
    let out = stdout(&status);
    assert!(status.status.success(), "status failed:\n{out}{}", stderr(&status));
    assert!(out.contains("daemon: running"), "{out}");
    assert!(out.contains(&format!("protocol: v{PROTO_VERSION}")), "{out}");
    assert!(out.contains("guests:   0"), "{out}");
    assert!(out.contains("services: 8"), "{out}");

    // Properties: one, all, and a write that is read back.
    let one = cli(&runtime, &["getprop", "ro.build.version.sdk"]);
    assert_eq!(stdout(&one).trim(), "[ro.build.version.sdk]: [37]");
    let downgrade = cli(&runtime, &["setprop", "ro.build.version.sdk", "33"]);
    assert!(!downgrade.status.success(), "platform downgrade must be rejected");
    let still_current = cli(&runtime, &["getprop", "ro.build.version.sdk"]);
    assert_eq!(stdout(&still_current).trim(), "[ro.build.version.sdk]: [37]");

    let all = cli(&runtime, &["getprop"]);
    let text = stdout(&all);
    assert!(text.lines().count() >= 10, "{text}");
    assert!(text.contains("[ro.product.model]: [Pixel 11]"), "{text}");
    assert!(text.contains("[ro.debuggable]: [0]"), "{text}");
    assert!(text.contains("[ro.secure]: [1]"), "{text}");

    assert!(cli(&runtime, &["setprop", "debug.e2e", "yes"]).status.success());
    let read = cli(&runtime, &["getprop", "debug.e2e"]);
    assert_eq!(stdout(&read).trim(), "[debug.e2e]: [yes]");

    // Services table, with headers and the platform stubs.
    let services = cli(&runtime, &["services"]);
    let text = stdout(&services);
    assert!(text.starts_with("HANDLE"), "{text}");
    assert!(text.contains("SurfaceFlinger") && text.contains("media.camera"), "{text}");

    // No guests yet.
    let ps = cli(&runtime, &["ps"]);
    assert!(stdout(&ps).contains("no guest processes running"), "{}", stdout(&ps));

    // A missing APK is an error, not a pretend launch.
    let missing = cli(&runtime, &["run", "/definitely/not/here.apk"]);
    assert!(!missing.status.success());
    assert!(stderr(&missing).contains("cannot read"), "{}", stderr(&missing));

    // `demo` starts a guest; this daemon has no graphics, so the guest exits and
    // the CLI reports the diagnostics instead of claiming success.
    let demo = cli(&runtime, &["demo"]);
    let err = stderr(&demo);
    assert!(!demo.status.success(), "demo should fail without graphics:\n{err}");
    assert!(err.contains("wayland-selftest"), "the hint should name the diagnostic:\n{err}");

    // stop: acknowledged, waits for the socket, leaves no daemon behind.
    let stop = cli(&runtime, &["stop"]);
    assert!(stop.status.success(), "stop failed: {}", stderr(&stop));
    assert!(!socket.exists(), "socket should be gone after `stop`");
    assert!(!handshake(&socket), "daemon still reachable after `stop`");

    let status = cli(&runtime, &["status"]);
    assert!(!status.status.success());
    assert!(stdout(&status).contains("daemon: not running"), "{}", stdout(&status));
}

#[test]
fn stop_then_start_is_race_free() {
    let (_daemon, runtime, _socket) = start_daemon("restart");

    // `stop` waits for the daemon to finish, so an immediately following start
    // must succeed rather than collide with a dying process.
    assert!(cli(&runtime, &["stop"]).status.success());
    let daemon = cli(&runtime, &["daemon"]);
    assert!(daemon.status.success(), "restart failed: {}", stderr(&daemon));
    assert!(stdout(&daemon).contains("daemon running on"), "{}", stdout(&daemon));

    let status = cli(&runtime, &["status"]);
    assert!(status.status.success());
    assert!(stdout(&status).contains("daemon: running"));

    assert!(cli(&runtime, &["stop"]).status.success());
}

#[test]
fn idle_input_subscriber_receives_every_key_in_order() {
    use pengudroid_proto::InputKind;

    let (_daemon, runtime, socket) = start_daemon("keys");
    let subscriber = Connection::connect_endpoint(&socket).expect("connect subscriber");
    subscriber.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    subscriber.send_message(&Message::Hello {
        version: PROTO_VERSION, role: Role::Guest, pid: 424242,
    }, &[]).unwrap();
    assert!(matches!(subscriber.recv_message().unwrap().0, Message::Welcome { .. }));
    subscriber.send_message(&Message::SubscribeInput { surface_id: 71 }, &[]).unwrap();
    assert_eq!(subscriber.recv_message().unwrap().0, Message::Ack);

    // Each CLI is a different socket. The subscription's serving thread is
    // waiting on recv when a key arrives in its inbox. A discarded try_recv
    // in the timeout path used to eat the first event in almost every burst.
    for code in 16..32 {
        std::thread::sleep(Duration::from_millis(12));
        let sent = cli(&runtime, &["key", &code.to_string()]);
        assert!(sent.status.success(), "{}", stderr(&sent));
        for pressed in [true, false] {
            let received = subscriber.recv_message().expect("input event was lost").0;
            assert_eq!(received, Message::InputEvent {
                surface_id: 71, event: InputKind::Key { keycode: code, pressed },
            });
        }
    }
    for (state, pressed) in [("down", true), ("up", false)] {
        assert!(cli(&runtime, &["key", "42", state]).status.success());
        assert_eq!(subscriber.recv_message().unwrap().0, Message::InputEvent {
            surface_id: 71, event: InputKind::Key { keycode: 42, pressed },
        });
    }
    for invalid in [&[][..], &["-1"][..], &["768"][..], &["30", "invalid"][..], &["30", "up", "extra"][..]] {
        let mut args = vec!["key"];
        args.extend_from_slice(invalid);
        assert_eq!(cli(&runtime, &args).status.code(), Some(2));
    }
}
