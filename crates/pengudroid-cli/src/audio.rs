//! Audio output through PipeWire.
//!
//! PenguDroid's audio plan is: the daemon owns a shared PCM ring (an ashmem
//! region) and a sink drains it into the user's sound server. This module
//! implements the sink itself and a self-test command.
//!
//! Rather than bind `libpipewire` (whose C ABI needs many structs and headers),
//! we drive PipeWire's own `pw-cat` tool over a pipe: it accepts raw PCM on
//! stdin and plays it. That is a real stream into the session's PipeWire graph,
//! just with a process boundary. `aplay` is the fallback.
//!
//! The self-test writes **silence** by default so running it cannot disturb
//! anyone; `--tone` opts into an audible 440 Hz sine.

use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd};
use std::process::{Child, ChildStdin, Command, Stdio};

use pengudroid_proto::{Message, Role};

use crate::client::Client;

/// A running playback stream (one subprocess reading raw PCM from our pipe).
pub struct AudioSink {
    child: Child,
    stdin: Option<ChildStdin>,
    /// Which backend was used (`pw-cat` or `aplay`).
    pub backend: &'static str,
    pub rate: u32,
    pub channels: u16,
}

/// Look up an executable on `PATH`.
pub fn find_in_path(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

impl AudioSink {
    /// Spawn a playback stream at `rate` / `channels` (16-bit signed LE).
    pub fn spawn(rate: u32, channels: u16) -> Result<AudioSink, String> {
        let rate_s = rate.to_string();
        let channels_s = channels.to_string();

        let mut cmd = if find_in_path("pw-cat").is_some() {
            let mut c = Command::new("pw-cat");
            c.args(["-p", "-a", "--rate", &rate_s, "--channels", &channels_s, "--format", "s16", "-"]);
            c
        } else if find_in_path("aplay").is_some() {
            let mut c = Command::new("aplay");
            c.args(["-q", "-t", "raw", "-f", "S16_LE", "-r", &rate_s, "-c", &channels_s, "-"]);
            c
        } else {
            return Err("neither `pw-cat` nor `aplay` is on PATH".into());
        };

        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot start the audio sink: {e}"))?;
        let stdin = child.stdin.take();
        let backend = if find_in_path("pw-cat").is_some() { "pw-cat" } else { "aplay" };
        Ok(AudioSink { child, stdin, backend, rate, channels })
    }

    /// Write raw PCM (16-bit LE, interleaved) to the stream.
    pub fn write(&mut self, pcm: &[u8]) -> std::io::Result<()> {
        match self.stdin.as_mut() {
            Some(stdin) => stdin.write_all(pcm),
            None => Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "stream closed")),
        }
    }

    /// Close the stream and wait for the backend to finish.
    pub fn finish(mut self) -> Result<std::process::ExitStatus, String> {
        // Dropping stdin signals EOF, which makes pw-cat/aplay drain and exit.
        drop(self.stdin.take());
        self.child.wait().map_err(|e| format!("waiting for the audio sink failed: {e}"))
    }
}

/// Generate interleaved 16-bit LE stereo PCM: silence unless `tone_hz` is set.
pub fn generate_pcm(seconds: f32, rate: u32, channels: u16, tone_hz: Option<f32>) -> Vec<u8> {
    let frames = (seconds.max(0.0) * rate as f32) as usize;
    let mut out = Vec::with_capacity(frames * channels as usize * 2);
    for n in 0..frames {
        let sample = match tone_hz {
            Some(hz) => {
                let t = n as f32 / rate as f32;
                // 0.1 amplitude: audible but quiet.
                ((t * hz * std::f32::consts::TAU).sin() * 0.1 * i16::MAX as f32) as i16
            }
            None => 0,
        };
        let bytes = sample.to_le_bytes();
        for _ in 0..channels {
            out.extend_from_slice(&bytes);
        }
    }
    out
}

/// `pengudroid audio-test [--seconds N] [--tone] [--via-daemon]`
pub fn test(args: &[String]) -> i32 {
    let mut seconds = 0.5f32;
    let mut tone = false;
    let mut via_daemon = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--seconds" => match it.next().and_then(|v| v.parse::<f32>().ok()) {
                Some(v) => seconds = v,
                None => {
                    eprintln!("pengudroid: --seconds needs a number");
                    return 2;
                }
            },
            "--tone" => tone = true,
            "--via-daemon" => via_daemon = true,
            other => {
                eprintln!("pengudroid: unknown argument {other}");
                return 2;
            }
        }
    }

    if via_daemon {
        return via_daemon_ring(seconds, tone);
    }

    const RATE: u32 = 48_000;
    const CHANNELS: u16 = 2;

    let mut sink = match AudioSink::spawn(RATE, CHANNELS) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("pengudroid: {e}");
            return 1;
        }
    };
    println!(
        "audio: {} @ {} Hz, {} ch, {:.2}s of {}",
        sink.backend,
        sink.rate,
        sink.channels,
        seconds,
        if tone { "440 Hz tone" } else { "silence" }
    );

    let pcm = generate_pcm(seconds, RATE, CHANNELS, if tone { Some(440.0) } else { None });
    if let Err(e) = sink.write(&pcm) {
        eprintln!("pengudroid: writing PCM failed: {e}");
        return 1;
    }

    match sink.finish() {
        Ok(status) if status.success() => {
            println!("audio: stream completed cleanly");
            0
        }
        Ok(status) => {
            eprintln!("pengudroid: audio backend exited with {status}");
            1
        }
        Err(e) => {
            eprintln!("pengudroid: {e}");
            1
        }
    }
}

/// A shared mapping of the daemon's audio ring.
struct Map {
    ptr: *mut u8,
    len: usize,
}

impl Map {
    fn new(fd: &OwnedFd, len: usize) -> std::io::Result<Map> {
        // SAFETY: mmap of a valid memfd, PROT_READ|PROT_WRITE, MAP_SHARED.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Map { ptr: ptr as *mut u8, len })
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: the mapping is valid for `len` bytes.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for Map {
    fn drop(&mut self) {
        // SAFETY: ptr/len came from a successful mmap we own.
        unsafe {
            libc::munmap(self.ptr as *mut libc::c_void, self.len);
        }
    }
}

/// Ask the daemon for its ring, fill it with PCM and ask the daemon to play it.
fn via_daemon_ring(seconds: f32, tone: bool) -> i32 {
    let client = match Client::connect().and_then(|c| {
        c.handshake(Role::Cli)?;
        Ok(c)
    }) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("pengudroid: {e}");
            return 1;
        }
    };

    let (reply, fds) = match client.request(&Message::GetAudioRing) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("pengudroid: {e}");
            return 1;
        }
    };
    let (rate, channels, size) = match reply {
        Message::AudioRing { rate, channels, size } => (rate, channels, size),
        Message::Error { message, .. } => {
            eprintln!("daemon: {message}");
            return 1;
        }
        other => {
            eprintln!("pengudroid: unexpected reply {other:?}");
            return 1;
        }
    };
    let Some(fd) = fds.into_iter().next() else {
        eprintln!("pengudroid: daemon sent no audio-ring fd");
        return 1;
    };
    let mut map = match Map::new(&fd, size as usize) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("pengudroid: cannot map the audio ring: {e}");
            return 1;
        }
    };

    let pcm = generate_pcm(seconds, rate, channels, if tone { Some(440.0) } else { None });
    let mut sent = 0usize;
    while sent < pcm.len() {
        let n = (pcm.len() - sent).min(size as usize);
        map.as_mut_slice()[..n].copy_from_slice(&pcm[sent..sent + n]);
        match client.request(&Message::AudioWrite { offset: 0, len: n as u32 }) {
            Ok((Message::Ack, _)) => {}
            Ok((Message::Error { message, .. }, _)) => {
                eprintln!("daemon: {message}");
                return 1;
            }
            Ok((other, _)) => {
                eprintln!("pengudroid: unexpected reply {other:?}");
                return 1;
            }
            Err(e) => {
                eprintln!("pengudroid: {e}");
                return 1;
            }
        }
        sent += n;
    }

    println!(
        "audio: sent {sent} bytes through the daemon ring ({rate} Hz, {channels} ch)"
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_is_zeroed_and_sized() {
        let pcm = generate_pcm(0.1, 48_000, 2, None);
        assert_eq!(pcm.len(), 4_800 * 2 * 2);
        assert!(pcm.iter().all(|&b| b == 0));
    }

    #[test]
    fn tone_is_nonzero() {
        let pcm = generate_pcm(0.05, 48_000, 2, Some(440.0));
        assert!(pcm.iter().any(|&b| b != 0));
        // Interleaved: consecutive stereo samples are identical.
        assert_eq!(&pcm[0..2], &pcm[2..4]);
    }

    #[test]
    fn find_in_path_finds_sh() {
        assert!(find_in_path("sh").is_some());
        assert!(find_in_path("definitely-not-a-program-xyz").is_none());
    }
}
