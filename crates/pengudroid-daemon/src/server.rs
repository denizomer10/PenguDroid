//! The daemon's server core: shared state, the accept loop, and message
//! dispatch.
//!
//! One thread per client connection. Shared state is behind mutexes; nothing on
//! the hot path is more contended than a `Mutex` acquisition, and the graphics
//! broker (the one genuinely stateful, non-`Send`-friendly component) is
//! serialized behind a single mutex because Wayland's `EventQueue` must not be
//! driven concurrently.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pengudroid_proto::{Message, ProcInfo, PROTO_VERSION};
use pengudroid_socket::{Connection, Listener};

use crate::ashmem::AshmemRegion;
use crate::audio::AudioBroker;
use crate::binder::{BinderBackend, KernelBinder};
use crate::graphics::Graphics;
use crate::props::Props;
use crate::services::ServiceTable;

/// Set by the signal handler; observed by the accept loop.
pub static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Default surface size requested when a guest does not specify one.
const DEFAULT_SURFACE_W: u32 = 720;
const DEFAULT_SURFACE_H: u32 = 1280;

pub struct Daemon {
    pub start: Instant,
    props: Mutex<Props>,
    services: Mutex<ServiceTable>,
    procs: Mutex<Vec<ProcInfo>>,
    graphics: Mutex<Option<Graphics>>,
    audio: AudioBroker,
    binder: BinderBackend,
    shmem_regions: Mutex<Vec<(String, u64)>>,
    props_path: PathBuf,
    socket_path: PathBuf,
}

impl Daemon {
    pub fn new(socket_path: PathBuf, props_path: PathBuf, enable_wayland: bool) -> Daemon {
        let binder = match KernelBinder::open() {
            Some(k) => BinderBackend::Kernel(k),
            None => {
                crate::info!("kernel binder not available; using userspace broker");
                BinderBackend::UserspaceBroker
            }
        };
        let audio = AudioBroker::start();
        let mut props = Props::new();
        props.load_into(&props_path);
        let graphics = if enable_wayland {
            Graphics::connect()
        } else {
            crate::info!("graphics disabled by flag");
            None
        };
        Daemon {
            start: Instant::now(),
            props: Mutex::new(props),
            services: Mutex::new(ServiceTable::with_system_stubs()),
            procs: Mutex::new(Vec::new()),
            graphics: Mutex::new(graphics),
            audio,
            binder,
            shmem_regions: Mutex::new(Vec::new()),
            props_path,
            socket_path,
        }
    }

    pub fn socket_path(&self) -> &std::path::Path {
        &self.socket_path
    }

    pub fn summary(&self) -> String {
        let gfx = if self.graphics.lock().unwrap().is_some() { "wayland" } else { "none" };
        format!(
            "binder={}, graphics={}, audio={}, services={}, props={}",
            self.binder.describe(),
            gfx,
            self.audio.describe(),
            self.services.lock().unwrap().count(),
            self.props.lock().unwrap().len(),
        )
    }

    /// Accept loop. Runs until [`SHUTDOWN`] is set (SIGTERM/SIGINT) or `Shutdown`
    /// is received from a client.
    pub fn run(self: Arc<Self>, listener: Listener) -> io::Result<()> {
        listener.set_nonblocking(true)?;
        crate::info!("daemon ready on {} ({} ms startup)", self.socket_path.display(), self.start.elapsed().as_millis());
        crate::info!("{}", self.summary());
        while !SHUTDOWN.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok(conn) => {
                    let d = Arc::clone(&self);
                    std::thread::spawn(move || {
                        if let Err(e) = serve(d, conn) {
                            crate::debug!("client disconnected: {e}");
                        }
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => {
                    crate::warn!("accept failed: {e}");
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
        crate::info!("shutting down");
        // Persist properties for the next run.
        let props = self.props.lock().unwrap();
        if let Err(e) = props.save(&self.props_path) {
            crate::warn!("could not persist properties: {e}");
        }
        Ok(())
    }
}

/// Perform the handshake and then serve requests until the peer leaves.
fn serve(daemon: Arc<Daemon>, conn: Connection) -> io::Result<()> {
    let (hello, _) = conn
        .recv_message()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let role = match hello {
        Message::Hello { version, role, pid } => {
            if version != PROTO_VERSION {
                crate::warn!("client protocol v{version} != daemon v{PROTO_VERSION}");
            }
            crate::debug!("client connected: role={role:?} pid={pid}");
            let _ = conn.send_message(
                &Message::Welcome { version: PROTO_VERSION, pid: std::process::id() as i32 },
                &[],
            );
            role
        }
        other => {
            let _ = conn.send_message(
                &Message::Error { code: -1, message: "expected Hello".into() },
                &[],
            );
            crate::warn!("first message was not Hello: {other:?}");
            return Ok(());
        }
    };
    let _ = role;

    loop {
        let (msg, _fds) = match conn.recv_message() {
            Ok(v) => v,
            Err(pengudroid_socket::SocketError::Io(e))
                if e.kind() == io::ErrorKind::UnexpectedEof =>
            {
                crate::debug!("client closed connection");
                return Ok(());
            }
            Err(e) => return Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
        };
        let (reply, out_fds, close) = handle(&daemon, msg);
        if let Some(r) = reply {
            let raw: Vec<RawFd> = out_fds.iter().map(|f| f.as_raw_fd()).collect();
            // `out_fds` must outlive the send call.
            if let Err(e) = conn.send_message(&r, &raw) {
                crate::debug!("send failed: {e}");
                return Ok(());
            }
            drop(out_fds);
        }
        if close {
            return Ok(());
        }
    }
}

/// Dispatch one message. Returns `(reply, fds_to_send, should_close)`.
fn handle(daemon: &Arc<Daemon>, msg: Message) -> (Option<Message>, Vec<OwnedFd>, bool) {
    match msg {
        Message::Ping => (Some(Message::Pong), vec![], false),
        Message::Hello { .. } | Message::Welcome { .. } => (None, vec![], false),

        Message::GetProp { name } => {
            let props = daemon.props.lock().unwrap();
            let value = props.get(&name).map(|s| s.to_string());
            (Some(Message::PropValue { name, value }), vec![], false)
        }

        Message::SetProp { name, value } => {
            daemon.props.lock().unwrap().set(&name, &value);
            crate::debug!("setprop {name}={value}");
            (Some(Message::Ack), vec![], false)
        }

        Message::AllocShmem { name, size } => match AshmemRegion::create(&name, size) {
            Ok(region) => {
                let fd = match region.try_clone_fd() {
                    Ok(fd) => fd,
                    Err(e) => {
                        return (
                            Some(Message::Error { code: -5, message: format!("dup fd: {e}") }),
                            vec![],
                            false,
                        )
                    }
                };
                let seam = region.seal().is_ok();
                daemon.shmem_regions.lock().unwrap().push((name.clone(), size));
                crate::info!("allocated ashmem '{name}' ({size} bytes)");
                (Some(Message::ShmemAllocd { name, size, seam }), vec![fd], false)
            }
            Err(e) => (
                Some(Message::Error { code: -12, message: format!("ashmem create: {e}") }),
                vec![],
                false,
            ),
        },

        Message::RegisterService { name, iface } => {
            let handle = daemon.services.lock().unwrap().register(
                &name,
                &iface,
                crate::services::ServiceKind::Guest,
            );
            crate::info!("guest registered service '{name}' ({iface}) -> handle {handle}");
            (Some(Message::ServiceRegistered { handle }), vec![], false)
        }

        Message::BinderTransact { handle, code, flags, data } => {
            let reply = daemon.services.lock().unwrap().transact(handle, code, flags, &data);
            let reply = crate::stubs::clamp(reply);
            (Some(Message::BinderReply { status: reply.status, data: reply.data }), vec![], false)
        }

        Message::CreateSurface { width, height, title } => {
            let (w, h) = if width == 0 || height == 0 {
                (DEFAULT_SURFACE_W, DEFAULT_SURFACE_H)
            } else {
                (width, height)
            };
            let mut guard = daemon.graphics.lock().unwrap();
            let Some(gfx) = guard.as_mut() else {
                return (
                    Some(Message::Error {
                        code: -19,
                        message: "graphics unavailable (no Wayland session)".into(),
                    }),
                    vec![],
                    false,
                );
            };
            match gfx.create_toplevel(w, h, &title) {
                Some(info) => {
                    let fd = gfx.surface_fd(info.id);
                    crate::info!("created surface {} '{}' {}×{}", info.id, title, w, h);
                    let reply = Message::SurfaceCreated {
                        surface_id: info.id,
                        shm_name: info.shm_name,
                        stride: info.stride,
                        size: info.size,
                        format: info.format,
                    };
                    (Some(reply), fd.into_iter().collect(), false)
                }
                None => (
                    Some(Message::Error { code: -19, message: "surface creation failed".into() }),
                    vec![],
                    false,
                ),
            }
        }

        Message::PresentFrame { surface_id } => {
            let mut guard = daemon.graphics.lock().unwrap();
            if let Some(gfx) = guard.as_mut() {
                gfx.present(surface_id);
            }
            (Some(Message::Ack), vec![], false)
        }

        Message::RegisterProc { pid, package, component, surface_id } => {
            let mut procs = daemon.procs.lock().unwrap();
            procs.retain(|p| p.pid != pid);
            procs.push(ProcInfo { pid, package, component, surface_id });
            crate::info!("tracking guest pid {pid}");
            (Some(Message::Ack), vec![], false)
        }

        Message::ListProcs => {
            let mut procs = daemon.procs.lock().unwrap();
            procs.retain(|p| pid_alive(p.pid));
            (Some(Message::ProcList { procs: procs.clone() }), vec![], false)
        }

        Message::RunApk { path, .. } => (
            Some(Message::Error {
                code: -38,
                message: format!(
                    "RunApk is driven by the CLI in this MVP; use CreateSurface+RegisterProc for {path}"
                ),
            }),
            vec![],
            false,
        ),

        Message::Shutdown => {
            crate::info!("shutdown requested by client");
            SHUTDOWN.store(true, Ordering::Relaxed);
            (Some(Message::Goodbye), vec![], true)
        }
        Message::Goodbye => (None, vec![], true),
        Message::Error { .. } | Message::ProcList { .. } | Message::ShmemAllocd { .. }
        | Message::ServiceRegistered { .. } | Message::BinderReply { .. }
        | Message::SurfaceCreated { .. } | Message::PropValue { .. } | Message::Pong
        | Message::Ack | Message::RunStarted { .. } => {
            crate::debug!("unexpected inbound message kind {:?}", msg.kind());
            (None, vec![], false)
        }
    }
}

/// Is `pid` still a live process? Uses `/proc`, so it needs no privileges.
fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_daemon() -> Arc<Daemon> {
        let dir = std::env::temp_dir();
        Arc::new(Daemon::new(
            dir.join("pd-test.sock"),
            dir.join("pd-test.props"),
            false, // no Wayland in unit tests
        ))
    }

    #[test]
    fn ping_pong() {
        let d = test_daemon();
        let (reply, fds, close) = handle(&d, Message::Ping);
        assert_eq!(reply, Some(Message::Pong));
        assert!(fds.is_empty() && !close);
    }

    #[test]
    fn getprop_reports_sdk() {
        let d = test_daemon();
        let (reply, _, _) = handle(&d, Message::GetProp { name: "ro.build.version.sdk".into() });
        assert_eq!(
            reply,
            Some(Message::PropValue {
                name: "ro.build.version.sdk".into(),
                value: Some("36".into())
            })
        );
    }

    #[test]
    fn alloc_shmem_returns_a_fd() {
        let d = test_daemon();
        let (reply, fds, _) = handle(&d, Message::AllocShmem { name: "t".into(), size: 4096 });
        assert!(matches!(reply, Some(Message::ShmemAllocd { .. })));
        assert_eq!(fds.len(), 1);
        assert!(fds[0].as_raw_fd() >= 0);
    }

    #[test]
    fn create_surface_without_wayland_errors() {
        let d = test_daemon();
        let (reply, fds, _) = handle(
            &d,
            Message::CreateSurface { width: 100, height: 100, title: "x".into() },
        );
        assert!(matches!(reply, Some(Message::Error { code: -19, .. })));
        assert!(fds.is_empty());
    }

    #[test]
    fn register_and_list_procs() {
        let d = test_daemon();
        let me = std::process::id() as i32;
        let (r, _, _) = handle(
            &d,
            Message::RegisterProc {
                pid: me,
                package: "com.x".into(),
                component: "com.x/.M".into(),
                surface_id: 1,
            },
        );
        assert_eq!(r, Some(Message::Ack));
        let (r, _, _) = handle(&d, Message::ListProcs);
        match r {
            Some(Message::ProcList { procs }) => {
                assert_eq!(procs.len(), 1);
                assert_eq!(procs[0].pid, me);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn binder_transact_to_camera_stub() {
        let d = test_daemon();
        let handle_id = d.services.lock().unwrap().lookup("media.camera").unwrap();
        let (reply, _, _) = handle(
            &d,
            Message::BinderTransact { handle: handle_id, code: 1, flags: 0, data: vec![] },
        );
        assert_eq!(reply, Some(Message::BinderReply { status: 0, data: 0i32.to_le_bytes().to_vec() }));
    }
}
