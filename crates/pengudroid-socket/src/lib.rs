//! Unix-socket transport for PenguDroid, with out-of-band file-descriptor
//! passing via `SCM_RIGHTS`.
//!
//! This is the single place that knows how to move an [`AshmemRegion`]-style
//! memfd from the daemon to a guest (or back) without copying pixels through
//! the control socket. Both [`pengudroid-daemon`] and [`pengudroid-cli`] use
//! this crate, so the framing is guaranteed identical on both ends.
//!
//! [`AshmemRegion`]: https://android.googlesource.com/platform/system/core/+/refs/heads/main/libcutils/ashmem-dev.cpp
//!
//! # Frame layout
//!
//! `[u32 body_len little-endian][body]`, where `body` is exactly what
//! [`pengudroid_proto::Message::to_frame`] produces minus its own length
//! prefix — i.e. `[kind u8][fields...]`.
//!
//! File descriptors sent with a frame arrive out-of-band and are returned
//! alongside the decoded body, in the order they were attached.

use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::ptr;
use std::time::Duration;

use pengudroid_proto::MAX_FRAME_LEN;

/// Maximum number of fds we will accept in a single ancillary message.
const MAX_FDS: usize = 16;
/// Control-message scratch buffer size. Comfortably fits [`MAX_FDS`].
const CMSG_BUF: usize = 512;

#[derive(Debug)]
pub enum SocketError {
    Io(io::Error),
    /// A frame carried a length that is zero or larger than [`MAX_FRAME_LEN`].
    BadLength(u32),
    /// More than [`MAX_FDS`] descriptors were attached to one message.
    TooManyFds(usize),
}

impl From<io::Error> for SocketError {
    fn from(e: io::Error) -> Self {
        SocketError::Io(e)
    }
}

impl std::fmt::Display for SocketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SocketError::Io(e) => write!(f, "socket i/o error: {e}"),
            SocketError::BadLength(n) => write!(f, "invalid frame length {n}"),
            SocketError::TooManyFds(n) => write!(f, "message carried {n} fds (max {MAX_FDS})"),
        }
    }
}

impl std::error::Error for SocketError {}

pub type Result<T> = std::result::Result<T, SocketError>;

/// A connected control-socket endpoint.
///
/// Wraps an [`OwnedFd`] so the fd is closed on drop. Reading and writing is
/// **not** internally synchronized: a [`Connection`] is meant to be owned by a
/// single thread (the daemon spawns one per client).
pub struct Connection {
    fd: OwnedFd,
}

impl Connection {
    /// Wrap an already-connected fd. Takes ownership.
    ///
    /// # Safety
    /// `fd` must be a connected stream socket and must not be closed elsewhere.
    pub unsafe fn from_raw_fd(fd: RawFd) -> Connection {
        Connection { fd: OwnedFd::from_raw_fd(fd) }
    }

    /// Wrap an already-connected [`UnixStream`].
    pub fn from_stream(s: UnixStream) -> Connection {
        Connection { fd: OwnedFd::from(s) }
    }

    /// Connect to a listening control socket.
    pub fn connect(path: &std::path::Path) -> io::Result<Connection> {
        Ok(Connection::from_stream(UnixStream::connect(path)?))
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Bound blocking reads (and writes, on most platforms) to `timeout`.
    ///
    /// A timed-out read fails with [`io::ErrorKind::WouldBlock`], which callers
    /// use to multiplex between the socket and their own work.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        set_socket_timeouts(self.fd.as_raw_fd(), timeout)
    }

    /// Set the socket non-blocking.
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        let flags = unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        let flags = if nonblocking { flags | libc::O_NONBLOCK } else { flags & !libc::O_NONBLOCK };
        if unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_SETFL, flags) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Read one framed message. Returns the body and any attached fds.
    pub fn recv_frame(&self) -> Result<(Vec<u8>, Vec<RawFd>)> {
        let mut fds = Vec::new();
        let mut len_bytes = [0u8; 4];
        recv_exact(self.fd.as_raw_fd(), &mut len_bytes, &mut fds)?;
        let len = u32::from_le_bytes(len_bytes);
        if len == 0 || len > MAX_FRAME_LEN {
            return Err(SocketError::BadLength(len));
        }
        let mut body = vec![0u8; len as usize];
        recv_exact(self.fd.as_raw_fd(), &mut body, &mut fds)?;
        Ok((body, fds))
    }

    /// Write one framed message with optional attached fds.
    pub fn send_frame(&self, body: &[u8], fds: &[RawFd]) -> Result<()> {
        let mut frame = Vec::with_capacity(body.len() + 4);
        frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
        frame.extend_from_slice(body);
        send_all_with_fds(self.fd.as_raw_fd(), &frame, fds)
    }

    /// Convenience: encode and send a protocol message.
    pub fn send_message(&self, msg: &pengudroid_proto::Message, fds: &[RawFd]) -> Result<()> {
        let frame = msg.to_frame();
        // `to_frame` includes the length prefix already.
        send_all_with_fds(self.fd.as_raw_fd(), &frame, fds)
    }

    /// Read one framed message and decode it.
    pub fn recv_message(&self) -> Result<(pengudroid_proto::Message, Vec<RawFd>)> {
        let (body, fds) = self.recv_frame()?;
        let msg = pengudroid_proto::Message::from_body(&body)
            .map_err(|e| SocketError::Io(io::Error::new(io::ErrorKind::InvalidData, e)))?;
        Ok((msg, fds))
    }
}

/// Apply `SO_RCVTIMEO`/`SO_SNDTIMEO` to a socket (a no-op timeout means
/// "block indefinitely").
fn set_socket_timeouts(fd: RawFd, timeout: Option<Duration>) -> io::Result<()> {
    let tv = timeout.map(|d| libc::timeval {
        tv_sec: d.as_secs() as libc::time_t,
        tv_usec: d.subsec_micros() as libc::suseconds_t,
    });
    let ptr = match &tv {
        Some(t) => t as *const libc::timeval,
        None => std::ptr::null(),
    };
    let len = mem::size_of::<libc::timeval>() as libc::socklen_t;
    // Timeouts are plain socket state; setting them cannot invalidate the fd.
    for opt in [libc::SO_RCVTIMEO, libc::SO_SNDTIMEO] {
        // SAFETY: `fd` is a live socket and `ptr`/`len` describe a `timeval`.
        let rc = unsafe {
            libc::setsockopt(fd, libc::SOL_SOCKET, opt, ptr as *const libc::c_void, len)
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// A listening control socket.
pub struct Listener {
    inner: UnixListener,
    path: std::path::PathBuf,
}

impl Listener {
    /// Bind a listener at `path`, creating parent directories and replacing a
    /// stale socket file. The socket is chmod'd to `0600`.
    pub fn bind(path: &std::path::Path) -> io::Result<Listener> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Remove a stale socket. If a live daemon owns it, `connect` will
        // succeed and the caller should detect that before calling us.
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let inner = UnixListener::bind(path)?;
        // Restrict to the owner: the control channel passes fds that can write
        // into guest memory.
        let mut perms = std::fs::metadata(path)?.permissions();
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms)?;
        Ok(Listener { inner, path: path.to_path_buf() })
    }

    pub fn accept(&self) -> io::Result<Connection> {
        let (stream, _addr) = self.inner.accept()?;
        Ok(Connection::from_stream(stream))
    }

    pub fn set_nonblocking(&self, nb: bool) -> io::Result<()> {
        self.inner.set_nonblocking(nb)
    }

    /// Make connections returned by [`Listener::accept`] use `timeout` for
    /// reads and writes from now on.
    ///
    /// Accepted sockets inherit this from the listener on Linux, so a daemon
    /// can keep a non-blocking listener and still give each client a bounded
    /// blocking channel.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        use std::os::fd::AsFd;
        set_socket_timeouts(self.inner.as_fd().as_raw_fd(), timeout)
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Read exactly `buf.len()` bytes, collecting any SCM_RIGHTS descriptors.
fn recv_exact(fd: RawFd, buf: &mut [u8], fds: &mut Vec<RawFd>) -> Result<()> {
    let mut off = 0;
    while off < buf.len() {
        let n = recv_once(fd, &mut buf[off..], fds)?;
        if n == 0 {
            return Err(SocketError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "peer closed the control socket",
            )));
        }
        off += n;
    }
    Ok(())
}

/// One `recvmsg` call, appending any received fds to `fds`.
fn recv_once(fd: RawFd, buf: &mut [u8], fds: &mut Vec<RawFd>) -> Result<usize> {
    loop {
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr() as *mut libc::c_void,
            iov_len: buf.len(),
        };
        let mut cmsg_buf = [0u8; CMSG_BUF];
        // SAFETY: all pointers below refer to live locals for the duration of
        // the `recvmsg` call; `cmsg_buf` is large enough for MAX_FDS fds.
        let n = unsafe {
            let mut msg: libc::msghdr = mem::zeroed();
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
            msg.msg_controllen = cmsg_buf.len();

            let n = libc::recvmsg(fd, &mut msg, 0);
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(SocketError::Io(e));
            }

            // Walk the control-message chain for SCM_RIGHTS payloads.
            let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
            while !cmsg.is_null() {
                if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS
                {
                    let header = libc::CMSG_LEN(0) as usize;
                    let total = (*cmsg).cmsg_len as usize;
                    let payload = total.saturating_sub(header);
                    let count = payload / mem::size_of::<RawFd>();
                    let mut got = Vec::with_capacity(count);
                    let data = libc::CMSG_DATA(cmsg);
                    for i in 0..count {
                        let mut one: RawFd = -1;
                        ptr::copy_nonoverlapping(
                            data.add(i * mem::size_of::<RawFd>()),
                            &mut one as *mut RawFd as *mut u8,
                            mem::size_of::<RawFd>(),
                        );
                        got.push(one);
                    }
                    if fds.len() + got.len() > MAX_FDS {
                        // Close what we cannot track so we don't leak fds.
                        for f in &got {
                            libc::close(*f);
                        }
                        return Err(SocketError::TooManyFds(fds.len() + got.len()));
                    }
                    fds.extend(got);
                }
                cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
            }
            n as usize
        };
        return Ok(n);
    }
}

/// Send all of `buf` on `fd`, attaching `fds` to the first write.
fn send_all_with_fds(fd: RawFd, buf: &[u8], fds: &[RawFd]) -> Result<()> {
    let mut sent = 0usize;
    let mut control_pending = !fds.is_empty();
    while sent < buf.len() {
        let mut iov = libc::iovec {
            iov_base: buf[sent..].as_ptr() as *mut libc::c_void,
            iov_len: buf.len() - sent,
        };
        let mut cmsg_buf = [0u8; CMSG_BUF];
        // SAFETY: pointers refer to live locals; CMSG layout is checked below.
        let n = unsafe {
            let mut msg: libc::msghdr = mem::zeroed();
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_flags = 0;

            if control_pending {
                msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
                msg.msg_controllen = libc::CMSG_SPACE((fds.len() * mem::size_of::<RawFd>()) as u32)
                    as usize;
                let cmsg = libc::CMSG_FIRSTHDR(&msg);
                if cmsg.is_null() {
                    return Err(SocketError::Io(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "ancillary buffer too small for fds",
                    )));
                }
                (*cmsg).cmsg_level = libc::SOL_SOCKET;
                (*cmsg).cmsg_type = libc::SCM_RIGHTS;
                (*cmsg).cmsg_len =
                    libc::CMSG_LEN((fds.len() * mem::size_of::<RawFd>()) as u32) as usize;
                ptr::copy_nonoverlapping(
                    fds.as_ptr() as *const u8,
                    libc::CMSG_DATA(cmsg),
                    fds.len() * mem::size_of::<RawFd>(),
                );
            }

            let n = libc::sendmsg(fd, &msg, libc::MSG_NOSIGNAL);
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue; // retry the same range; fds still pending
                }
                return Err(SocketError::Io(e));
            }
            n as usize
        };
        if n == 0 {
            continue; // interrupted; retry same range, fds still pending
        }
        sent += n;
        // The fds were attached to the first successful send.
        control_pending = false;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn socketpair() -> (Connection, Connection) {
        let mut fds = [0 as RawFd; 2];
        let rc = unsafe {
            libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0, fds.as_mut_ptr())
        };
        assert_eq!(rc, 0, "socketpair failed");
        // SAFETY: socketpair produced two fresh, connected fds.
        unsafe { (Connection::from_raw_fd(fds[0]), Connection::from_raw_fd(fds[1])) }
    }

    #[test]
    fn frame_roundtrip_without_fds() {
        let (a, b) = socketpair();
        let msg = pengudroid_proto::Message::Ping;
        a.send_message(&msg, &[]).unwrap();
        let (got, fds) = b.recv_message().unwrap();
        assert_eq!(got, msg);
        assert!(fds.is_empty());
    }

    #[test]
    fn frame_roundtrip_with_fd() {
        let (a, b) = socketpair();
        // Create a memfd to pass across.
        let name = std::ffi::CString::new("test-region").unwrap();
        let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0, "memfd_create failed");
        let rc = unsafe { libc::ftruncate(fd, 4096) };
        assert_eq!(rc, 0);

        let msg = pengudroid_proto::Message::ShmemAllocd {
            name: "region".into(),
            size: 4096,
            seam: false,
        };
        a.send_message(&msg, &[fd]).unwrap();
        // The receiver gets its own duplicate; close our original.
        unsafe { libc::close(fd) };

        let (got, fds) = b.recv_message().unwrap();
        assert_eq!(got, msg);
        assert_eq!(fds.len(), 1);

        // Verify the received fd refers to a 4096-byte object.
        let recv = fds[0];
        let size = unsafe { libc::lseek(recv, 0, libc::SEEK_END) };
        assert_eq!(size, 4096);
        // And it is writable.
        let rc = unsafe { libc::ftruncate(recv, 8192) };
        assert_eq!(rc, 0);
        unsafe { libc::close(recv) };
    }

    #[test]
    fn bad_length_is_rejected() {
        let (a, b) = socketpair();
        // Hand-craft a frame with a zero length prefix straight onto the wire.
        let zeros = [0u8; 4];
        let n = unsafe { libc::write(a.as_raw_fd(), zeros.as_ptr() as *const libc::c_void, 4) };
        assert_eq!(n, 4);
        let err = b.recv_frame().unwrap_err();
        assert!(matches!(err, SocketError::BadLength(0)), "got {err:?}");
    }

    #[test]
    fn read_timeout_reports_would_block() {
        let (a, b) = socketpair();
        // The far end must time out; `a` keeps blocking writes.
        b.set_read_timeout(Some(Duration::from_millis(30))).unwrap();
        let start = std::time::Instant::now();
        let err = b.recv_message().unwrap_err();
        match err {
            SocketError::Io(e) => {
                assert_eq!(e.kind(), std::io::ErrorKind::WouldBlock, "got {e:?}")
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(start.elapsed() < Duration::from_secs(2));

        // A frame that arrives in time is still delivered normally.
        a.send_message(&pengudroid_proto::Message::Ping, &[]).unwrap();
        assert_eq!(b.recv_message().unwrap().0, pengudroid_proto::Message::Ping);
    }

    #[test]
    fn multiple_frames_are_ordered() {
        let (a, b) = socketpair();
        for i in 0..5u32 {
            a.send_message(&pengudroid_proto::Message::PresentFrame { surface_id: i }, &[])
                .unwrap();
        }
        for i in 0..5u32 {
            let (m, _) = b.recv_message().unwrap();
            assert_eq!(m, pengudroid_proto::Message::PresentFrame { surface_id: i });
        }
    }
}
