//! Control-socket client used by the CLI and the guest runtime.

use std::os::fd::{FromRawFd, OwnedFd};

use pengudroid_proto::{default_socket_path, Message, Role, PROTO_VERSION};
use pengudroid_socket::Connection;

pub struct Client {
    conn: Connection,
}

impl Client {
    /// Connect to the daemon at the default socket path.
    pub fn connect() -> Result<Client, String> {
        Client::connect_to(&default_socket_path())
    }

    pub fn connect_to(path: &std::path::Path) -> Result<Client, String> {
        // `connect_endpoint` also understands the `@name` form a daemon falls
        // back to when the sandbox refuses filesystem `bind(2)`.
        let conn = Connection::connect_endpoint(path)
            .map_err(|e| format!("cannot connect to {}: {e}", path.display()))?;
        Ok(Client { conn })
    }

    /// Perform the `Hello`/`Welcome` handshake.
    pub fn handshake(&self, role: Role) -> Result<(), String> {
        self.conn
            .send_message(
                &Message::Hello { version: PROTO_VERSION, role, pid: std::process::id() as i32 },
                &[],
            )
            .map_err(|e| e.to_string())?;
        match self.conn.recv_message().map_err(|e| e.to_string())? {
            (Message::Welcome { version, .. }, _) => {
                if version != PROTO_VERSION {
                    return Err(format!("daemon speaks v{version}, we speak v{PROTO_VERSION}"));
                }
                Ok(())
            }
            (Message::Error { message, .. }, _) => {
                Err(format!("daemon rejected handshake: {message}"))
            }
            (other, _) => Err(format!("unexpected handshake reply: {other:?}")),
        }
    }

    pub fn send(&self, msg: &Message) -> Result<(), String> {
        self.conn.send_message(msg, &[]).map_err(|e| e.to_string())
    }

    /// Send a request and read the next reply. Returns any fds the daemon
    /// attached (e.g. an ashmem region).
    pub fn request(&self, msg: &Message) -> Result<(Message, Vec<OwnedFd>), String> {
        self.conn.send_message(msg, &[]).map_err(|e| e.to_string())?;
        let (reply, raw_fds) = self.conn.recv_message().map_err(|e| e.to_string())?;
        // SAFETY: the fds arrived via SCM_RIGHTS and are now ours to own.
        let fds = raw_fds
            .into_iter()
            .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) })
            .collect();
        Ok((reply, fds))
    }

    /// What a surface is showing: `(buffer_w, buffer_h, display_w, display_h)`, or
    /// `None` when the daemon does not have that surface right now.
    ///
    /// An app that replaces its window (a splash screen handing over to the game)
    /// destroys one surface and creates another, and a caller polling across that
    /// gap gets `-3` for a moment. That is an answer, not a failure.
    pub fn surface_geometry(&self, surface_id: u32) -> Result<Option<(u32, u32, u32, u32)>, String> {
        match self.request(&Message::query_surface_geometry(surface_id))?.0 {
            Message::SurfaceGeometry { width, height, display_width, display_height, .. } => {
                Ok(Some((width, height, display_width, display_height)))
            }
            Message::Error { code: -3, .. } => Ok(None),
            Message::Error { message, .. } => Err(message),
            other => Err(format!("unexpected reply to SurfaceGeometry: {:?}", other.kind())),
        }
    }

    /// Maximize, fullscreen, or return a window to normal. `state` is a set of
    /// `pengudroid_proto::WINDOW_*` bits.
    pub fn set_window_state(&self, surface_id: u32, state: u32) -> Result<(), String> {
        match self.request(&Message::SetWindowState { surface_id, state })?.0 {
            Message::Ack => Ok(()),
            Message::Error { message, .. } => Err(message),
            other => Err(format!("unexpected reply to SetWindowState: {:?}", other.kind())),
        }
    }

    /// Like [`Client::request`], but attaches file descriptors to the outgoing
    /// message (used to hand the daemon a GPU-exported dma-buf).
    pub fn request_with_fds(
        &self,
        msg: &Message,
        fds: &[std::os::fd::RawFd],
    ) -> Result<Message, String> {
        self.conn.send_message(msg, fds).map_err(|e| e.to_string())?;
        let (reply, raw_fds) = self.conn.recv_message().map_err(|e| e.to_string())?;
        // We expect no fds back here; owning and dropping them closes them.
        for fd in raw_fds {
            drop(unsafe { OwnedFd::from_raw_fd(fd) });
        }
        Ok(reply)
    }
}

/// Ask the daemon to shut down, and wait for the socket to go away so a
/// `stop; start` sequence in a script cannot race the old process.
///
/// Returns `Ok(true)` if it acknowledged.
pub fn stop() -> Result<bool, String> {
    let c = Client::connect()?;
    c.handshake(Role::Cli)?;
    let acknowledged = matches!(c.request(&Message::Shutdown)?, (Message::Goodbye, _));
    if !acknowledged {
        return Ok(false);
    }
    let socket = default_socket_path();
    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_secs(3) {
        if Connection::connect_endpoint(&socket).is_err() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    Ok(true)
}
