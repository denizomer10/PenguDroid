//! PenguDroid wire protocol.
//!
//! This crate is shared by the daemon, the `pengudroid` CLI, guest processes
//! and the Bionic translation shim. It has **no external dependencies** so it
//! can be linked into an `LD_PRELOAD` shim without dragging anything along.
//!
//! # Framing
//!
//! Every message is a single frame:
//!
//! ```text
//! +---------------------+-----------+----------------------------+
//! | u32 payload_len (LE)| kind: u8  | body...                    |
//! +---------------------+-----------+----------------------------+
//! |<------------ payload_len bytes -------------- >|
//! ```
//!
//! The `payload_len` covers the kind byte and the body. File descriptors are
//! *never* placed in the body; they are passed out-of-band with `SCM_RIGHTS`
//! on the same Unix socket (see `pengudroid-daemon::ipc`).

#![forbid(unsafe_code)]

use std::fmt;

/// Protocol version negotiated during the `Hello`/`Welcome` handshake.
///
/// v2 added property/service enumeration, guest-to-guest calls, process
/// unregistration and detached launches (`QueryRun`).
pub const PROTO_VERSION: u16 = 2;

/// Marker used by the daemon's pid/heartbeat file.
pub const PROTO_MAGIC: u32 = 0x5044_5247; // "PDRG"

/// Maximum accepted frame size. Guards against a hostile peer announcing a
/// multi-gigabyte frame and forcing us to allocate it.
pub const MAX_FRAME_LEN: u32 = 16 * 1024 * 1024;

/// Discriminant for every message exchanged on the control socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MsgKind {
    Hello = 1,
    Welcome = 2,
    Error = 3,
    Ping = 4,
    Pong = 5,

    RunApk = 10,
    RunStarted = 11,
    RegisterProc = 12,

    ListProcs = 20,
    ProcList = 21,

    AllocShmem = 30,
    ShmemAllocd = 31,

    RegisterService = 40,
    ServiceRegistered = 41,

    BinderTransact = 50,
    BinderReply = 51,

    CreateSurface = 60,
    SurfaceCreated = 61,
    PresentFrame = 62,
    /// Present a GPU-rendered dma-buf on an existing surface (fd via SCM_RIGHTS).
    PresentDmabuf = 63,
    /// Create a `wl_subsurface` of an existing surface, for a child layer such
    /// as a `SurfaceView`'s video output.
    CreateSubsurface = 64,
    SubsurfaceCreated = 65,
    /// Move a subsurface inside its parent (parent-local pixels).
    SetSubsurfacePosition = 66,
    DestroySurface = 67,
    PresentShm = 68,
    SetSurfaceOptions = 69,

    GetProp = 70,
    PropValue = 71,
    SetProp = 72,
    ListProps = 73,
    PropList = 74,

    ListServices = 75,
    ServiceList = 76,

    GuestCall = 77,
    GuestCallReply = 78,
    UnregisterProc = 79,

    QueryRun = 82,
    RunStatus = 83,

    GetAudioRing = 84,
    AudioRing = 85,
    AudioWrite = 86,

    SubscribeInput = 87,
    InputEvent = 88,

    Ack = 80,

    Shutdown = 90,
    Goodbye = 91,
    VideoDecoderOpen = 92,
    VideoDecoderReady = 93,
    VideoPacket = 94,
    VideoDecodeStatus = 95,
    VideoDecoderFlush = 96,
    VideoDecoderClose = 97,
    VideoDecoderPoll = 98,
}

impl MsgKind {
    pub fn from_u8(v: u8) -> Result<Self, ProtoError> {
        use MsgKind::*;
        Ok(match v {
            1 => Hello,
            2 => Welcome,
            3 => Error,
            4 => Ping,
            5 => Pong,
            10 => RunApk,
            11 => RunStarted,
            12 => RegisterProc,
            20 => ListProcs,
            21 => ProcList,
            30 => AllocShmem,
            31 => ShmemAllocd,
            40 => RegisterService,
            41 => ServiceRegistered,
            50 => BinderTransact,
            51 => BinderReply,
            60 => CreateSurface,
            61 => SurfaceCreated,
            62 => PresentFrame,
            63 => PresentDmabuf,
            64 => CreateSubsurface,
            65 => SubsurfaceCreated,
            66 => SetSubsurfacePosition,
            67 => DestroySurface,
            68 => PresentShm,
            69 => SetSurfaceOptions,
            70 => GetProp,
            71 => PropValue,
            72 => SetProp,
            73 => ListProps,
            74 => PropList,
            75 => ListServices,
            76 => ServiceList,
            77 => GuestCall,
            78 => GuestCallReply,
            79 => UnregisterProc,
            82 => QueryRun,
            83 => RunStatus,
            84 => GetAudioRing,
            85 => AudioRing,
            86 => AudioWrite,
            87 => SubscribeInput,
            88 => InputEvent,
            80 => Ack,
            90 => Shutdown,
            91 => Goodbye,
            92 => VideoDecoderOpen,
            93 => VideoDecoderReady,
            94 => VideoPacket,
            95 => VideoDecodeStatus,
            96 => VideoDecoderFlush,
            97 => VideoDecoderClose,
            98 => VideoDecoderPoll,
            other => return Err(ProtoError::BadKind(other)),
        })
    }
}

/// What kind of peer is on the other end of the socket. Used to decide which
/// messages are legal (e.g. only guests may present frames).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Role {
    Cli = 1,
    Shim = 2,
    Guest = 3,
}

impl Role {
    pub fn from_u8(v: u8) -> Result<Self, ProtoError> {
        Ok(match v {
            1 => Role::Cli,
            2 => Role::Shim,
            3 => Role::Guest,
            other => return Err(ProtoError::BadRole(other)),
        })
    }
}

/// A process tracked by the daemon's supervisor. These are the entries the
/// `pengudroid ps` subcommand prints, and they correspond 1:1 to real host
/// processes visible in `/proc`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcInfo {
    /// Host PID of the guest.
    pub pid: i32,
    /// Android package name (e.g. `com.example.app`).
    pub package: String,
    /// Main component being run (e.g. `com.example.app/.MainActivity`).
    pub component: String,
    /// Wayland surface id if the process owns a window, else 0.
    pub surface_id: u32,
}

/// A pointer or keyboard event forwarded from the compositor.
///
/// Coordinates are in surface pixels, with the origin at the top-left, which is
/// what `MotionEvent` expects.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputKind {
    PointerEnter { x: f32, y: f32 },
    PointerLeave,
    PointerMotion { x: f32, y: f32 },
    /// `button` is a Linux `BTN_*` code (0x110 = left, 0x111 = right).
    PointerButton { button: u32, pressed: bool },
    /// `axis` is 0 for vertical, 1 for horizontal.
    PointerAxis { axis: u8, value: f32 },
    /// `keycode` is a Linux input keycode (`KEY_*`).
    Key { keycode: u32, pressed: bool },
}

impl InputKind {
    fn tag(&self) -> u8 {
        match self {
            InputKind::PointerEnter { .. } => 1,
            InputKind::PointerLeave => 2,
            InputKind::PointerMotion { .. } => 3,
            InputKind::PointerButton { .. } => 4,
            InputKind::PointerAxis { .. } => 5,
            InputKind::Key { .. } => 6,
        }
    }
}

/// Every control-plane message.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Hello { version: u16, role: Role, pid: i32 },
    Welcome { version: u16, pid: i32 },
    Error { code: i32, message: String },
    Ping,
    Pong,

    RunApk { path: String, component: Option<String>, args: Vec<String> },
    RunStarted { pid: i32, package: String, surface_id: u32 },
    /// Ask the daemon to track a host process as a guest (for `pengudroid ps`).
    RegisterProc { pid: i32, package: String, component: String, surface_id: u32 },

    ListProcs,
    ProcList { procs: Vec<ProcInfo> },

    /// Ask the daemon to allocate an ashmem-compatible region. The resulting
    /// memfd is delivered via `SCM_RIGHTS` alongside the `ShmemAllocd` reply.
    AllocShmem { name: String, size: u64 },
    ShmemAllocd { name: String, size: u64, seam: bool },

    RegisterService { name: String, iface: String },
    ServiceRegistered { handle: u32 },

    /// A userspace-broker binder transaction (used when the kernel binder
    /// driver is unavailable).
    BinderTransact { handle: u32, code: u32, flags: u32, data: Vec<u8> },
    BinderReply { status: i32, data: Vec<u8> },

    /// Ask for a Wayland-toplevel-backed surface. The returned shm name can be
    /// mapped to write pixels translated from the guest's framebuffer.
    CreateSurface { width: u32, height: u32, title: String },
    SurfaceCreated { surface_id: u32, shm_name: String, stride: u32, size: u64, format: u32 },
    PresentFrame { surface_id: u32 },
    /// Ask for a child layer of the surface owned by `parent_pid`.
    ///
    /// This is how a `SurfaceView`'s video output reaches the screen: Android
    /// composites it as a separate layer under the window, and the closest
    /// Wayland equivalent is a `wl_subsurface` positioned inside the parent.
    CreateSubsurface { parent_pid: i32, x: i32, y: i32, width: u32, height: u32 },
    SubsurfaceCreated { surface_id: u32 },
    SetSubsurfacePosition { surface_id: u32, x: i32, y: i32 },
    DestroySurface { surface_id: u32 },
    SetSurfaceOptions { surface_id: u32, fps_meter: bool },
    /// Snapshot a CPU buffer (memfd passed out-of-band), never a GPU dma-buf.
    /// `format` is an Android HAL pixel format; `stride` is bytes per row.
    PresentShm { surface_id: u32, width: u32, height: u32, stride: u32, format: u32 },
    /// Present a dma-buf (fd passed out-of-band) on an existing surface.
    PresentDmabuf {
        surface_id: u32,
        width: u32,
        height: u32,
        stride: u32,
        fourcc: u32,
        modifier: u64,
        offset: u32,
    },

    /// One hardware decoder per connection, bound to an owned video surface.
    VideoDecoderOpen { surface_id: u32, codec: String, device: String, extra: Vec<u8> },
    VideoDecoderReady,
    /// A complete access unit. EOS requires empty data; use Poll to drain.
    VideoPacket { pts_us: i64, data: Vec<u8>, end_of_stream: bool },
    /// Counters are cumulative since Open/Flush. Presented means submitted,
    /// not proof of display; source reuse still waits for compositor release.
    VideoDecodeStatus { input_accepted: bool, decoded: u64, presented: u64,
        end_of_stream: bool, blocked: bool, last_pts_us: Option<i64> },
    VideoDecoderFlush,
    VideoDecoderClose,
    VideoDecoderPoll,

    GetProp { name: String },
    PropValue { name: String, value: Option<String> },
    SetProp { name: String, value: String },

    /// Every property the daemon holds (answers with one `PropValue` each, in
    /// `PropList` batches).
    ListProps,
    /// One batch of `ListProps` results; `more` is true when another batch follows.
    PropList { pairs: Vec<(String, String)>, more: bool },

    /// Every service the broker advertises: `(name, interface, handle)`.
    ListServices,
    ServiceList { services: Vec<(String, String, u32)> },

    /// Invoke a method on a guest-registered service. The owner receives this
    /// through its own connection; the daemon forwards the reply back to the
    /// caller.
    GuestCall { handle: u32, code: u32, data: Vec<u8>, token: u32 },
    /// A reply to the oldest outstanding `GuestCall` on this connection.
    GuestCallReply { status: i32, data: Vec<u8> },

    /// Tell the daemon this process's guest connection is gone.
    UnregisterProc { pid: i32 },

    /// Query the exit status of a guest launched through `RunApk`.
    QueryRun { pid: i32 },
    /// Ask for the daemon's shared audio ring; the memfd is attached.
    GetAudioRing,
    AudioRing { rate: u32, channels: u16, size: u32 },
    /// Ask the daemon to forward `ring[offset .. offset+len]` to the audio sink.
    AudioWrite { offset: u32, len: u32 },
    /// A guest launched through `RunApk` has exited (`running` false) or is
    /// still alive.
    RunStatus { pid: i32, running: bool, exit_code: i32, signal: i32 },

    /// Ask the daemon to forward input for a surface on this connection.
    /// The connection then only receives `InputEvent` messages.
    SubscribeInput { surface_id: u32 },
    /// Pointer/keyboard input for a surface the receiver subscribed to.
    InputEvent { surface_id: u32, event: InputKind },

    /// Generic acknowledgement for fire-and-forget requests.
    Ack,

    Shutdown,
    Goodbye,
}

impl Message {
    pub fn kind(&self) -> MsgKind {
        use Message::*;
        match self {
            Hello { .. } => MsgKind::Hello,
            Welcome { .. } => MsgKind::Welcome,
            Error { .. } => MsgKind::Error,
            Ping => MsgKind::Ping,
            Pong => MsgKind::Pong,
            RunApk { .. } => MsgKind::RunApk,
            RunStarted { .. } => MsgKind::RunStarted,
            RegisterProc { .. } => MsgKind::RegisterProc,
            ListProcs => MsgKind::ListProcs,
            ProcList { .. } => MsgKind::ProcList,
            AllocShmem { .. } => MsgKind::AllocShmem,
            ShmemAllocd { .. } => MsgKind::ShmemAllocd,
            RegisterService { .. } => MsgKind::RegisterService,
            ServiceRegistered { .. } => MsgKind::ServiceRegistered,
            BinderTransact { .. } => MsgKind::BinderTransact,
            BinderReply { .. } => MsgKind::BinderReply,
            CreateSurface { .. } => MsgKind::CreateSurface,
            SurfaceCreated { .. } => MsgKind::SurfaceCreated,
            PresentFrame { .. } => MsgKind::PresentFrame,
            CreateSubsurface { .. } => MsgKind::CreateSubsurface,
            SubsurfaceCreated { .. } => MsgKind::SubsurfaceCreated,
            SetSubsurfacePosition { .. } => MsgKind::SetSubsurfacePosition,
            DestroySurface { .. } => MsgKind::DestroySurface,
            SetSurfaceOptions { .. } => MsgKind::SetSurfaceOptions,
            PresentDmabuf { .. } => MsgKind::PresentDmabuf,
            PresentShm { .. } => MsgKind::PresentShm,
            VideoDecoderOpen { .. } => MsgKind::VideoDecoderOpen,
            VideoDecoderReady => MsgKind::VideoDecoderReady,
            VideoPacket { .. } => MsgKind::VideoPacket,
            VideoDecodeStatus { .. } => MsgKind::VideoDecodeStatus,
            VideoDecoderFlush => MsgKind::VideoDecoderFlush,
            VideoDecoderClose => MsgKind::VideoDecoderClose,
            VideoDecoderPoll => MsgKind::VideoDecoderPoll,
            GetProp { .. } => MsgKind::GetProp,
            PropValue { .. } => MsgKind::PropValue,
            SetProp { .. } => MsgKind::SetProp,
            ListProps => MsgKind::ListProps,
            PropList { .. } => MsgKind::PropList,
            ListServices => MsgKind::ListServices,
            ServiceList { .. } => MsgKind::ServiceList,
            GuestCall { .. } => MsgKind::GuestCall,
            GuestCallReply { .. } => MsgKind::GuestCallReply,
            UnregisterProc { .. } => MsgKind::UnregisterProc,
            QueryRun { .. } => MsgKind::QueryRun,
            RunStatus { .. } => MsgKind::RunStatus,
            GetAudioRing => MsgKind::GetAudioRing,
            AudioRing { .. } => MsgKind::AudioRing,
            AudioWrite { .. } => MsgKind::AudioWrite,
            SubscribeInput { .. } => MsgKind::SubscribeInput,
            InputEvent { .. } => MsgKind::InputEvent,
            Ack => MsgKind::Ack,
            Shutdown => MsgKind::Shutdown,
            Goodbye => MsgKind::Goodbye,
        }
    }

    /// Encode the full framed message (length prefix included).
    pub fn to_frame(&self) -> Vec<u8> {
        let mut e = Encoder::new(self.kind());
        use Message::*;
        match self {
            Hello { version, role, pid } => {
                e.u16(*version);
                e.u8(*role as u8);
                e.i32(*pid);
            }
            Welcome { version, pid } => {
                e.u16(*version);
                e.i32(*pid);
            }
            Error { code, message } => {
                e.i32(*code);
                e.str(message);
            }
            Ping | Pong | ListProcs | Shutdown | Goodbye | Ack => {}
            ListProps | ListServices => {}
            RunApk { path, component, args } => {
                e.str(path);
                match component {
                    Some(c) => {
                        e.u8(1);
                        e.str(c);
                    }
                    None => e.u8(0),
                }
                e.u32(args.len() as u32);
                for a in args {
                    e.str(a);
                }
            }
            RunStarted { pid, package, surface_id } => {
                e.i32(*pid);
                e.str(package);
                e.u32(*surface_id);
            }
            RegisterProc { pid, package, component, surface_id } => {
                e.i32(*pid);
                e.str(package);
                e.str(component);
                e.u32(*surface_id);
            }
            ProcList { procs } => {
                e.u32(procs.len() as u32);
                for p in procs {
                    e.i32(p.pid);
                    e.str(&p.package);
                    e.str(&p.component);
                    e.u32(p.surface_id);
                }
            }
            AllocShmem { name, size } => {
                e.str(name);
                e.u64(*size);
            }
            ShmemAllocd { name, size, seam } => {
                e.str(name);
                e.u64(*size);
                e.u8(if *seam { 1 } else { 0 });
            }
            RegisterService { name, iface } => {
                e.str(name);
                e.str(iface);
            }
            ServiceRegistered { handle } => e.u32(*handle),
            BinderTransact { handle, code, flags, data } => {
                e.u32(*handle);
                e.u32(*code);
                e.u32(*flags);
                e.bytes(data);
            }
            BinderReply { status, data } => {
                e.i32(*status);
                e.bytes(data);
            }
            CreateSurface { width, height, title } => {
                e.u32(*width);
                e.u32(*height);
                e.str(title);
            }
            SurfaceCreated { surface_id, shm_name, stride, size, format } => {
                e.u32(*surface_id);
                e.str(shm_name);
                e.u32(*stride);
                e.u64(*size);
                e.u32(*format);
            }
            PresentFrame { surface_id } => e.u32(*surface_id),
            CreateSubsurface { parent_pid, x, y, width, height } => {
                e.i32(*parent_pid);
                e.i32(*x);
                e.i32(*y);
                e.u32(*width);
                e.u32(*height);
            }
            SubsurfaceCreated { surface_id } => e.u32(*surface_id),
            SetSubsurfacePosition { surface_id, x, y } => {
                e.u32(*surface_id);
                e.i32(*x);
                e.i32(*y);
            }
            DestroySurface { surface_id } => e.u32(*surface_id),
            SetSurfaceOptions { surface_id, fps_meter } => { e.u32(*surface_id); e.u8(u8::from(*fps_meter)); }
            PresentShm { surface_id, width, height, stride, format } => {
                e.u32(*surface_id);
                e.u32(*width);
                e.u32(*height);
                e.u32(*stride);
                e.u32(*format);
            }
            PresentDmabuf { surface_id, width, height, stride, fourcc, modifier, offset } => {
                e.u32(*surface_id);
                e.u32(*width);
                e.u32(*height);
                e.u32(*stride);
                e.u32(*fourcc);
                e.u64(*modifier);
                e.u32(*offset);
            }
            VideoDecoderOpen { surface_id, codec, device, extra } => {
                e.u32(*surface_id); e.str(codec); e.str(device); e.bytes(extra);
            }
            VideoPacket { pts_us, data, end_of_stream } => {
                e.u64(*pts_us as u64); e.bytes(data); e.u8(u8::from(*end_of_stream));
            }
            VideoDecodeStatus { input_accepted, decoded, presented, end_of_stream, blocked, last_pts_us } => {
                e.u8(u8::from(*input_accepted)); e.u64(*decoded); e.u64(*presented);
                e.u8(u8::from(*end_of_stream)); e.u8(u8::from(*blocked));
                e.u8(u8::from(last_pts_us.is_some()));
                if let Some(pts) = last_pts_us { e.u64(*pts as u64); }
            }
            VideoDecoderReady | VideoDecoderFlush | VideoDecoderClose | VideoDecoderPoll => {}
            GetProp { name } => e.str(name),
            SetProp { name, value } => {
                e.str(name);
                e.str(value);
            }
            PropValue { name, value } => {
                e.str(name);
                match value {
                    Some(v) => {
                        e.u8(1);
                        e.str(v);
                    }
                    None => e.u8(0),
                }
            }
            UnregisterProc { pid } => e.i32(*pid),
            PropList { pairs, more } => {
                e.u32(pairs.len() as u32);
                for (k, v) in pairs {
                    e.str(k);
                    e.str(v);
                }
                e.u8(if *more { 1 } else { 0 });
            }
            ServiceList { services } => {
                e.u32(services.len() as u32);
                for (name, iface, handle) in services {
                    e.str(name);
                    e.str(iface);
                    e.u32(*handle);
                }
            }
            GuestCall { handle, code, data, token } => {
                e.u32(*handle);
                e.u32(*code);
                e.bytes(data);
                e.u32(*token);
            }
            GuestCallReply { status, data } => {
                e.i32(*status);
                e.bytes(data);
            }
            QueryRun { pid } => e.i32(*pid),
            RunStatus { pid, running, exit_code, signal } => {
                e.i32(*pid);
                e.u8(if *running { 1 } else { 0 });
                e.i32(*exit_code);
                e.i32(*signal);
            }
            GetAudioRing => {}
            AudioRing { rate, channels, size } => {
                e.u32(*rate);
                e.u16(*channels);
                e.u32(*size);
            }
            AudioWrite { offset, len } => {
                e.u32(*offset);
                e.u32(*len);
            }
            SubscribeInput { surface_id } => e.u32(*surface_id),
            InputEvent { surface_id, event } => {
                e.u32(*surface_id);
                e.u8(event.tag());
                match event {
                    InputKind::PointerEnter { x, y }
                    | InputKind::PointerMotion { x, y } => {
                        e.f32(*x);
                        e.f32(*y);
                    }
                    InputKind::PointerLeave => {}
                    InputKind::PointerButton { button, pressed } => {
                        e.u32(*button);
                        e.u8(if *pressed { 1 } else { 0 });
                    }
                    InputKind::PointerAxis { axis, value } => {
                        e.u8(*axis);
                        e.f32(*value);
                    }
                    InputKind::Key { keycode, pressed } => {
                        e.u32(*keycode);
                        e.u8(if *pressed { 1 } else { 0 });
                    }
                }
            }
        }
        e.into_frame()
    }

    /// Decode a message body (kind byte included, length prefix already
    /// stripped by the framing layer).
    pub fn from_body(body: &[u8]) -> Result<Message, ProtoError> {
        let mut d = Decoder::new(body);
        let kind = MsgKind::from_u8(d.u8()?)?;
        use MsgKind as K;
        use Message::*;
        Ok(match kind {
            K::Hello => Hello {
                version: d.u16()?,
                role: Role::from_u8(d.u8()?)?,
                pid: d.i32()?,
            },
            K::Welcome => Welcome { version: d.u16()?, pid: d.i32()? },
            K::Error => Error { code: d.i32()?, message: d.str()? },
            K::Ping => Ping,
            K::Pong => Pong,
            K::RunApk => {
                let path = d.str()?;
                let component = if d.u8()? == 1 { Some(d.str()?) } else { None };
                let n = d.u32()? as usize;
                let mut args = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    args.push(d.str()?);
                }
                RunApk { path, component, args }
            }
            K::RunStarted => RunStarted { pid: d.i32()?, package: d.str()?, surface_id: d.u32()? },
            K::RegisterProc => RegisterProc {
                pid: d.i32()?,
                package: d.str()?,
                component: d.str()?,
                surface_id: d.u32()?,
            },
            K::ListProcs => ListProcs,
            K::ProcList => {
                let n = d.u32()? as usize;
                let mut procs = Vec::with_capacity(n.min(4096));
                for _ in 0..n {
                    procs.push(ProcInfo {
                        pid: d.i32()?,
                        package: d.str()?,
                        component: d.str()?,
                        surface_id: d.u32()?,
                    });
                }
                ProcList { procs }
            }
            K::AllocShmem => AllocShmem { name: d.str()?, size: d.u64()? },
            K::ShmemAllocd => ShmemAllocd { name: d.str()?, size: d.u64()?, seam: d.u8()? != 0 },
            K::RegisterService => RegisterService { name: d.str()?, iface: d.str()? },
            K::ServiceRegistered => ServiceRegistered { handle: d.u32()? },
            K::BinderTransact => BinderTransact {
                handle: d.u32()?,
                code: d.u32()?,
                flags: d.u32()?,
                data: d.bytes()?,
            },
            K::BinderReply => BinderReply { status: d.i32()?, data: d.bytes()? },
            K::CreateSurface => CreateSurface { width: d.u32()?, height: d.u32()?, title: d.str()? },
            K::SurfaceCreated => SurfaceCreated {
                surface_id: d.u32()?,
                shm_name: d.str()?,
                stride: d.u32()?,
                size: d.u64()?,
                format: d.u32()?,
            },
            K::PresentFrame => PresentFrame { surface_id: d.u32()? },
            K::CreateSubsurface => CreateSubsurface {
                parent_pid: d.i32()?,
                x: d.i32()?,
                y: d.i32()?,
                width: d.u32()?,
                height: d.u32()?,
            },
            K::SubsurfaceCreated => SubsurfaceCreated { surface_id: d.u32()? },
            K::SetSubsurfacePosition => SetSubsurfacePosition {
                surface_id: d.u32()?,
                x: d.i32()?,
                y: d.i32()?,
            },
            K::DestroySurface => DestroySurface { surface_id: d.u32()? },
            K::SetSurfaceOptions => SetSurfaceOptions { surface_id: d.u32()?, fps_meter: d.u8()? != 0 },
            K::PresentShm => PresentShm {
                surface_id: d.u32()?, width: d.u32()?, height: d.u32()?,
                stride: d.u32()?, format: d.u32()?,
            },
            K::PresentDmabuf => PresentDmabuf {
                surface_id: d.u32()?,
                width: d.u32()?,
                height: d.u32()?,
                stride: d.u32()?,
                fourcc: d.u32()?,
                modifier: d.u64()?,
                offset: d.u32()?,
            },
            K::VideoDecoderOpen => VideoDecoderOpen { surface_id: d.u32()?, codec: d.str()?, device: d.str()?, extra: d.bytes()?.to_vec() },
            K::VideoDecoderReady => VideoDecoderReady,
            K::VideoPacket => VideoPacket { pts_us: d.u64()? as i64, data: d.bytes()?.to_vec(), end_of_stream: d.u8()? != 0 },
            K::VideoDecodeStatus => VideoDecodeStatus {
                input_accepted: d.u8()? != 0, decoded: d.u64()?, presented: d.u64()?,
                end_of_stream: d.u8()? != 0, blocked: d.u8()? != 0,
                last_pts_us: if d.u8()? != 0 { Some(d.u64()? as i64) } else { None },
            },
            K::VideoDecoderFlush => VideoDecoderFlush,
            K::VideoDecoderClose => VideoDecoderClose,
            K::VideoDecoderPoll => VideoDecoderPoll,
            K::GetProp => GetProp { name: d.str()? },
            K::SetProp => SetProp { name: d.str()?, value: d.str()? },
            K::PropValue => {
                let name = d.str()?;
                let value = if d.u8()? == 1 { Some(d.str()?) } else { None };
                PropValue { name, value }
            }
            K::Shutdown => Shutdown,
            K::Goodbye => Goodbye,
            K::Ack => Ack,
            K::ListProps => ListProps,
            K::ListServices => ListServices,
            K::UnregisterProc => UnregisterProc { pid: d.i32()? },
            K::PropList => {
                let n = d.u32()? as usize;
                let mut pairs = Vec::with_capacity(n.min(4096));
                for _ in 0..n {
                    pairs.push((d.str()?, d.str()?));
                }
                PropList { pairs, more: d.u8()? != 0 }
            }
            K::ServiceList => {
                let n = d.u32()? as usize;
                let mut services = Vec::with_capacity(n.min(4096));
                for _ in 0..n {
                    services.push((d.str()?, d.str()?, d.u32()?));
                }
                ServiceList { services }
            }
            K::GuestCall => GuestCall {
                handle: d.u32()?,
                code: d.u32()?,
                data: d.bytes()?,
                token: d.u32()?,
            },
            K::GuestCallReply => GuestCallReply { status: d.i32()?, data: d.bytes()? },
            K::QueryRun => QueryRun { pid: d.i32()? },
            K::RunStatus => RunStatus {
                pid: d.i32()?,
                running: d.u8()? != 0,
                exit_code: d.i32()?,
                signal: d.i32()?,
            },
            K::GetAudioRing => GetAudioRing,
            K::AudioRing => AudioRing {
                rate: d.u32()?,
                channels: d.u16()?,
                size: d.u32()?,
            },
            K::AudioWrite => AudioWrite { offset: d.u32()?, len: d.u32()? },
            K::SubscribeInput => SubscribeInput { surface_id: d.u32()? },
            K::InputEvent => {
                let surface_id = d.u32()?;
                let event = match d.u8()? {
                    1 => InputKind::PointerEnter { x: d.f32()?, y: d.f32()? },
                    2 => InputKind::PointerLeave,
                    3 => InputKind::PointerMotion { x: d.f32()?, y: d.f32()? },
                    4 => InputKind::PointerButton { button: d.u32()?, pressed: d.u8()? != 0 },
                    5 => InputKind::PointerAxis { axis: d.u8()?, value: d.f32()? },
                    6 => InputKind::Key { keycode: d.u32()?, pressed: d.u8()? != 0 },
                    other => return Err(ProtoError::BadKind(other)),
                };
                InputEvent { surface_id, event }
            }
        })
    }
}

/// Little-endian message encoder. Body only; call [`Encoder::into_frame`] to
/// add the length prefix.
#[derive(Default)]
pub struct Encoder {
    buf: Vec<u8>,
}

impl Encoder {
    pub fn new(kind: MsgKind) -> Self {
        let mut e = Encoder { buf: Vec::with_capacity(64) };
        e.buf.push(kind as u8);
        e
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn f32(&mut self, v: f32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.buf.extend_from_slice(b);
    }
    pub fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }

    pub fn body(&self) -> &[u8] {
        &self.buf
    }

    /// Produce `[u32 len][body]`.
    pub fn into_frame(self) -> Vec<u8> {
        let body = self.buf;
        let mut frame = Vec::with_capacity(body.len() + 4);
        frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
        frame.extend_from_slice(&body);
        frame
    }
}

/// Bounds-checked little-endian decoder.
pub struct Decoder<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Decoder<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Decoder { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], ProtoError> {
        let end = self.pos.checked_add(n).ok_or(ProtoError::Truncated)?;
        if end > self.buf.len() {
            return Err(ProtoError::Truncated);
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    pub fn u8(&mut self) -> Result<u8, ProtoError> {
        Ok(self.take(1)?[0])
    }
    pub fn u16(&mut self) -> Result<u16, ProtoError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    pub fn u32(&mut self) -> Result<u32, ProtoError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn u64(&mut self) -> Result<u64, ProtoError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn i32(&mut self) -> Result<i32, ProtoError> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn f32(&mut self) -> Result<f32, ProtoError> {
        Ok(f32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn bytes(&mut self) -> Result<Vec<u8>, ProtoError> {
        let n = self.u32()? as usize;
        Ok(self.take(n)?.to_vec())
    }
    pub fn str(&mut self) -> Result<String, ProtoError> {
        let b = self.bytes()?;
        String::from_utf8(b).map_err(|_| ProtoError::BadUtf8)
    }
}

/// Well-known ABI constants that PenguDroid must speak.
pub mod abi {
    use crate::Ioctl;

    /// `BINDER_WRITE_READ` = `_IOWR('b', 1, struct binder_write_read)`.
    /// struct size is 48 bytes on 64-bit.
    pub const BINDER_WRITE_READ: Ioctl = 0xC030_6201;
    /// `BINDER_VERSION` = `_IOWR('b', 9, struct binder_version)` (4-byte struct).
    pub const BINDER_VERSION: Ioctl = 0xC004_6209;
    /// `BINDER_SET_CONTEXT_MGR` = `_IOW('b', 7, int)`.
    pub const BINDER_SET_CONTEXT_MGR: Ioctl = 0x4004_6207;

    /// Primary kernel binder device nodes, in probe order.
    pub const BINDER_DEVICES: &[&str] =
        &["/dev/binderfs/binder", "/dev/binder", "/dev/binderfs/binder0"];

    /// ashmem ioctls (kept for ABI completeness; the shim maps these to
    /// `memfd_create` + seals instead of the legacy ashmem driver).
    pub const ASHMEM_SET_NAME: Ioctl = 0x4010_6101;
    pub const ASHMEM_SET_SIZE: Ioctl = 0x4008_6103;
    pub const ASHMEM_GET_SIZE: Ioctl = 0x8008_6104;
    pub const ASHMEM_PIN: Ioctl = 0x4008_6107;
    pub const ASHMEM_UNPIN: Ioctl = 0x4008_6108;

    /// `PROT_EXEC`-safe sealing we apply to emulate ashmem's pin semantics.
    pub const F_SEAL_SHRINK_MASK: u32 = 0x0002;
    pub const F_SEAL_GROW_MASK: u32 = 0x0004;
    pub const F_SEAL_WRITE_MASK: u32 = 0x0008;
    pub const F_SEAL_SEAL_MASK: u32 = 0x0010;

    /// DRM fourcc for `ARGB8888` = `'A','R','2','4'`-style little-endian.
    pub const WL_DRM_FORMAT_ARGB8888: u32 = 0x3432_5241;
    /// The Android `HAL_PIXEL_FORMAT_RGBA_8888` value.
    pub const ANDROID_RGBA_8888: u32 = 1;

    /// Android 16 (API 36) — what we advertise to guests.
    pub const ANDROID_API_LEVEL: i32 = 36;
    pub const ANDROID_RELEASE: &str = "16";
}

/// `ioctl` request numbers are `unsigned long` on Linux.
pub type Ioctl = u64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtoError {
    Truncated,
    BadKind(u8),
    BadRole(u8),
    BadUtf8,
    FrameTooLarge(u32),
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtoError::Truncated => write!(f, "message truncated"),
            ProtoError::BadKind(k) => write!(f, "unknown message kind {k}"),
            ProtoError::BadRole(r) => write!(f, "unknown role {r}"),
            ProtoError::BadUtf8 => write!(f, "string field was not valid UTF-8"),
            ProtoError::FrameTooLarge(n) => write!(f, "frame of {n} bytes exceeds limit"),
        }
    }
}

impl std::error::Error for ProtoError {}

/// Where the daemon's control socket lives. An explicit PENGUDROID_SOCKET
/// isolates a broker without replacing XDG_RUNTIME_DIR: PipeWire and Wayland
/// must still see the real desktop session. Without it retain the runtime-dir
/// and per-uid /tmp fallback used by existing installations.
pub fn default_socket_path() -> std::path::PathBuf {
    socket_path(std::env::var_os("PENGUDROID_SOCKET").as_deref(),
        std::env::var_os("XDG_RUNTIME_DIR").as_deref(), uid())
}

fn socket_path(explicit: Option<&std::ffi::OsStr>, runtime: Option<&std::ffi::OsStr>, uid: u32) -> std::path::PathBuf {
    if let Some(path) = explicit.filter(|s| !s.is_empty()) {
        return std::path::PathBuf::from(path);
    }
    if let Some(dir) = runtime.filter(|s| !s.is_empty()) {
        return std::path::PathBuf::from(dir).join("pengudroid/daemon.sock");
    }
    std::path::PathBuf::from(format!("/tmp/pengudroid-{uid}/daemon.sock"))
}

/// Current effective uid, read via `std` metadata (no `unsafe`, no `libc`
/// dependency in this crate).
fn uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self")
        .map(|m| m.uid())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(m: Message) {
        let frame = m.to_frame();
        let len = u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize;
        assert_eq!(len, frame.len() - 4, "length prefix must match body");
        let decoded = Message::from_body(&frame[4..]).unwrap();
        assert_eq!(m, decoded);
    }

    #[test]
    fn hardware_video_commands_preserve_timestamps_configuration_and_status() {
        roundtrip(Message::VideoDecoderOpen { surface_id: 42, codec: "h264".into(),
            device: "/dev/dri/renderD128".into(), extra: vec![0, 1, 0, 255] });
        roundtrip(Message::VideoDecoderReady);
        for pts_us in [i64::MIN, -123456, 0, i64::MAX] {
            roundtrip(Message::VideoPacket { pts_us, data: vec![0, 0, 1, 0x65], end_of_stream: false });
            roundtrip(Message::VideoDecodeStatus { input_accepted: true, decoded: 9, presented: 7,
                end_of_stream: false, blocked: true, last_pts_us: Some(pts_us) });
        }
        roundtrip(Message::VideoPacket { pts_us: 0, data: vec![], end_of_stream: true });
        roundtrip(Message::VideoDecodeStatus { input_accepted: false, decoded: 0, presented: 0,
            end_of_stream: true, blocked: false, last_pts_us: None });
        roundtrip(Message::VideoDecoderFlush);
        roundtrip(Message::VideoDecoderClose);
        roundtrip(Message::VideoDecoderPoll);
    }

    #[test]
    fn explicit_control_socket_preserves_the_host_runtime_contract() {
        use std::ffi::OsStr;
        assert_eq!(socket_path(Some(OsStr::new("/tmp/private/daemon.sock")), Some(OsStr::new("/run/user/1000")), 1000),
            std::path::PathBuf::from("/tmp/private/daemon.sock"));
        assert_eq!(socket_path(Some(OsStr::new("")), Some(OsStr::new("/run/user/1000")), 1000),
            std::path::PathBuf::from("/run/user/1000/pengudroid/daemon.sock"));
        assert_eq!(socket_path(None, None, 1000), std::path::PathBuf::from("/tmp/pengudroid-1000/daemon.sock"));
    }

    #[test]
    fn roundtrip_simple() {
        roundtrip(Message::Ping);
        roundtrip(Message::Shutdown);
        roundtrip(Message::Hello { version: PROTO_VERSION, role: Role::Guest, pid: 4242 });
    }

    #[test]
    fn roundtrip_complex() {
        roundtrip(Message::PresentShm { surface_id: 4, width: 426, height: 240, stride: 852, format: 4 });
        roundtrip(Message::SetSurfaceOptions { surface_id: 4, fps_meter: true });
        roundtrip(Message::SetSurfaceOptions { surface_id: 4, fps_meter: false });
        roundtrip(Message::RunApk {
            path: "/tmp/app.apk".into(),
            component: Some("com.x/.Main".into()),
            args: vec!["a".into(), "b".into()],
        });
        roundtrip(Message::ProcList {
            procs: vec![ProcInfo {
                pid: 7,
                package: "com.x".into(),
                component: "com.x/.Main".into(),
                surface_id: 3,
            }],
        });
        roundtrip(Message::BinderTransact {
            handle: 1,
            code: 2,
            flags: 3,
            data: vec![1, 2, 3, 4],
        });
    }

    #[test]
    fn roundtrip_status_messages() {
        roundtrip(Message::ListProps);
        roundtrip(Message::ListServices);
        roundtrip(Message::PropList {
            pairs: vec![("ro.build.version.sdk".into(), "36".into())],
            more: true,
        });
        roundtrip(Message::ServiceList {
            services: vec![("activity".into(), "android.app.IActivityManager".into(), 1)],
        });
        roundtrip(Message::UnregisterProc { pid: 99 });
        roundtrip(Message::QueryRun { pid: 12 });
        roundtrip(Message::RunStatus { pid: 12, running: false, exit_code: 0, signal: 9 });
        roundtrip(Message::GuestCall {
            handle: 3,
            code: 7,
            data: vec![9, 8],
            token: 11,
        });
        roundtrip(Message::GuestCallReply { status: -38, data: vec![] });
    }

    #[test]
    fn decode_rejects_truncation() {
        let frame = Message::RunApk { path: "x".into(), component: None, args: vec![] }.to_frame();
        assert!(Message::from_body(&frame[4..frame.len() - 2]).is_err());
    }

    #[test]
    fn bad_kind_is_rejected() {
        assert_eq!(Message::from_body(&[0xFF]), Err(ProtoError::BadKind(0xFF)));
    }
}
