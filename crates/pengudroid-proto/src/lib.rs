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

    Ack = 80,

    Shutdown = 90,
    Goodbye = 91,
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
            80 => Ack,
            90 => Shutdown,
            91 => Goodbye,
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

/// Every control-plane message.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// A guest launched through `RunApk` has exited (`running` false) or is
    /// still alive.
    RunStatus { pid: i32, running: bool, exit_code: i32, signal: i32 },

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

/// Where the daemon's control socket lives. Prefers the per-user runtime dir,
/// falling back to a per-uid directory under `/tmp` when `XDG_RUNTIME_DIR` is
/// unset (e.g. under `su`, cron, or a bare TTY).
pub fn default_socket_path() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        if !dir.is_empty() {
            return std::path::PathBuf::from(dir).join("pengudroid").join("daemon.sock");
        }
    }
    std::path::PathBuf::from(format!("/tmp/pengudroid-{}", uid())).join("daemon.sock")
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
    fn roundtrip_simple() {
        roundtrip(Message::Ping);
        roundtrip(Message::Shutdown);
        roundtrip(Message::Hello { version: PROTO_VERSION, role: Role::Guest, pid: 4242 });
    }

    #[test]
    fn roundtrip_complex() {
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
