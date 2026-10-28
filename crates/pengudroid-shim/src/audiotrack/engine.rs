//! Bounded PCM streaming. The head/timestamp advance only after sink pipe writes.
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

pub(super) const PCM_16: i32 = 2;
pub(super) const PCM_FLOAT: i32 = 4;
pub(super) const OUTPUT_RATE: i32 = 48_000;
pub(super) const BAD_VALUE: i32 = -2;
pub(super) const INVALID_OPERATION: i32 = -3;
pub(super) const DEAD_OBJECT: i32 = -6;
pub(super) const MAX_BUFFER_BYTES: usize = 16 * 1024 * 1024;
const MAX_TRACKS: usize = 256;
const HANDLE_TAG: u64 = 0x5041_0000_0000_0000;
const HANDLE_MASK: u64 = 0xffff_0000_0000_0000;
const HANDLE_SEQUENCE: u64 = !HANDLE_MASK;
const SINK_CHUNK: usize = 2048;
const MAX_SINK_RESTARTS: u32 = 5;

static NEXT_SESSION: AtomicI64 = AtomicI64::new(1);
static NEXT_PLAYER: AtomicI64 = AtomicI64::new(1);
static NEXT_TRACK: AtomicU64 = AtomicU64::new(1);

fn allocate_id(counter: &AtomicI64) -> i32 {
    let mut id = counter.load(Ordering::Relaxed);
    loop {
        if !(1..=i32::MAX as i64).contains(&id) { return -1; }
        match counter.compare_exchange_weak(id, id + 1, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return id as i32,
            Err(current) => id = current,
        }
    }
}
pub(super) fn new_session() -> i32 { allocate_id(&NEXT_SESSION) }
pub(super) fn new_player() -> i32 { allocate_id(&NEXT_PLAYER) }
pub(super) fn session(requested: i32) -> Result<i32, i32> {
    match requested {
        1.. => Ok(requested),
        0 => { let id = new_session(); if id > 0 { Ok(id) } else { Err(INVALID_OPERATION) } }
        _ => Err(BAD_VALUE),
    }
}

pub(super) fn sample_bytes(encoding: i32) -> Result<u32, i32> {
    match encoding { PCM_16 => Ok(2), PCM_FLOAT => Ok(4), _ => Err(BAD_VALUE) }
}
pub(super) fn valid_rate(rate: i32) -> bool { (4_000..=192_000).contains(&rate) }
pub(super) fn min_buffer(rate: i32, channels: i32, encoding: i32) -> i32 {
    if !valid_rate(rate) || !(1..=8).contains(&channels) { return BAD_VALUE; }
    let Ok(bytes) = sample_bytes(encoding) else { return BAD_VALUE; };
    // Twenty milliseconds, rounded up in frames, without a hardware query.
    ((rate + 49) / 50) * channels * bytes as i32
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ChannelMasks {
    pub(super) position: i32,
    pub(super) index: i32,
    pub(super) acn: i32,
    pub(super) count: i32,
}
impl ChannelMasks {
    pub(super) fn channels(self) -> Result<u32, i32> {
        // Java output-position bits start at FRONT_LEFT=4, not the native bit=1.
        // No ambisonics, haptics, or guessed speaker remapping in the host sink.
        const SUPPORTED_POSITIONS: u32 = 0xfc | 0x100 | 0x200 | 0x400 | 0x800 | 0x1000 | 0x2000 | 0x40000 | 0x80000;
        let position = self.position as u32;
        let index = self.index as u32;
        if self.acn != 0 || position & !SUPPORTED_POSITIONS != 0 || self.index < 0
            || !(1..=8).contains(&self.count) || (position == 0 && index == 0)
            || (position != 0 && position.count_ones() != self.count as u32)
            || (index != 0 && index.count_ones() != self.count as u32)
        { return Err(BAD_VALUE); }
        // The sink only accepts canonical mono/stereo and contiguous index order.
        // Larger positional layouts need an explicit Android -> sink map.
        if position != 0 && position != 4 && position != 12 { return Err(INVALID_OPERATION); }
        if index != 0 && index != (1u32 << self.count) - 1 { return Err(INVALID_OPERATION); }
        Ok(self.count as u32)
    }
}

pub(super) struct Shared {
    pub(super) queue: VecDeque<u8>,
    in_flight: usize,
    generation: u64,
    pub(super) played: u64,
    base: u64,
    pub(super) written: u64,
    pub(super) timestamp_ns: i64,
    pub(super) playing: bool,
    pub(super) alive: bool,
    stopping: bool,
    reset_on_start: bool,
    primed: bool,
    pub(super) buffer_frames: u32,
    pub(super) start_threshold: u32,
    pub(super) volume: [f32; 2],
    pub(super) player_id: i32,
    pub(super) log_session_id: String,
}
impl Shared {
    pub(super) fn position(&self) -> u64 {
        if self.reset_on_start { 0 } else { self.played.saturating_sub(self.base) }
    }
    fn occupied(&self) -> usize { self.queue.len() + self.in_flight }
}

pub(super) struct Track {
    pub(super) rate: u32,
    pub(super) channels: u32,
    pub(super) encoding: i32,
    pub(super) bytes_per_sample: u32,
    pub(super) capacity_frames: u32,
    pub(super) shared: Arc<Mutex<Shared>>,
    pump: Mutex<Option<std::thread::JoinHandle<()>>>,
}
impl Track {
    pub(super) fn new(rate: i32, channels: u32, encoding: i32, buffer: i32) -> Result<Self, i32> {
        if !valid_rate(rate) || !(1..=8).contains(&channels) { return Err(BAD_VALUE); }
        let bytes_per_sample = sample_bytes(encoding)?;
        let frame = (channels * bytes_per_sample) as usize;
        if buffer <= 0 || buffer as usize > MAX_BUFFER_BYTES || buffer as usize % frame != 0 {
            return Err(BAD_VALUE);
        }
        let capacity_frames = buffer as u32 / frame as u32;
        Ok(Self {
            rate: rate as u32, channels, encoding, bytes_per_sample, capacity_frames,
            shared: Arc::new(Mutex::new(Shared {
                queue: VecDeque::new(), in_flight: 0, generation: 0, played: 0, base: 0,
                written: 0, timestamp_ns: 0, playing: false, alive: true, stopping: false,
                reset_on_start: false, primed: false, buffer_frames: capacity_frames,
                start_threshold: 1, volume: [1.0, 1.0], player_id: 0, log_session_id: String::new(),
            })),
            pump: Mutex::new(None),
        })
    }
    pub(super) fn source_frame(&self) -> usize { (self.channels * self.bytes_per_sample) as usize }
    fn sink_frame(&self) -> usize { self.channels as usize * 2 }
    pub(super) fn resize(&self, frames: i32) -> i32 {
        if frames < 0 { return BAD_VALUE; }
        let mut sh = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        sh.buffer_frames = (frames as u32).clamp(1, self.capacity_frames);
        sh.start_threshold = sh.start_threshold.min(sh.buffer_frames);
        sh.buffer_frames as i32
    }
    pub(super) fn set_threshold(&self, frames: i32) -> i32 {
        if frames <= 0 { return BAD_VALUE; }
        let mut sh = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        sh.start_threshold = (frames as u32).min(sh.buffer_frames);
        sh.start_threshold as i32
    }
    pub(super) fn pause(&self) {
        let mut sh = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        sh.playing = false;
        sh.stopping = false;
    }
    pub(super) fn stop(&self) {
        let has_pump = self.pump.lock().unwrap_or_else(|e| e.into_inner()).is_some();
        let mut sh = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        // Streaming stop drains previously queued PCM; pause, unlike stop, retains it.
        sh.stopping = true;
        sh.playing = has_pump;
        sh.reset_on_start = true;
        sh.written = 0;
        sh.timestamp_ns = 0;
    }
    pub(super) fn flush(&self) {
        let mut sh = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        if sh.playing { return; } // Android streaming flush has no effect while playing.
        sh.queue.clear();
        sh.in_flight = 0;
        sh.generation = sh.generation.wrapping_add(1);
        sh.base = sh.played;
        sh.written = 0;
        sh.timestamp_ns = 0;
        sh.primed = false;
    }
    pub(super) fn release(&self) {
        let mut sh = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        sh.alive = false;
        sh.playing = false;
        sh.queue.clear();
    }
    pub(super) fn start(&self) -> Result<(), i32> {
        self.start_with(crate::spawn_sink)
    }
    fn start_with(&self, spawn_sink: impl FnMut(u32, u32) -> Option<crate::Sink> + Send + 'static) -> Result<(), i32> {
        if !self.shared.lock().unwrap_or_else(|e| e.into_inner()).alive { return Err(DEAD_OBJECT); }
        start_pump_with(self, spawn_sink)?;
        let mut sh = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        if !sh.alive { return Err(DEAD_OBJECT); }
        if sh.reset_on_start {
            sh.base = sh.played;
            sh.timestamp_ns = 0;
            sh.reset_on_start = false;
            sh.primed = false;
        }
        sh.playing = true;
        sh.stopping = false;
        Ok(())
    }
    /// Return source bytes, not sink bytes. No large temporary allocation is made
    /// for data that does not fit, and an in-flight chunk still occupies capacity.
    pub(super) fn write(&self, bytes: &[u8], blocking: bool) -> i32 {
        let mut consumed = 0;
        loop {
            let result = push_pcm(self, &bytes[consumed..]);
            if result < 0 { return if consumed == 0 { result } else { consumed as i32 }; }
            consumed += result as usize;
            if !blocking || bytes.len() - consumed < self.source_frame() { return consumed as i32; }
            std::thread::sleep(Duration::from_millis(3));
        }
    }
}
impl Drop for Track {
    fn drop(&mut self) {
        self.release();
        // The pipe is nonblocking, so the owned worker will notice release and
        // reap its sink without blocking a Java finalizer on an unresponsive server.
    }
}

fn tracks() -> &'static Mutex<HashMap<u64, Arc<Track>>> {
    static TRACKS: OnceLock<Mutex<HashMap<u64, Arc<Track>>>> = OnceLock::new();
    TRACKS.get_or_init(|| Mutex::new(HashMap::new()))
}
pub(super) fn insert(track: Track) -> Result<u64, i32> {
    let mut tracks = tracks().lock().unwrap_or_else(|e| e.into_inner());
    if tracks.len() >= MAX_TRACKS { return Err(INVALID_OPERATION); }
    // The registry lock also serializes this non-wrapping sequence allocator.
    let seq = NEXT_TRACK.load(Ordering::Relaxed);
    if seq == 0 || seq > HANDLE_SEQUENCE { return Err(INVALID_OPERATION); }
    NEXT_TRACK.store(seq + 1, Ordering::Relaxed);
    let handle = HANDLE_TAG | seq;
    tracks.insert(handle, Arc::new(track));
    Ok(handle)
}
pub(super) fn get(handle: i64) -> Option<Arc<Track>> {
    let handle = handle as u64;
    if handle & HANDLE_MASK != HANDLE_TAG { return None; }
    tracks().lock().unwrap_or_else(|e| e.into_inner()).get(&handle).cloned()
}
pub(super) fn remove(handle: i64) -> bool {
    if (handle as u64) & HANDLE_MASK != HANDLE_TAG { return false; }
    let track = tracks().lock().unwrap_or_else(|e| e.into_inner()).remove(&(handle as u64));
    if let Some(track) = track { track.release(); true } else { false }
}

pub(super) fn checked_range(length: i64, offset: i32, size: i32) -> Result<std::ops::Range<usize>, i32> {
    if length < 0 || offset < 0 || size < 0 { return Err(BAD_VALUE); }
    let end = i64::from(offset) + i64::from(size);
    if end > length || end > isize::MAX as i64 { return Err(BAD_VALUE); }
    Ok(offset as usize..end as usize)
}

fn to_s16(encoding: i32, bytes: &[u8], channels: u32, volume: [f32; 2]) -> Vec<u8> {
    let sample_bytes = if encoding == PCM_FLOAT { 4 } else { 2 };
    let mut out = Vec::with_capacity(bytes.len() / sample_bytes * 2);
    for (i, chunk) in bytes.chunks_exact(sample_bytes).enumerate() {
        let gain = match channels { 1 => (volume[0] + volume[1]) * 0.5, 2 => volume[i % 2], _ => volume[0] };
        let sample = if encoding == PCM_FLOAT {
            let v = f32::from_le_bytes(chunk.try_into().unwrap());
            let v = if v.is_nan() { 0.0 } else { v.clamp(-1.0, 1.0) };
            (v * gain * 32767.0).round() as i16
        } else {
            (i16::from_le_bytes(chunk.try_into().unwrap()) as f32 * gain).round() as i16
        };
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

fn push_pcm(track: &Track, bytes: &[u8]) -> i32 {
    let mut sh = track.shared.lock().unwrap_or_else(|e| e.into_inner());
    if !sh.alive { return DEAD_OBJECT; }
    let capacity = sh.buffer_frames as usize * track.sink_frame();
    let frames = (capacity.saturating_sub(sh.occupied()) / track.sink_frame())
        .min(bytes.len() / track.source_frame());
    let consumed = frames * track.source_frame();
    if consumed == 0 { return 0; }
    let converted = to_s16(track.encoding, &bytes[..consumed], track.channels, sh.volume);
    sh.queue.extend(converted);
    sh.written = sh.written.saturating_add(frames as u64);
    consumed as i32
}

pub(super) fn monotonic_ns() -> i64 {
    let mut time = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } == 0 {
        time.tv_sec.saturating_mul(1_000_000_000).saturating_add(time.tv_nsec)
    } else { 0 }
}

fn nonblocking(sink: &crate::Sink) -> bool {
    use std::os::fd::AsRawFd;
    let Some(pipe) = sink.stdin.as_ref() else { return false; };
    unsafe {
        let flags = libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL);
        flags >= 0 && libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) == 0
    }
}

fn start_pump_with(track: &Track, mut spawn_sink: impl FnMut(u32, u32) -> Option<crate::Sink> + Send + 'static) -> Result<(), i32> {
    let mut pump = track.pump.lock().unwrap_or_else(|e| e.into_inner());
    if pump.is_some() {
        return if track.shared.lock().unwrap_or_else(|e| e.into_inner()).alive { Ok(()) } else { Err(DEAD_OBJECT) };
    }
    let sink = spawn_sink(track.rate, track.channels);
    let Some(sink) = sink.filter(nonblocking) else {
        track.shared.lock().unwrap_or_else(|e| e.into_inner()).alive = false;
        eprintln!("audio: AudioTrack has no writable pw-cat/aplay sink");
        return Err(DEAD_OBJECT);
    };
    let shared = Arc::clone(&track.shared);
    let (rate, channels, frame) = (track.rate, track.channels, track.sink_frame());
    let worker = std::thread::Builder::new().name("pengudroid-audio".into()).spawn(move || {
        use std::io::Write;
        let mut sink = sink;
        let mut restarts = 0;
        loop {
            let (chunk, generation) = {
                let mut sh = shared.lock().unwrap_or_else(|e| e.into_inner());
                if !sh.alive { break; }
                if sh.stopping && sh.queue.is_empty() {
                    sh.playing = false;
                    sh.stopping = false;
                }
                if !sh.primed && (sh.queue.len() >= sh.start_threshold as usize * frame || sh.stopping) {
                    sh.primed = true;
                }
                if !sh.playing || !sh.primed || sh.queue.is_empty() {
                    drop(sh);
                    std::thread::sleep(Duration::from_millis(3));
                    continue;
                }
                let size = (sh.queue.len().min(SINK_CHUNK) / frame) * frame;
                let bytes: Vec<_> = sh.queue.drain(..size).collect();
                sh.in_flight = size;
                (bytes, sh.generation)
            };
            let mut offset = 0;
            while offset < chunk.len() {
                {
                    let sh = shared.lock().unwrap_or_else(|e| e.into_inner());
                    if !sh.alive { return; }
                    if sh.generation != generation { break; }
                    if !sh.playing {
                        drop(sh);
                        std::thread::sleep(Duration::from_millis(3));
                        continue;
                    }
                }
                let alive = matches!(sink.child.try_wait(), Ok(None));
                let result = if alive {
                    sink.stdin.as_mut().unwrap().write(&chunk[offset..])
                } else { Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "audio sink exited")) };
                match result {
                    Ok(n) if n > 0 => {
                        let mut sh = shared.lock().unwrap_or_else(|e| e.into_inner());
                        if sh.generation == generation {
                            sh.in_flight = sh.in_flight.saturating_sub(n);
                            let frames = (offset + n) / frame - offset / frame;
                            sh.played = sh.played.saturating_add(frames as u64);
                            if frames != 0 { sh.timestamp_ns = monotonic_ns(); }
                        }
                        offset += n;
                    }
                    Err(ref e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted) => {
                        std::thread::sleep(Duration::from_millis(3));
                    }
                    error => {
                        if restarts >= MAX_SINK_RESTARTS {
                            eprintln!("audio: AudioTrack sink restart budget exhausted: {error:?}");
                            shared.lock().unwrap_or_else(|e| e.into_inner()).alive = false;
                            return;
                        }
                        restarts += 1;
                        std::thread::sleep(Duration::from_millis(150));
                        match spawn_sink(rate, channels).filter(nonblocking) {
                            Some(fresh) => {
                                sink = fresh;
                                eprintln!("audio: AudioTrack sink restarted ({restarts}/{MAX_SINK_RESTARTS})");
                            }
                            None => {
                                shared.lock().unwrap_or_else(|e| e.into_inner()).alive = false;
                                return;
                            }
                        }
                    }
                }
            }
        }
    }).map_err(|error| {
        track.shared.lock().unwrap_or_else(|e| e.into_inner()).alive = false;
        eprintln!("audio: cannot start AudioTrack sink worker: {error}");
        DEAD_OBJECT
    })?;
    *pump = Some(worker);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    static NEXT_CAPTURE: AtomicU64 = AtomicU64::new(1);

    fn track(bytes: u32, channels: u32, capacity: usize) -> Track {
        Track::new(48_000, channels, if bytes == 4 { PCM_FLOAT } else { PCM_16 }, capacity as i32).unwrap()
    }
    fn dead_sink() -> crate::Sink {
        let mut child = std::process::Command::new("/bin/cat").stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null()).spawn().unwrap();
        let stdin = child.stdin.take();
        child.kill().unwrap(); child.wait().unwrap();
        crate::Sink { child, stdin }
    }
    fn finish_pump(t: &Track, done: impl Fn(&Shared) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        let reached = loop {
            if done(&t.shared.lock().unwrap()) { break true; }
            if Instant::now() >= deadline { break false; }
            std::thread::sleep(Duration::from_millis(5));
        };
        t.release();
        if let Some(pump) = t.pump.lock().unwrap().take() { pump.join().unwrap(); }
        reached
    }
    #[test]
    fn sink_restart_preserves_pcm_order_and_playback_head() {
        let t = track(2, 2, SINK_CHUNK * 3);
        let pcm: Vec<u8> = (0..SINK_CHUNK * 3).map(|i| (i % 251) as u8).collect();
        assert_eq!(t.write(&pcm, false), pcm.len() as i32);
        let path = std::env::temp_dir().join(format!("pengudroid-audio-restart-{}-{}.pcm", std::process::id(), NEXT_CAPTURE.fetch_add(1, Ordering::Relaxed)));
        let output = std::fs::OpenOptions::new().write(true).create_new(true).open(&path).unwrap();
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = Arc::clone(&attempts);
        t.start_with(move |rate, channels| {
            assert_eq!((rate, channels), (48_000, 2));
            if count.fetch_add(1, Ordering::SeqCst) == 0 { return Some(dead_sink()); }
            let mut child = std::process::Command::new("/bin/cat").stdin(std::process::Stdio::piped())
                .stdout(output.try_clone().unwrap()).spawn().unwrap();
            let stdin = child.stdin.take(); Some(crate::Sink { child, stdin })
        }).unwrap();
        assert!(finish_pump(&t, |sh| sh.played == (pcm.len() / 4) as u64 && std::fs::metadata(&path).unwrap().len() == pcm.len() as u64));
        let captured = std::fs::read(&path).unwrap(); std::fs::remove_file(path).unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(captured, pcm);
        assert_eq!(t.shared.lock().unwrap().position(), (pcm.len() / 4) as u64);
    }
    #[test]
    fn permanently_broken_sink_stops_after_the_restart_budget() {
        let t = track(2, 2, 32);
        assert_eq!(t.write(&[0; 32], false), 32);
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = Arc::clone(&attempts);
        t.start_with(move |_, _| { count.fetch_add(1, Ordering::SeqCst); Some(dead_sink()) }).unwrap();
        assert!(finish_pump(&t, |sh| !sh.alive));
        assert_eq!(attempts.load(Ordering::SeqCst), MAX_SINK_RESTARTS as usize + 1);
        assert_eq!(t.shared.lock().unwrap().position(), 0);
        assert_eq!(t.write(&[0; 4], false), DEAD_OBJECT);
    }
    #[test]
    fn missing_sink_reports_dead_object_without_fake_progress() {
        let t = track(2, 2, 32);
        assert_eq!(t.start_with(|_, _| None), Err(DEAD_OBJECT));
        assert_eq!(t.write(&[0; 4], false), DEAD_OBJECT);
        let sh = t.shared.lock().unwrap();
        assert_eq!((sh.played, sh.timestamp_ns), (0, 0));
    }
    #[test]
    fn blocked_writer_is_released_and_sink_pipe_cannot_trap_the_finalizer() {
        let t = Arc::new(track(2, 2, 32));
        assert_eq!(t.write(&[0; 32], false), 32);
        let writer_track = Arc::clone(&t);
        let (sent, received) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || sent.send(writer_track.write(&[0; 4], true)).unwrap());
        assert!(received.recv_timeout(Duration::from_millis(20)).is_err());
        t.release();
        assert_eq!(received.recv_timeout(Duration::from_secs(1)).unwrap(), DEAD_OBJECT);
        writer.join().unwrap();

        // cat's unread stdout eventually stalls its stdin too. Release must not
        // wait for pipe.write_all or for an audio server to start reading again.
        let t = track(2, 2, 256 * 1024);
        assert_eq!(t.write(&vec![0; 256 * 1024], false), 256 * 1024);
        t.start_with(|_, _| {
            let mut child = std::process::Command::new("/bin/cat")
                .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().unwrap();
            let stdin = child.stdin.take(); Some(crate::Sink { child, stdin })
        }).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert!(t.shared.lock().unwrap().occupied() > 0);
        let start = Instant::now();
        assert!(finish_pump(&t, |_| true));
        assert!(start.elapsed() < Duration::from_secs(1));
    }
    #[test]
    fn pcm_conversion_and_volume_are_real() {
        let out = to_s16(PCM_FLOAT, &[0.0f32, 1.0, -1.0, 2.0, f32::NAN].iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>(), 1, [1.0; 2]);
        let samples: Vec<i16> = out.chunks_exact(2).map(|c| i16::from_le_bytes(c.try_into().unwrap())).collect();
        assert_eq!(samples, [0, 32767, -32767, 32767, 0]);
        let pcm = [1, 0, 255, 127, 0, 128, 4, 0];
        assert_eq!(to_s16(PCM_16, &pcm, 2, [1.0; 2]), pcm);
        assert_eq!(to_s16(PCM_16, &pcm, 2, [0.0; 2]), [0; 8]);
    }
    #[test]
    fn bounds_frame_alignment_source_units_and_in_flight_backpressure() {
        let t = track(4, 2, 32);
        assert_eq!(t.write(&[0; 53], false), 32);
        let mut sh = t.shared.lock().unwrap();
        assert_eq!(sh.queue.len(), 16);
        assert_eq!(sh.written, 4);
        sh.queue.clear(); sh.in_flight = 16; drop(sh);
        assert_eq!(t.write(&[0; 8], false), 0);
        t.release();
        assert_eq!(t.write(&[0; 8], false), DEAD_OBJECT);
        for (length, offset, size) in [(8, -1, 1), (8, 0, -1), (8, 7, 2), (-1, 0, 0), (16, i32::MAX, i32::MAX)] {
            assert_eq!(checked_range(length, offset, size), Err(BAD_VALUE));
        }
        assert_eq!(checked_range(8, 8, 0), Ok(8..8));
    }
    #[test]
    fn pause_flush_stop_release_and_resize_are_bounded() {
        let t = track(2, 2, 32);
        assert_eq!(t.resize(99), 8); assert_eq!(t.resize(-1), BAD_VALUE);
        assert_eq!(t.resize(0), 1); assert_eq!(t.set_threshold(99), 1);
        assert_eq!(t.write(&[0; 32], false), 4);
        t.pause();
        { let mut sh = t.shared.lock().unwrap(); sh.played = 100; }
        t.flush();
        { let sh = t.shared.lock().unwrap(); assert!(sh.queue.is_empty()); assert_eq!((sh.position(), sh.written), (0, 0)); }
        t.stop(); t.release();
        assert_eq!(t.write(&[0; 4], false), DEAD_OBJECT);
    }
    #[test]
    fn safe_handles_never_resolve_foreign_or_released_values() {
        let h = insert(track(2, 2, 32)).unwrap() as i64;
        assert!(get(h).is_some());
        for foreign in [0, -1, 1, 0x7fff_1234_5678, (HANDLE_TAG | HANDLE_SEQUENCE) as i64] { assert!(get(foreign).is_none()); assert!(!remove(foreign)); }
        assert!(remove(h)); assert!(get(h).is_none()); assert!(!remove(h));
    }
    #[test]
    fn sessions_are_positive_preserved_and_never_wrap() {
        assert_eq!(session(12345), Ok(12345)); assert_eq!(session(-1), Err(BAD_VALUE));
        let generated = session(0).unwrap(); assert!(generated > 0); assert_ne!(generated, session(0).unwrap());
        let counter = AtomicI64::new(i32::MAX as i64);
        assert_eq!(allocate_id(&counter), i32::MAX); assert_eq!(allocate_id(&counter), -1);
        let counter = Arc::new(AtomicI64::new(1));
        let jobs: Vec<_> = (0..4).map(|_| { let c = Arc::clone(&counter); std::thread::spawn(move || (0..100).map(|_| allocate_id(&c)).collect::<Vec<_>>()) }).collect();
        let all: std::collections::HashSet<_> = jobs.into_iter().flat_map(|j| j.join().unwrap()).collect();
        assert_eq!(all.len(), 400); assert!(all.iter().all(|id| *id > 0));
    }
    #[test]
    fn preflight_validates_count_not_mask_and_negotiates_output_rate() {
        assert_eq!(min_buffer(44_100, 2, PCM_16), 3528);
        assert_eq!(min_buffer(44_101, 2, PCM_FLOAT) % 8, 0);
        for (rate, count, format) in [(0, 2, PCM_16), (44_100, 12, PCM_16), (44_100, 0, PCM_16), (44_100, 2, 3), (i32::MAX, 2, PCM_16)] { assert_eq!(min_buffer(rate, count, format), BAD_VALUE); }
        assert_ne!(OUTPUT_RATE, 3);
        assert!(Track::new(44_100, 2, PCM_16, 3).is_err());
        assert!(Track::new(44_100, 2, PCM_16, MAX_BUFFER_BYTES as i32 + 4).is_err());
    }
    #[test]
    fn java_channel_masks_are_not_native_bit_masks() {
        for masks in [ChannelMasks { position: 12, index: 0, acn: 0, count: 2 }, ChannelMasks { position: 0, index: 3, acn: 0, count: 2 }] { assert_eq!(masks.channels(), Ok(2)); }
        for masks in [ChannelMasks { position: 3, index: 0, acn: 0, count: 2 }, ChannelMasks { position: 12, index: 7, acn: 0, count: 2 }, ChannelMasks { position: 0, index: 0, acn: 0, count: 0 }, ChannelMasks { position: 12, index: 0, acn: 1, count: 2 }] { assert!(masks.channels().is_err()); }
    }
}
