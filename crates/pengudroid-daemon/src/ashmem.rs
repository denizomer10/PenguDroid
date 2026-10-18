//! Android `ashmem` emulation on top of `memfd_create`.
//!
//! Modern Android no longer has the legacy `/dev/ashmem` driver; `libcutils`
//! itself implements ashmem with a memfd. PenguDroid does the same, and adds
//! `MFD_ALLOW_SEALING` so that [`AshmemRegion::seal`] can emulate the
//! pinning semantics that protect a region from shrinking once a guest has
//! mapped it executable.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use pengudroid_proto::abi;

/// A shared-memory region handed to guests as an fd (over the control socket)
/// and mapped by the daemon to read/write pixels.
pub struct AshmemRegion {
    name: String,
    size: u64,
    fd: OwnedFd,
}

impl AshmemRegion {
    /// Create a region of `size` bytes. `size` of 0 is rejected, matching
    /// ashmem's contract.
    pub fn create(name: &str, size: u64) -> io::Result<AshmemRegion> {
        if size == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "ashmem size may not be 0"));
        }
        let cname = std::ffi::CString::new(name)
            .unwrap_or_else(|_| std::ffi::CString::new("ashmem").unwrap());
        // SAFETY: `cname` is a valid NUL-terminated C string; flags are plain
        // bit values. memfd_create returns a fresh fd or -1.
        let raw = unsafe {
            libc::memfd_create(cname.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING)
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a fresh fd we exclusively own.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: `fd` is valid and `size` fits in i64 for realistic sizes.
        if unsafe { libc::ftruncate(fd.as_raw_fd(), size as libc::off_t) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(AshmemRegion { name: name.to_string(), size, fd })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Duplicate the fd for passing to a peer via `SCM_RIGHTS`.
    pub fn try_clone_fd(&self) -> io::Result<OwnedFd> {
        self.fd.try_clone()
    }

    /// Apply shrink/grow seals, emulating a pinned ashmem region. Irreversible.
    pub fn seal(&self) -> io::Result<()> {
        let seals = abi::F_SEAL_SHRINK_MASK | abi::F_SEAL_GROW_MASK;
        // SAFETY: `fd` is valid; F_ADD_SEALS with these flags cannot fail in a
        // way that invalidates the fd.
        let rc = unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_ADD_SEALS, seals as libc::c_int) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Map the region read/write. Used by the graphics broker to fill pixels.
    pub fn map_mut(&self) -> io::Result<Mapping> {
        Mapping::new(self.fd.as_raw_fd(), self.size)
    }
}

/// An `mmap`ed region that unmaps itself on drop.
pub struct Mapping {
    ptr: *mut u8,
    len: usize,
}

impl Mapping {
    fn new(fd: RawFd, len: u64) -> io::Result<Mapping> {
        let len = usize::try_from(len)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "region too large to map"))?;
        // SAFETY: mmap of a valid fd with PROT_READ|PROT_WRITE. MAP_SHARED so
        // peer writes are visible.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Mapping { ptr: ptr as *mut u8, len })
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: the mapping is valid for `len` bytes and we have exclusive
        // mutable access via `&mut self`.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }

    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: as above, shared view.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    /// Fill the whole mapping with a solid RGBA color (used for a placeholder
    /// frame until a real guest renders into it).
    pub fn fill_rgba(&mut self, r: u8, g: u8, b: u8, a: u8) {
        for px in self.as_mut_slice().chunks_exact_mut(4) {
            px[0] = b; // Wayland ARGB8888 is little-endian B,G,R,A in memory
            px[1] = g;
            px[2] = r;
            px[3] = a;
        }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`len` came from a successful mmap we own.
        unsafe {
            libc::munmap(self.ptr as *mut libc::c_void, self.len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_map() {
        let region = AshmemRegion::create("test", 4096).unwrap();
        assert_eq!(region.size(), 4096);
        assert!(region.as_raw_fd() >= 0);
        let mut map = region.map_mut().unwrap();
        map.fill_rgba(255, 0, 0, 255);
        assert_eq!(&map.as_slice()[..4], &[0, 0, 255, 255]);
    }

    #[test]
    fn zero_size_rejected() {
        assert!(AshmemRegion::create("z", 0).is_err());
    }

    #[test]
    fn seal_blocks_grow() {
        let region = AshmemRegion::create("seal", 4096).unwrap();
        region.seal().unwrap();
        // Growing a sealed region must fail with EPERM.
        let rc = unsafe { libc::ftruncate(region.as_raw_fd(), 8192) };
        assert_eq!(rc, -1);
    }

    #[test]
    fn clone_fd_shares_backing() {
        let region = AshmemRegion::create("clone", 8192).unwrap();
        let dup = region.try_clone_fd().unwrap();
        let mut map = Mapping::new(dup.as_raw_fd(), 8192).unwrap();
        map.fill_rgba(1, 2, 3, 4);
        // Original mapping sees the same bytes.
        let map2 = region.map_mut().unwrap();
        assert_eq!(&map2.as_slice()[..4], &[3, 2, 1, 4]);
    }
}
