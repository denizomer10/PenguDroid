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
        let conn = Connection::connect(path)
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
}

/// Ask the daemon to shut down. Returns `Ok(true)` if it acknowledged.
pub fn stop() -> Result<bool, String> {
    let c = Client::connect()?;
    c.handshake(Role::Cli)?;
    match c.request(&Message::Shutdown)? {
        (Message::Goodbye, _) => Ok(true),
        _ => Ok(false),
    }
}
