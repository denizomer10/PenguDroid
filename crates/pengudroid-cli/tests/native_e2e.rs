//! End-to-end test of the NativeActivity path: a real Android native app
//! renders on the host GPU and its frames reach the daemon.
//!
//! The test is skipped (not failed) when there is no Wayland session or the C
//! fixture has not been built, because neither is a property of the Rust code:
//!
//! ```sh
//! fixtures/native-game/build.sh          # needs a C compiler
//! cargo test -p pengudroid-cli --test native_e2e
//! ```

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

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
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

#[test]
fn native_activity_game_renders_on_the_host_gpu() {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        eprintln!("skipping: no WAYLAND_DISPLAY (the GPU path needs a compositor)");
        return;
    }
    let fixture = workspace_root().join("fixtures/native-game/libmain.so");
    if !fixture.exists() {
        eprintln!(
            "skipping: {} is missing; run fixtures/native-game/build.sh",
            fixture.display()
        );
        return;
    }

    let base = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join(format!("pd-native-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("pengudroid")).expect("runtime dir");

    // The daemon and the EGL shim both derive the socket from XDG_RUNTIME_DIR.
    let child = Command::new(binary("pengudroid-daemon"))
        .env("XDG_RUNTIME_DIR", &dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn daemon");
    let daemon = Daemon { child, dir: dir.clone() };
    let socket = dir.join("pengudroid").join("daemon.sock");
    let deadline = Instant::now() + Duration::from_secs(15);
    while !handshake(&socket) {
        assert!(Instant::now() < deadline, "daemon never came up");
        std::thread::sleep(Duration::from_millis(20));
    }

    let out: Output = Command::new(binary("pengudroid"))
        .args(["native", "--lib"])
        .arg(&fixture)
        .args(["--width", "640", "--height", "360", "--seconds", "3"])
        .env("XDG_RUNTIME_DIR", &dir)
        .env("PENGUDROID_DAEMON", binary("pengudroid-daemon"))
        .output()
        .expect("run native");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "native app failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("PenguDroid Invaders") || stderr.contains("rendering"),
        "the app never reached its render loop:\n{stderr}"
    );
    assert!(
        stderr.contains("frames") || stdout.contains("native app exited"),
        "no frame report:\n{stderr}"
    );

    // The daemon must have created a surface for the app.
    let conn = Connection::connect_endpoint(&socket).expect("connect");
    conn.send_message(&Message::Hello { version: PROTO_VERSION, role: Role::Cli, pid: 1 }, &[])
        .expect("hello");
    let _ = conn.recv_message();
    conn.send_message(&Message::Shutdown, &[]).expect("shutdown");
    let _ = conn.recv_message();
    drop(daemon);
}
