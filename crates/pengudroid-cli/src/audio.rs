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
use std::process::{Child, ChildStdin, Command, Stdio};

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

/// `pengudroid audio-test [--seconds N] [--tone]`
pub fn test(args: &[String]) -> i32 {
    let mut seconds = 0.5f32;
    let mut tone = false;
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
            other => {
                eprintln!("pengudroid: unknown argument {other}");
                return 2;
            }
        }
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
