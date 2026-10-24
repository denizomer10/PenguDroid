//! The daemon's server core: shared state, the accept loop, and message
//! dispatch.
//!
//! One thread per client connection. Shared state is behind mutexes; nothing on
//! the hot path is more contended than a `Mutex` acquisition, and the graphics
//! broker (the one genuinely stateful, non-`Send`-friendly component) is
//! serialized behind a single mutex because Wayland's `EventQueue` must not be
//! driven concurrently.
//!
//! ## Connections and guest services
//!
//! Every connection has an inbox (`mpsc::Receiver<Message>`). A request is
//! normally answered on the caller's own thread; the inbox is only used for
//! server-initiated traffic — a `GuestCall` aimed at a service that the client
//! registered. The serving thread selects between the socket and the inbox, so
//! nested calls work without two threads deadlocking on each other's socket.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
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

/// How long the daemon waits for a guest service to answer a forwarded call.
const GUEST_CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the connection loop looks at its inbox while blocked on the
/// socket. Short enough to keep nested calls responsive, and free on an idle
/// daemon.
const INBOX_POLL: Duration = Duration::from_millis(5);

pub struct Daemon {
    pub start: Instant,
    props: Mutex<Props>,
    pub(crate) services: Mutex<ServiceTable>,
    procs: Mutex<Vec<ProcInfo>>,
    graphics: Mutex<Option<Graphics>>,
    audio: AudioBroker,
    binder: BinderBackend,
    shmem_regions: Mutex<Vec<(String, u64)>>,
    props_path: PathBuf,
    socket_path: PathBuf,
    /// The CLI binary used to fork guests for `RunApk`.
    cli_path: Option<PathBuf>,
    /// Threads that finish a `RunApk` child so the daemon never keeps zombies
    /// and can report the exit status.
    pub(crate) spawned: Mutex<Vec<SpawnedRun>>,
    /// Connections that asked for input on a surface, keyed by surface id.
    /// The sender is the peer's inbox, so input travels the same path as
    /// forwarded guest calls.
    input_sinks: Mutex<std::collections::HashMap<u32, Sender<Message>>>,
}

pub(crate) struct SpawnedRun {
    pub pid: i32,
    pub status: Option<RunExit>,
    /// A client has already collected `status` through `QueryRun`.
    pub reported: bool,
}

/// A decoded child status.
#[derive(Clone, Copy)]
pub(crate) struct RunExit {
    pub code: i32,
    pub signal: i32,
}

impl RunExit {
    pub fn describe(&self) -> String {
        if self.signal != 0 {
            format!("signal {}", self.signal)
        } else {
            format!("exit code {}", self.code)
        }
    }
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
            cli_path: find_cli_binary(),
            spawned: Mutex::new(Vec::new()),
            input_sinks: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Override the CLI used to fork `RunApk` guests (tests and unusual
    /// layouts).
    pub fn set_cli_path(&mut self, path: PathBuf) {
        self.cli_path = Some(path);
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
        crate::info!(
            "daemon ready on {} ({} ms startup)",
            self.socket_path.display(),
            self.start.elapsed().as_millis()
        );
        crate::info!("{}", self.summary());
        while !SHUTDOWN.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok(conn) => self.spawn_client(conn),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    self.reap_children();
                    self.reap_surfaces();
                    // This loop is also what reads the compositor's events, so the
                    // sleep is the input latency: 50 ms was noticeable when
                    // dragging. 8 ms keeps a click feeling immediate without
                    // burning CPU.
                    self.pump_input();
                    std::thread::sleep(Duration::from_millis(8));
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

    /// Serve one accepted connection on its own thread.
    pub(crate) fn spawn_client(self: &Arc<Self>, conn: Connection) {
        let d = Arc::clone(self);
        std::thread::spawn(move || {
            if let Err(e) = serve(d, conn) {
                crate::debug!("client disconnected: {e}");
            }
        });
    }

    /// Notice surfaces whose `xdg_toplevel` was closed by the compositor and ask
    /// the guest owning them to exit.
    pub fn reap_surfaces(&self) {
        let closed = {
            let mut guard = self.graphics.lock().unwrap();
            match guard.as_mut() {
                Some(gfx) => gfx.close_requests(),
                None => return,
            }
        };
        if closed.is_empty() {
            return;
        }
        let procs = self.procs.lock().unwrap();
        for id in closed {
            crate::info!("surface {id} closed by the compositor");
            if let Some(p) = procs.iter().find(|p| p.surface_id == id) {
                if p.pid > 0 {
                    // SAFETY: plain `kill(2)` on a pid recorded by the guest.
                    unsafe { libc::kill(p.pid, libc::SIGTERM) };
                }
            }
        }
    }

    /// Forward compositor input to the connections that subscribed for it.
    ///
    /// Called from the accept loop, which is also what drives the Wayland
    /// event queue, so a pointer event reaches the guest within one poll.
    pub fn pump_input(&self) {
        let events = {
            let mut guard = self.graphics.lock().unwrap();
            match guard.as_mut() {
                Some(gfx) => gfx.poll_input(),
                None => return,
            }
        };
        if events.is_empty() {
            return;
        }
        let sinks = self.input_sinks.lock().unwrap();
        for (surface_id, event) in events {
            if let Some(tx) = sinks.get(&surface_id) {
                let _ = tx.send(Message::InputEvent { surface_id, event });
            }
        }
    }

    /// Record a child's exit status (called by the `RunApk` waiter thread).
    pub(crate) fn note_exit(&self, pid: i32, exit: RunExit) {
        let mut spawned = self.spawned.lock().unwrap();
        if let Some(run) = spawned.iter_mut().find(|r| r.pid == pid) {
            run.status = Some(exit);
        }
        crate::info!("guest pid {pid} exited ({})", exit.describe());
    }

    /// Reap `RunApk` children.
    ///
    /// The waiter threads do the real `wait`. An exit status that no client has
    /// collected yet is *kept* even after the child is gone, so `QueryRun` can
    /// always report a guest that finished quickly; a reported entry is dropped
    /// (it will never be asked for again). Entries that were never reported and
    /// whose pid no longer exists are still kept, which bounds the table to the
    /// number of launches.
    pub fn reap_children(&self) {
        let mut spawned = self.spawned.lock().unwrap();
        spawned.retain(|r| match r.status {
            None => true,
            Some(_) => !r.reported,
        });
    }
}

/// The `pengudroid` CLI next to this daemon, used to fork `RunApk` guests.
fn find_cli_binary() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("PENGUDROID_CLI") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let candidate = dir.join("pengudroid");
    candidate.exists().then_some(candidate)
}

/// State a single connection needs while it is being served.
struct Peer {
    pid: i32,
    /// Server-initiated messages (forwarded `GuestCall`s) for this peer.
    inbox: Receiver<Message>,
    /// Replies to calls this connection forwarded to other guests.
    pending: Arc<Mutex<Vec<Message>>>,
}

/// Perform the handshake and then serve requests until the peer leaves.
fn serve(daemon: Arc<Daemon>, conn: Connection) -> io::Result<()> {
    let (hello, _) = conn
        .recv_message()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let (_role, pid) = match hello {
        Message::Hello { version, role, pid } => {
            if version != PROTO_VERSION {
                crate::warn!("client protocol v{version} != daemon v{PROTO_VERSION}");
            }
            crate::debug!("client connected: role={role:?} pid={pid}");
            let _ = conn.send_message(
                &Message::Welcome { version: PROTO_VERSION, pid: std::process::id() as i32 },
                &[],
            );
            (role, pid)
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

    let (tx, inbox) = std::sync::mpsc::channel::<Message>();
    let pending: Arc<Mutex<Vec<Message>>> = Arc::new(Mutex::new(Vec::new()));
    let peer = Peer { pid, inbox, pending };
    // `inbox_tx` is cloned into every service registration; keeping the original
    // alive means the receiver is never spuriously disconnected.
    let _keepalive = tx;

    // Blocking reads are the common case; a timeout lets the loop notice both
    // forwarded calls and (from the peer's side) the socket going away.
    conn.set_read_timeout(Some(INBOX_POLL))
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

    let result = loop {
        // Deliver anything queued for this peer before waiting again, so a
        // guest call cannot be starved by a busy requester.
        if !drain_inbox(&peer.inbox, &conn) {
            break Ok(());
        }

        let received = match conn.recv_message() {
            Ok(v) => v,
            Err(pengudroid_socket::SocketError::Io(e))
                if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) =>
            {
                if peer.inbox.try_recv().is_ok() {
                    // Something arrived while we slept: loop to drain it.
                }
                continue;
            }
            Err(pengudroid_socket::SocketError::Io(e))
                if e.kind() == io::ErrorKind::UnexpectedEof =>
            {
                crate::debug!("client closed connection");
                break Ok(());
            }
            Err(e) => break Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
        };
        let (msg, raw_fds) = received;
        let (reply, out_fds, close) = match msg {
            Message::PresentDmabuf { surface_id, width, height, stride, fourcc, modifier, offset } => {
                // SAFETY: these fds arrived via SCM_RIGHTS and are now ours.
                let in_fds: Vec<OwnedFd> = raw_fds
                    .into_iter()
                    .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) })
                    .collect();
                handle_present_dmabuf(
                    &daemon, surface_id, width, height, stride, fourcc, modifier, offset, in_fds,
                )
            }
            other => {
                // Any fds attached to other messages are not expected; drop
                // them (closing them) rather than leaking.
                let _unexpected: Vec<OwnedFd> = raw_fds
                    .into_iter()
                    .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) })
                    .collect();
                handle(&daemon, &peer, &_keepalive, other)
            }
        };
        if let Some(r) = reply {
            let raw: Vec<RawFd> = out_fds.iter().map(|f| f.as_raw_fd()).collect();
            // `out_fds` must outlive the send call.
            if let Err(e) = conn.send_message(&r, &raw) {
                crate::debug!("send failed: {e}");
                break Ok(());
            }
            drop(out_fds);
        }
        if close {
            break Ok(());
        }
    };

    // The connection is over: drop the guest services it published.
    //
    // The peer's `Hello` pid is only a hint (short-lived tools and tests often
    // share a pid with a guest they talk to), so the process table is *not*
    // touched by pid here; a guest's own disconnect is handled by its
    // `UnregisterProc` or, failing that, by `ListProcs` liveness pruning.
    forget_peer(&daemon, &peer);
    result
}

/// Release everything bound to a connection that is closing.
fn forget_peer(daemon: &Arc<Daemon>, peer: &Peer) {
    let removed = daemon.services.lock().unwrap().remove_owned_by(peer.pid);
    if !removed.is_empty() {
        crate::info!(
            "connection pid {} closed; dropped {} service(s)",
            peer.pid,
            removed.len()
        );
    }
}

/// Dispatch one message. Returns `(reply, fds_to_send, should_close)`.
fn handle(
    daemon: &Arc<Daemon>,
    peer: &Peer,
    inbox: &Sender<Message>,
    msg: Message,
) -> (Option<Message>, Vec<OwnedFd>, bool) {
    let cli_pid = peer.pid;
    match msg {
        Message::Ping => (Some(Message::Pong), vec![], false),
        Message::Hello { .. } | Message::Welcome { .. } => (None, vec![], false),

        Message::SubscribeInput { surface_id } => {
            daemon
                .input_sinks
                .lock()
                .unwrap()
                .insert(surface_id, inbox.clone());
            crate::info!("pid {} subscribed to input on surface {surface_id}", peer.pid);
            (Some(Message::Ack), vec![], false)
        }
        // A *client* sending `InputEvent` is an injection request: the daemon
        // broadcasts it to whatever subscribed for that surface. Guests never
        // send these (the compositor feeds them), so the two directions do not
        // collide. `surface_id == 0` means "the surface under the pointer".
        Message::InputEvent { surface_id, event } => {
            let target = if surface_id != 0 {
                Some(surface_id)
            } else {
                let mut guard = daemon.graphics.lock().unwrap();
                guard.as_ref().and_then(|g| g.focused_surface())
                    .or_else(|| {
                        guard.as_ref().and_then(|g| g.surface_ids().first().copied())
                    })
            };
            if let Some(id) = target {
                let sinks = daemon.input_sinks.lock().unwrap();
                if let Some(tx) = sinks.get(&id) {
                    let _ = tx.send(Message::InputEvent { surface_id: id, event });
                    crate::info!("injected {:?} on surface {id}", event);
                } else {
                    crate::warn!("input injected for surface {id} with no subscriber");
                }
            } else {
                crate::warn!("input injected but no surface is known");
            }
            (Some(Message::Ack), vec![], false)
        }

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

        Message::ListProps => {
            let props = daemon.props.lock().unwrap();
            let pairs: Vec<(String, String)> =
                props.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            (Some(Message::PropList { pairs, more: false }), vec![], false)
        }

        Message::ListServices => {
            let services = daemon.services.lock().unwrap();
            let list = services
                .list()
                .into_iter()
                .map(|(name, iface, handle)| (name.to_string(), iface.to_string(), handle))
                .collect();
            (Some(Message::ServiceList { services: list }), vec![], false)
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
            let handle = daemon
                .services
                .lock()
                .unwrap()
                .register_guest(&name, &iface, inbox.clone(), cli_pid);
            match handle {
                Some(handle) => {
                    crate::info!(
                        "guest {cli_pid} registered service '{name}' ({iface}) -> handle {handle}"
                    );
                    (Some(Message::ServiceRegistered { handle }), vec![], false)
                }
                None => (
                    Some(Message::Error {
                        code: -17,
                        message: format!("service '{name}' is already provided by the platform"),
                    }),
                    vec![],
                    false,
                ),
            }
        }

        Message::BinderTransact { handle, code, flags, data } => {
            let owner = daemon.services.lock().unwrap().guest_owner(handle).cloned();
            match owner {
                Some(owner) => forward_guest_call(peer, &owner, code, data),
                None => {
                    let reply = daemon.services.lock().unwrap().transact(handle, code, flags, &data);
                    let reply = crate::stubs::clamp(reply);
                    (Some(Message::BinderReply { status: reply.status, data: reply.data }), vec![], false)
                }
            }
        }

        Message::GuestCallReply { status, data } => {
            peer.pending.lock().unwrap().push(Message::BinderReply { status, data });
            (None, vec![], false)
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
            let presented = match guard.as_mut() {
                Some(gfx) => gfx.present(surface_id),
                None => false,
            };
            // A frame for a surface that no longer exists (or whose window was
            // closed) is reported as an error so the guest stops repainting.
            if presented {
                (Some(Message::Ack), vec![], false)
            } else {
                (
                    Some(Message::Error {
                        code: -19,
                        message: format!("surface {surface_id} is gone or closed"),
                    }),
                    vec![],
                    false,
                )
            }
        }

        Message::RegisterProc { pid, package, component, surface_id } => {
            let mut procs = daemon.procs.lock().unwrap();
            procs.retain(|p| p.pid != pid);
            procs.push(ProcInfo { pid, package, component, surface_id });
            crate::info!("tracking guest pid {pid}");
            (Some(Message::Ack), vec![], false)
        }

        Message::UnregisterProc { pid } => {
            daemon.procs.lock().unwrap().retain(|p| p.pid != pid);
            let removed = daemon.services.lock().unwrap().remove_owned_by(pid);
            crate::debug!("guest pid {pid} unregistered ({} service(s) dropped)", removed.len());
            (Some(Message::Ack), vec![], false)
        }

        Message::ListProcs => {
            let mut procs = daemon.procs.lock().unwrap();
            procs.retain(|p| pid_alive(p.pid));
            (Some(Message::ProcList { procs: procs.clone() }), vec![], false)
        }

        Message::RunApk { path, component, args } => {
            (Some(run_apk(daemon, &path, component, &args)), vec![], false)
        }

        Message::QueryRun { pid } => {
            let mut spawned = daemon.spawned.lock().unwrap();
            let reply = match spawned.iter_mut().find(|r| r.pid == pid) {
                Some(run) => match run.status {
                    Some(exit) => {
                        // One collector: the reporter needs the status once.
                        run.reported = true;
                        Some(Message::RunStatus {
                            pid,
                            running: false,
                            exit_code: exit.code,
                            signal: exit.signal,
                        })
                    }
                    None => Some(Message::RunStatus {
                        pid,
                        running: pid_alive(pid),
                        exit_code: 0,
                        signal: 0,
                    }),
                },
                None => Some(Message::Error {
                    code: -3,
                    message: format!("pid {pid} was not started by RunApk"),
                }),
            };
            (reply, vec![], false)
        }

        Message::GetAudioRing => {
            let (rate, channels, size) = daemon.audio.params();
            match daemon.audio.dup_ring_fd() {
                Ok(fd) => (
                    Some(Message::AudioRing { rate, channels, size }),
                    vec![fd],
                    false,
                ),
                Err(e) => (
                    Some(Message::Error { code: -5, message: format!("audio ring fd: {e}") }),
                    vec![],
                    false,
                ),
            }
        }

        Message::AudioWrite { offset, len } => match daemon.audio.write_ring(offset, len) {
            Ok(()) => (Some(Message::Ack), vec![], false),
            Err(e) => (Some(Message::Error { code: -5, message: e }), vec![], false),
        },

        Message::Shutdown => {
            crate::info!("shutdown requested by client");
            SHUTDOWN.store(true, Ordering::Relaxed);
            (Some(Message::Goodbye), vec![], true)
        }
        Message::PresentDmabuf { .. } => (
            Some(Message::Error {
                code: -22,
                message: "PresentDmabuf must carry a dma-buf fd".into(),
            }),
            vec![],
            false,
        ),
        Message::Goodbye => (None, vec![], true),
        Message::Error { .. }
        | Message::ProcList { .. }
        | Message::ShmemAllocd { .. }
        | Message::ServiceRegistered { .. }
        | Message::BinderReply { .. }
        | Message::SurfaceCreated { .. }
        | Message::PropValue { .. }
        | Message::PropList { .. }
        | Message::ServiceList { .. }
        | Message::RunStatus { .. }
        | Message::GuestCall { .. }
        | Message::RunStarted { .. }
        | Message::AudioRing { .. }
        | Message::Pong
        | Message::Ack => {
            crate::debug!("unexpected inbound message kind {:?}", msg.kind());
            (None, vec![], false)
        }
    }
}

include!("server_forward.rs");

/// Spawn `pengudroid __guest ...` for an APK, detached from the caller.
fn run_apk(daemon: &Arc<Daemon>, path: &str, component: Option<String>, args: &[String]) -> Message {
    let Some(cli) = daemon.cli_path.clone() else {
        return Message::Error {
            code: -2,
            message: "the `pengudroid` CLI was not found next to the daemon \
                      (set PENGUDROID_CLI); RunApk needs it to fork guests"
                .into(),
        };
    };
    // `pengudroid start --demo` addresses the built-in demo with a marker path.
    let demo = path == "-";
    if !demo && !std::path::Path::new(path).exists() {
        return Message::Error { code: -2, message: format!("no such file: {path}") };
    }
    let mut cmd = std::process::Command::new(&cli);
    cmd.arg("__guest").arg("--socket").arg(&daemon.socket_path);
    if demo {
        cmd.arg("--package")
            .arg("com.pengudroid.demo")
            .arg("--title")
            .arg("PenguDroid demo");
    } else {
        cmd.arg("--apk").arg(path);
    }
    if let Some(c) = &component {
        cmd.arg("--component").arg(c);
    }
    cmd.args(args);
    // A daemon started before the session existed (or from a cron job) still
    // hands the guest the session when the caller knows it.
    for var in ["WAYLAND_DISPLAY", "XDG_RUNTIME_DIR", "DISPLAY", "PULSE_SERVER"] {
        if std::env::var_os(var).is_some() {
            cmd.env(var, std::env::var_os(var).unwrap());
        }
    }

    match cmd.spawn() {
        Ok(mut child) => {
            let pid = child.id() as i32;
            daemon.spawned.lock().unwrap().push(SpawnedRun { pid, status: None, reported: false });
            crate::info!("RunApk: started guest pid {pid} for {path}");
            let d = Arc::clone(daemon);
            std::thread::spawn(move || {
                if let Ok(status) = child.wait() {
                    let d = d;
                    d.note_exit(pid, decode_status(status));
                }
            });
            // The package is known once the guest registers; the surface id is
            // reported by the guest in `pengudroid ps`/`ps` itself.
            Message::RunStarted { pid, package: String::new(), surface_id: 0 }
        }
        Err(e) => {
            Message::Error { code: -5, message: format!("cannot spawn `{}`: {e}", cli.display()) }
        }
    }
}

fn decode_status(status: std::process::ExitStatus) -> RunExit {
    use std::os::unix::process::ExitStatusExt;
    RunExit { code: status.code().unwrap_or(0), signal: status.signal().unwrap_or(0) }
}

/// Handle a `PresentDmabuf`: present a GPU-rendered buffer on an existing
/// surface, zero-copy.
#[allow(clippy::too_many_arguments)]
fn handle_present_dmabuf(
    daemon: &Arc<Daemon>,
    surface_id: u32,
    width: u32,
    height: u32,
    stride: u32,
    fourcc: u32,
    modifier: u64,
    offset: u32,
    in_fds: Vec<OwnedFd>,
) -> (Option<Message>, Vec<OwnedFd>, bool) {
    let mut fds = in_fds.into_iter();
    let Some(fd) = fds.next() else {
        return (
            Some(Message::Error { code: -22, message: "PresentDmabuf: no fd attached".into() }),
            vec![],
            false,
        );
    };
    let mut guard = daemon.graphics.lock().unwrap();
    let Some(gfx) = guard.as_mut() else {
        return (
            Some(Message::Error { code: -19, message: "graphics unavailable".into() }),
            vec![],
            false,
        );
    };
    match gfx.present_dmabuf(surface_id, fd, width, height, stride, fourcc, modifier, offset) {
        Ok(()) => {
            crate::debug!("presented dmabuf on surface {surface_id} ({width}x{height})");
            (Some(Message::Ack), vec![], false)
        }
        Err(e) => (Some(Message::Error { code: -5, message: e }), vec![], false),
    }
}

/// Is `pid` still a live process? Uses `/proc`, so it needs no privileges.
pub(crate) fn pid_alive(pid: i32) -> bool {
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

    /// A peer not backed by a socket. `handle` never touches the socket, so this
    /// is enough to exercise every dispatch path.
    fn peer(_role: pengudroid_proto::Role) -> (Peer, Sender<Message>) {
        let (tx, inbox) = std::sync::mpsc::channel();
        let peer = Peer {
            pid: std::process::id() as i32,
            inbox,
            pending: Arc::new(Mutex::new(Vec::new())),
        };
        (peer, tx)
    }

    fn call(daemon: &Arc<Daemon>, peer: &Peer, m: Message) -> (Option<Message>, Vec<OwnedFd>, bool) {
        let (tx, _rx) = std::sync::mpsc::channel();
        handle(daemon, peer, &tx, m)
    }

    #[test]
    fn ping_pong() {
        let d = test_daemon();
        let (p, _tx) = peer(pengudroid_proto::Role::Cli);
        let (reply, fds, close) = call(&d, &p, Message::Ping);
        assert_eq!(reply, Some(Message::Pong));
        assert!(fds.is_empty() && !close);
    }

    #[test]
    fn getprop_reports_sdk() {
        let d = test_daemon();
        let (p, _tx) = peer(pengudroid_proto::Role::Cli);
        let (reply, _, _) = call(&d, &p, Message::GetProp { name: "ro.build.version.sdk".into() });
        assert_eq!(
            reply,
            Some(Message::PropValue {
                name: "ro.build.version.sdk".into(),
                value: Some("36".into())
            })
        );
    }

    #[test]
    fn list_props_enumerates_the_store() {
        let d = test_daemon();
        let (p, _tx) = peer(pengudroid_proto::Role::Cli);
        match call(&d, &p, Message::ListProps).0 {
            Some(Message::PropList { pairs, more }) => {
                assert!(!more);
                assert!(pairs.iter().any(|(k, v)| k == "ro.build.version.sdk" && v == "36"));
                assert!(pairs.windows(2).all(|w| w[0].0 < w[1].0), "ordered by name");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn list_services_reports_platform_stubs() {
        let d = test_daemon();
        let (p, _tx) = peer(pengudroid_proto::Role::Cli);
        match call(&d, &p, Message::ListServices).0 {
            Some(Message::ServiceList { services }) => {
                assert!(services
                    .iter()
                    .any(|(n, i, _)| n == "activity" && i.contains("IActivityManager")));
                assert!(services.len() >= 6);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn guest_can_register_a_service() {
        let d = test_daemon();
        let (p, _tx) = peer(pengudroid_proto::Role::Guest);
        let reply = call(&d, &p, Message::RegisterService { name: "com.x.Svc".into(), iface: "IX".into() }).0;
        match reply {
            Some(Message::ServiceRegistered { handle }) => {
                assert!(d.services.lock().unwrap().guest_owner(handle).is_some());
            }
            other => panic!("unexpected {other:?}"),
        }
        // A guest may not shadow a platform service.
        let reply = call(&d, &p, Message::RegisterService { name: "activity".into(), iface: "IX".into() }).0;
        assert!(matches!(reply, Some(Message::Error { code: -17, .. })));
    }

    #[test]
    fn alloc_shmem_returns_a_fd() {
        let d = test_daemon();
        let (p, _tx) = peer(pengudroid_proto::Role::Guest);
        let (reply, fds, _) = call(&d, &p, Message::AllocShmem { name: "t".into(), size: 4096 });
        assert!(matches!(reply, Some(Message::ShmemAllocd { .. })));
        assert_eq!(fds.len(), 1);
        assert!(fds[0].as_raw_fd() >= 0);
    }

    #[test]
    fn create_surface_without_wayland_errors() {
        let d = test_daemon();
        let (p, _tx) = peer(pengudroid_proto::Role::Guest);
        let (reply, fds, _) =
            call(&d, &p, Message::CreateSurface { width: 100, height: 100, title: "x".into() });
        assert!(matches!(reply, Some(Message::Error { code: -19, .. })));
        assert!(fds.is_empty());
    }

    #[test]
    fn register_and_list_procs() {
        let d = test_daemon();
        let me = std::process::id() as i32;
        let (p, _tx) = peer(pengudroid_proto::Role::Guest);
        let (r, _, _) = call(
            &d,
            &p,
            Message::RegisterProc {
                pid: me,
                package: "com.x".into(),
                component: "com.x/.M".into(),
                surface_id: 1,
            },
        );
        assert_eq!(r, Some(Message::Ack));

        let (cli, _tx2) = peer(pengudroid_proto::Role::Cli);
        match call(&d, &cli, Message::ListProcs).0 {
            Some(Message::ProcList { procs }) => {
                assert_eq!(procs.len(), 1);
                assert_eq!(procs[0].pid, me);
            }
            other => panic!("unexpected {other:?}"),
        }

        let _ = call(&d, &p, Message::UnregisterProc { pid: me });
        match call(&d, &cli, Message::ListProcs).0 {
            Some(Message::ProcList { procs }) => assert!(procs.is_empty()),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn binder_transact_to_camera_stub() {
        let d = test_daemon();
        let handle_id = d.services.lock().unwrap().lookup("media.camera").unwrap();
        let (p, _tx) = peer(pengudroid_proto::Role::Cli);
        let (reply, _, _) = call(
            &d,
            &p,
            Message::BinderTransact { handle: handle_id, code: 1, flags: 0, data: vec![] },
        );
        assert_eq!(reply, Some(Message::BinderReply { status: 0, data: 0i32.to_le_bytes().to_vec() }));
    }

    #[test]
    fn binder_transact_to_unknown_handle_is_invalid() {
        let d = test_daemon();
        let (p, _tx) = peer(pengudroid_proto::Role::Cli);
        let (reply, _, _) = call(
            &d,
            &p,
            Message::BinderTransact { handle: 9999, code: 0, flags: 0, data: vec![] },
        );
        assert_eq!(reply, Some(Message::BinderReply { status: -22, data: vec![] }));
    }

    #[test]
    fn query_run_unknown_pid_is_an_error() {
        let d = test_daemon();
        let (p, _tx) = peer(pengudroid_proto::Role::Cli);
        let (reply, _, _) = call(&d, &p, Message::QueryRun { pid: 999_999 });
        assert!(matches!(reply, Some(Message::Error { code: -3, .. })));
    }

    #[test]
    fn run_apk_reports_missing_cli_and_missing_file() {
        let mut daemon = Daemon::new(
            std::env::temp_dir().join("pd-test.sock"),
            std::env::temp_dir().join("pd-test.props"),
            false,
        );
        daemon.cli_path = None;
        let d = Arc::new(daemon);
        let (p, _tx) = peer(pengudroid_proto::Role::Cli);
        let (reply, _, _) =
            call(&d, &p, Message::RunApk { path: "/nonexistent.apk".into(), component: None, args: vec![] });
        assert!(matches!(reply, Some(Message::Error { code: -2, .. })));
    }
}
