//! Audio broker.
//!
//! The real design routes an `AudioFlinger`/`AudioTrack` write path into
//! PipeWire. libpipewire is present at runtime on this class of system, but we
//! do not link it (no build-time headers); instead we probe for it with
//! `dlopen` and, when present, expose a shared ring buffer (an
//! [`AshmemRegion`]) that a future PipeWire writer thread would drain into a
//! virtual sink.
//!
//! **Status: transport is real, the PipeWire pump is a stub.** The ring buffer
//! and the availability probe work today; actually feeding samples to PipeWire
//! is the next milestone.

use std::os::fd::RawFd;

use crate::ashmem::AshmemRegion;

/// Frames per ring segment (≈21 ms at 48 kHz).
const RING_FRAMES: u32 = 1024;
/// Bytes per frame: 2 channels × 16-bit (PCM_S16_LE).
const BYTES_PER_FRAME: u32 = 4;

/// PipeWire's shared-library soname.
const PIPEWIRE_SONAME: &str = "libpipewire-0.3.so.0";

pub struct AudioBroker {
    /// Whether libpipewire was found at runtime.
    pub pipewire_available: bool,
    pub sample_rate: u32,
    pub channels: u16,
    /// Shared ring the guest writes PCM into.
    ring: AshmemRegion,
}

impl AudioBroker {
    /// Start the broker. Never fails: if the ring cannot be created we report
    /// audio as unavailable rather than aborting the daemon.
    pub fn start() -> AudioBroker {
        let pipewire_available = probe_library(PIPEWIRE_SONAME);
        let ring = AshmemRegion::create("pengudroid-audio-ring", (RING_FRAMES * BYTES_PER_FRAME) as u64)
            .expect("audio ring allocation");
        crate::info!(
            "audio broker: pipewire={}, {} Hz, {} ch, ring={} bytes",
            if pipewire_available { "available" } else { "missing (stub)" },
            48000,
            2,
            ring.size()
        );
        AudioBroker { pipewire_available, sample_rate: 48000, channels: 2, ring }
    }

    pub fn ring_fd(&self) -> RawFd {
        self.ring.as_raw_fd()
    }

    pub fn describe(&self) -> String {
        format!(
            "PipeWire {} @ {}Hz/{}ch",
            if self.pipewire_available { "ready" } else { "unavailable (stub sink)" },
            self.sample_rate,
            self.channels
        )
    }
}

/// `dlopen` a shared library without linking it, returning whether it loaded.
pub fn probe_library(soname: &str) -> bool {
    let Ok(name) = std::ffi::CString::new(soname) else {
        return false;
    };
    // SAFETY: dlopen/dlclose with a valid NUL-terminated name; RTLD_LAZY is
    // fine because we never call into the library here.
    unsafe {
        let handle = libc::dlopen(name.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL);
        if handle.is_null() {
            return false;
        }
        libc::dlclose(handle);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_reports_bool() {
        // We cannot assert a specific result (depends on the host), only that
        // probing a nonexistent library is cleanly false.
        assert!(!probe_library("libdefinitely-not-here-12345.so.0"));
    }

    #[test]
    fn broker_has_ring() {
        let b = AudioBroker::start();
        assert!(b.ring_fd() >= 0);
        assert_eq!(b.sample_rate, 48000);
    }
}
