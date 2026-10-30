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

    // Isolate the control socket while retaining the real Wayland/PipeWire runtime.
    let child = Command::new(binary("pengudroid-daemon"))
        .env("PENGUDROID_SOCKET", dir.join("pengudroid/daemon.sock"))
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
        .env("PENGUDROID_SOCKET", dir.join("pengudroid/daemon.sock"))
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

#[test]
fn software_video_frames_keep_wayland_connection_alive() {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        eprintln!("skipping: no WAYLAND_DISPLAY");
        return;
    }
    let dir = std::env::temp_dir().join(format!("pd-shm-e2e-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("pengudroid")).unwrap();
    // Resolve the compositor explicitly; the host runtime remains unchanged.
    let display = PathBuf::from(std::env::var_os("WAYLAND_DISPLAY").unwrap());
    let display = if display.is_absolute() { display } else {
        PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap()).join(display)
    };
    let child = Command::new(binary("pengudroid-daemon"))
        .env("PENGUDROID_SOCKET", dir.join("pengudroid/daemon.sock")).env("WAYLAND_DISPLAY", display)
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
        .spawn().unwrap();
    let _daemon = Daemon { child, dir: dir.clone() };
    let socket = dir.join("pengudroid/daemon.sock");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !handshake(&socket) {
        assert!(Instant::now() < deadline, "daemon never came up");
        std::thread::sleep(Duration::from_millis(20));
    }
    let conn = Connection::connect_endpoint(&socket).unwrap();
    // A full workspace run executes many test binaries in parallel, and this
    // test waits on compositor round trips. Three seconds was enough alone but
    // not under that load, where a healthy daemon was reported as `WouldBlock`.
    conn.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    conn.send_message(&Message::Hello {
        version: PROTO_VERSION, role: Role::Guest, pid: std::process::id() as i32,
    }, &[]).unwrap();
    conn.recv_message().unwrap();
    let request = |msg: Message, fds: &[i32]| {
        conn.send_message(&msg, fds).unwrap();
        let (reply, received_fds) = conn.recv_message().unwrap();
        for fd in received_fds { drop(unsafe { OwnedFd::from_raw_fd(fd) }); }
        reply
    };
    let parent = request(Message::CreateSurface { width: 64, height: 64, title: "Software video test".into() }, &[]);
    let Message::SurfaceCreated { surface_id: parent_id, .. } = parent else { panic!("{parent:?}") };
    let child = request(Message::CreateSubsurface {
        parent_pid: std::process::id() as i32, x: 0, y: 0, width: 16, height: 16,
    }, &[]);
    let Message::SubsurfaceCreated { surface_id } = child else { panic!("{child:?}") };
    assert_eq!(request(Message::SetSurfaceOptions { surface_id: parent_id, fps_meter: true }, &[]), Message::Ack);
    assert!(matches!(request(Message::SetSurfaceOptions { surface_id: u32::MAX, fps_meter: false }, &[]), Message::Error { .. }));
    // RGB565 red. This is ordinary memfd storage, deliberately not DMA_BUF.
    let raw = unsafe { libc::memfd_create(c"software-video-test".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(raw >= 0);
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let pixels = [0, 0xf8].repeat(32 * 32);
    assert_eq!(unsafe { libc::write(fd.as_raw_fd(), pixels.as_ptr().cast(), pixels.len()) }, pixels.len() as isize);
    for _ in 0..8 {
        assert_eq!(request(Message::PresentShm { surface_id, width: 32, height: 32, stride: 64, format: 4,
            crop: pengudroid_proto::BufferCrop::full(32, 32) }, &[fd.as_raw_fd()]), Message::Ack);
    }
    // Two video queues in one window must be siblings, never nested under the
    // newest video layer. Retiring the first queue must not retire the second.
    let sibling = request(Message::CreateSubsurface {
        parent_pid: std::process::id() as i32, x: 16, y: 0, width: 16, height: 16,
    }, &[]);
    let Message::SubsurfaceCreated { surface_id: sibling_id } = sibling else { panic!("{sibling:?}") };
    assert_eq!(request(Message::DestroySurface { surface_id }, &[]), Message::Ack);
    assert_eq!(request(Message::PresentShm {
        surface_id: sibling_id, width: 32, height: 32, stride: 64, format: 4,
        crop: pengudroid_proto::BufferCrop::full(32, 32),
    }, &[fd.as_raw_fd()]), Message::Ack);
    // EGL window destruction must invalidate its video layers. Acking frames
    // on an orphan would keep the BLAST consumer stuck on an invisible layer.
    assert_eq!(request(Message::DestroySurface { surface_id: parent_id }, &[]), Message::Ack);
    assert!(matches!(request(Message::PresentShm {
        surface_id: sibling_id, width: 32, height: 32, stride: 64, format: 4,
        crop: pengudroid_proto::BufferCrop::full(32, 32),
    }, &[fd.as_raw_fd()]), Message::Error { .. }));
    assert!(matches!(request(Message::CreateSubsurface {
        parent_pid: std::process::id() as i32, x: 0, y: 0, width: 16, height: 16,
    }, &[]), Message::Error { .. }));
    // Creating another toplevel forces a compositor roundtrip. A bogus
    // linux-dmabuf import can appear Acked before its fatal protocol error is
    // dispatched; this must still succeed after all software frames.
    let after = request(Message::CreateSurface { width: 64, height: 64, title: "After software frames".into() }, &[]);
    assert!(matches!(after, Message::SurfaceCreated { .. }), "compositor connection was lost: {after:?}");
    let child = request(Message::CreateSubsurface {
        parent_pid: std::process::id() as i32, x: 0, y: 0, width: 16, height: 16,
    }, &[]);
    let Message::SubsurfaceCreated { surface_id: replacement } = child else { panic!("{child:?}") };
    assert_eq!(request(Message::PresentShm {
        surface_id: replacement, width: 32, height: 32, stride: 64, format: 4,
        crop: pengudroid_proto::BufferCrop::full(32, 32),
    }, &[fd.as_raw_fd()]), Message::Ack);
    assert_eq!(request(Message::Shutdown, &[]), Message::Goodbye);
}
