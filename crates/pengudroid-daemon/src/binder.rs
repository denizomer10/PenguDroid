//! Kernel binder driver access.
//!
//! PenguDroid prefers the real in-kernel binder driver when present
//! (`/dev/binderfs/binder` etc.), because that is what gives correct
//! oneway/death-notification semantics with no in-daemon marshalling cost.
//! When the driver is unavailable (very common on desktop Linux, where
//! `CONFIG_ANDROID_BINDER_IPC` is often off, or the module is not loaded and we
//! lack the privileges to load it), the daemon falls back to a userspace broker
//! in [`crate::services`] that speaks the same message shapes over the control
//! socket.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use pengudroid_proto::abi;

/// `struct binder_write_read` (64-bit layout: 6 × 8 bytes = 48).
#[repr(C)]
#[derive(Default, Debug, Clone, Copy)]
pub struct BinderWriteRead {
    pub write_size: u64,
    pub write_consumed: u64,
    pub write_buffer: u64,
    pub read_size: u64,
    pub read_consumed: u64,
    pub read_buffer: u64,
}

/// `struct binder_version` (4 bytes).
#[repr(C)]
#[derive(Default, Debug, Clone, Copy)]
pub struct BinderVersion {
    pub protocol_version: i32,
}

// Keep the ioctl ABI honest: a layout drift here would corrupt transactions.
const _: () = assert!(std::mem::size_of::<BinderWriteRead>() == 48);
const _: () = assert!(std::mem::size_of::<BinderVersion>() == 4);

/// An open handle to the kernel binder driver.
pub struct KernelBinder {
    path: String,
    fd: OwnedFd,
    protocol_version: i32,
}

impl KernelBinder {
    /// Probe the well-known device nodes and return the first that answers
    /// `BINDER_VERSION`. Returns `None` (never an error) so callers can fall
    /// back cleanly.
    pub fn open() -> Option<KernelBinder> {
        for dev in abi::BINDER_DEVICES {
            match Self::open_one(dev) {
                Some(k) => {
                    crate::info!("kernel binder available at {} (protocol {})", dev, k.protocol_version);
                    return Some(k);
                }
                None => crate::debug!("no kernel binder at {}", dev),
            }
        }
        None
    }

    fn open_one(path: &str) -> Option<KernelBinder> {
        let cpath = std::ffi::CString::new(path).ok()?;
        // SAFETY: opening a device path read/write.
        let raw = unsafe { libc::open(cpath.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if raw < 0 {
            return None;
        }
        // SAFETY: fresh fd.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let mut ver = BinderVersion::default();
        // SAFETY: BINDER_VERSION expects a `struct binder_version` output.
        let rc = unsafe { libc::ioctl(fd.as_raw_fd(), abi::BINDER_VERSION, &mut ver) };
        if rc != 0 {
            return None;
        }
        Some(KernelBinder { path: path.to_string(), fd, protocol_version: ver.protocol_version })
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn protocol_version(&self) -> i32 {
        self.protocol_version
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Run a `BINDER_WRITE_READ` round-trip. This is the low-level primitive a
    /// full binder client would drive; the MVP uses it only for a liveness
    /// probe and leaves transaction translation to the userspace broker.
    pub fn write_read(&self, wr: &mut BinderWriteRead) -> std::io::Result<()> {
        // SAFETY: BINDER_WRITE_READ takes a `struct binder_write_read`.
        let rc = unsafe { libc::ioctl(self.fd.as_raw_fd(), abi::BINDER_WRITE_READ, wr) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Probe result for reporting at startup.
pub enum BinderBackend {
    Kernel(KernelBinder),
    UserspaceBroker,
}

impl BinderBackend {
    pub fn describe(&self) -> String {
        match self {
            BinderBackend::Kernel(k) => format!("kernel ({}, proto {})", k.path(), k.protocol_version()),
            BinderBackend::UserspaceBroker => "userspace broker (kernel binder unavailable)".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_linux_encoding() {
        // _IOWR('b', 1, 48) == 0xC0306201, _IOWR('b', 9, 4) == 0xC0046209.
        assert_eq!(abi::BINDER_WRITE_READ, 0xC030_6201);
        assert_eq!(abi::BINDER_VERSION, 0xC004_6209);
    }

    #[test]
    fn probe_never_panics() {
        // On most desktop kernels this is None; on Android it may be Some.
        let _ = KernelBinder::open();
    }
}
