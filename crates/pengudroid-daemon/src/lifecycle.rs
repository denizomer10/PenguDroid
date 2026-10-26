//! Bounded guest shutdown using a stable Linux process handle.
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

/// A process identity captured when its first surface is registered. Keeping
/// this handle also lets the daemon retire windows after an unexpected exit.
pub(crate) struct GuestProcess {
    pid: i32,
    fd: OwnedFd,
}

impl GuestProcess {
    pub fn open(pid: i32) -> io::Result<Self> {
        if pid <= 1 || pid == std::process::id() as i32 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid guest pid"));
        }
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if raw < 0 { return Err(io::Error::last_os_error()); }
        Ok(Self { pid, fd: unsafe { OwnedFd::from_raw_fd(raw as i32) } })
    }

    pub fn exited(&self) -> bool {
        let mut descriptor = libc::pollfd { fd: self.fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        unsafe { libc::poll(&mut descriptor, 1, 0) > 0 && descriptor.revents & (libc::POLLIN | libc::POLLHUP) != 0 }
    }

    /// Bound shutdown to this identity, even if a numeric PID is later reused.
    pub fn stop(&self) -> io::Result<()> {
        let fd = self.fd.try_clone()?;
        signal(&fd, libc::SIGTERM)?;
        let pid = self.pid;
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            if let Err(e) = signal(&fd, libc::SIGKILL) {
                if e.raw_os_error() != Some(libc::ESRCH) {
                    crate::warn!("cannot force guest {pid} to exit: {e}");
                }
            }
        });
        Ok(())
    }
}

pub(crate) fn stop_guest(pid: i32) -> io::Result<()> {
    GuestProcess::open(pid)?.stop()
}

fn signal(fd: &OwnedFd, signal: i32) -> io::Result<()> {
    let rc = unsafe { libc::syscall(libc::SYS_pidfd_send_signal,
        fd.as_raw_fd(), signal, std::ptr::null::<libc::siginfo_t>(), 0) };
    if rc < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::process::{Command, Stdio};
    #[test]
    fn hung_guest_is_killed_after_grace_period() {
        let mut child = Command::new("/bin/sh").args(["-c", "trap '' TERM; echo ready; exec sleep 30"])
            .stdout(Stdio::piped()).spawn().unwrap();
        let mut ready = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap()).read_line(&mut ready).unwrap();
        stop_guest(child.id() as i32).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let result = loop {
            if let Some(status) = child.try_wait().unwrap() { break Some(status); }
            if std::time::Instant::now() > deadline { break None; }
            std::thread::sleep(Duration::from_millis(10));
        };
        let _ = child.kill(); let _ = child.wait();
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(result.expect("hung process survived close").signal(), Some(libc::SIGKILL));
    }
}
