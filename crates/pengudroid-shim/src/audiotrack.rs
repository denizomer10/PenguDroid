//! `android.media.AudioTrack`, hosted on the host's audio sink.
//!
//! On a device `AudioTrack` is a thin Java object over `libaudioclient`: the
//! native half asks `AudioFlinger` for a track over binder and then hands PCM
//! to a `BitTube` the audio HAL drains. PenguDroid has neither `system_server`
//! nor a HAL, so the class is hosted at the **JNI boundary** instead.
//!
//! `libmedia_jni` publishes its natives with
//! `AndroidRuntime::registerNativeMethods(env, "android/media/AudioTrack", …)`.
//! That call is interposed here (see [`register_native_methods`]); for the two
//! classes below the table is patched in place — the *signatures* are the ones
//! the framework supplied, only the function pointers change, so nothing about
//! `AudioTrack.java` has to be re-declared.
//!
//! PCM then follows the same route as the OpenSL ES buffer queue: a
//! [`crate::spawn_sink`] `pw-cat` (or `aplay`) stream, fed by one thread per
//! track out of a bounded queue. The queue is what gives `write()` its
//! non-blocking, short-write semantics — ExoPlayer's `DefaultAudioSink` writes
//! with `WRITE_NON_BLOCKING` and depends on a short write meaning "come back
//! later" — and it is what makes `getPlaybackHeadPosition` advance at the rate
//! the sink actually consumes.
//!
//! **Scope.** Everything here is playback only, and deliberately so: a track is
//! a queue plus a sink, there is no mixing, no effects, no volume scaling and no
//! routing. `AudioFlinger` itself stays a stub; this replaces the *client* side.

use std::collections::VecDeque;
use std::ffi::CStr;
use std::os::raw::c_char;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::dlsym_next;

// ---------------------------------------------------------------------------
// JNI plumbing
// ---------------------------------------------------------------------------
//
// Slot numbers inside `JNINativeInterface` (see the JNI specification). Only the
// handful this module needs are listed, each next to the reason it is needed.

const SLOT_FIND_CLASS: usize = 6;
const SLOT_GET_OBJECT_CLASS: usize = 31;
const SLOT_GET_FIELD_ID: usize = 94;
const SLOT_GET_LONG_FIELD: usize = 101;
const SLOT_SET_LONG_FIELD: usize = 110;
const SLOT_GET_ARRAY_LENGTH: usize = 171;
const SLOT_GET_BYTE_ARRAY_REGION: usize = 200;
const SLOT_GET_SHORT_ARRAY_REGION: usize = 202;
const SLOT_GET_INT_ARRAY_REGION: usize = 203;
const SLOT_SET_INT_ARRAY_REGION: usize = 211;
const SLOT_SET_LONG_ARRAY_REGION: usize = 212;
const SLOT_REGISTER_NATIVES: usize = 215;
const SLOT_GET_DIRECT_BUFFER_ADDRESS: usize = 230;

/// A `JNIEnv*` as the JNI expects it: a pointer to a table of function pointers.
pub(crate) unsafe fn jni_slot(env: *mut std::os::raw::c_void, slot: usize) -> *const std::os::raw::c_void {
    let table = *(env as *const *const *const std::os::raw::c_void);
    *table.add(slot)
}

pub(crate) unsafe fn jni_find_class(env: *mut std::os::raw::c_void, name: &CStr) -> *mut std::os::raw::c_void {
    let f: unsafe extern "C" fn(
        *mut std::os::raw::c_void,
        *const c_char,
    ) -> *mut std::os::raw::c_void = std::mem::transmute(jni_slot(env, SLOT_FIND_CLASS));
    f(env, name.as_ptr())
}

unsafe fn jni_get_object_class(
    env: *mut std::os::raw::c_void,
    obj: *mut std::os::raw::c_void,
) -> *mut std::os::raw::c_void {
    let f: unsafe extern "C" fn(
        *mut std::os::raw::c_void,
        *mut std::os::raw::c_void,
    ) -> *mut std::os::raw::c_void = std::mem::transmute(jni_slot(env, SLOT_GET_OBJECT_CLASS));
    f(env, obj)
}

unsafe fn jni_get_field_id(
    env: *mut std::os::raw::c_void,
    class: *mut std::os::raw::c_void,
    name: &CStr,
    signature: &CStr,
) -> *mut std::os::raw::c_void {
    let f: unsafe extern "C" fn(
        *mut std::os::raw::c_void,
        *mut std::os::raw::c_void,
        *const c_char,
        *const c_char,
    ) -> *mut std::os::raw::c_void = std::mem::transmute(jni_slot(env, SLOT_GET_FIELD_ID));
    f(env, class, name.as_ptr(), signature.as_ptr())
}

unsafe fn jni_get_long_field(
    env: *mut std::os::raw::c_void,
    obj: *mut std::os::raw::c_void,
    field: *mut std::os::raw::c_void,
) -> i64 {
    let f: unsafe extern "C" fn(
        *mut std::os::raw::c_void,
        *mut std::os::raw::c_void,
        *mut std::os::raw::c_void,
    ) -> i64 = std::mem::transmute(jni_slot(env, SLOT_GET_LONG_FIELD));
    f(env, obj, field)
}

unsafe fn jni_set_long_field(
    env: *mut std::os::raw::c_void,
    obj: *mut std::os::raw::c_void,
    field: *mut std::os::raw::c_void,
    value: i64,
) {
    let f: unsafe extern "C" fn(
        *mut std::os::raw::c_void,
        *mut std::os::raw::c_void,
        *mut std::os::raw::c_void,
        i64,
    ) = std::mem::transmute(jni_slot(env, SLOT_SET_LONG_FIELD));
    f(env, obj, field, value)
}

unsafe fn jni_array_length(env: *mut std::os::raw::c_void, array: *mut std::os::raw::c_void) -> i32 {
    let f: unsafe extern "C" fn(
        *mut std::os::raw::c_void,
        *mut std::os::raw::c_void,
    ) -> i32 = std::mem::transmute(jni_slot(env, SLOT_GET_ARRAY_LENGTH));
    f(env, array)
}

unsafe fn jni_get_int_array(
    env: *mut std::os::raw::c_void,
    array: *mut std::os::raw::c_void,
    start: i32,
    out: &mut [i32],
) {
    let f: unsafe extern "C" fn(
        *mut std::os::raw::c_void,
        *mut std::os::raw::c_void,
        i32,
        i32,
        *mut i32,
    ) = std::mem::transmute(jni_slot(env, SLOT_GET_INT_ARRAY_REGION));
    f(env, array, start, out.len() as i32, out.as_mut_ptr());
}

unsafe fn jni_set_int_array(
    env: *mut std::os::raw::c_void,
    array: *mut std::os::raw::c_void,
    start: i32,
    values: &[i32],
) {
    let f: unsafe extern "C" fn(
        *mut std::os::raw::c_void,
        *mut std::os::raw::c_void,
        i32,
        i32,
        *const i32,
    ) = std::mem::transmute(jni_slot(env, SLOT_SET_INT_ARRAY_REGION));
    f(env, array, start, values.len() as i32, values.as_ptr());
}

unsafe fn jni_set_long_array(
    env: *mut std::os::raw::c_void,
    array: *mut std::os::raw::c_void,
    start: i32,
    values: &[i64],
) {
    let f: unsafe extern "C" fn(
        *mut std::os::raw::c_void,
        *mut std::os::raw::c_void,
        i32,
        i32,
        *const i64,
    ) = std::mem::transmute(jni_slot(env, SLOT_SET_LONG_ARRAY_REGION));
    f(env, array, start, values.len() as i32, values.as_ptr());
}

/// Copy a primitive array out of the Java heap.
///
/// The element size differs per call site, so the caller passes the region
/// reader (byte/short) and the destination buffer.
unsafe fn jni_get_byte_array(
    env: *mut std::os::raw::c_void,
    array: *mut std::os::raw::c_void,
    start: i32,
    out: &mut [u8],
) {
    let f: unsafe extern "C" fn(
        *mut std::os::raw::c_void,
        *mut std::os::raw::c_void,
        i32,
        i32,
        *mut i8,
    ) = std::mem::transmute(jni_slot(env, SLOT_GET_BYTE_ARRAY_REGION));
    f(env, array, start, out.len() as i32, out.as_mut_ptr() as *mut i8);
}

unsafe fn jni_get_short_array(
    env: *mut std::os::raw::c_void,
    array: *mut std::os::raw::c_void,
    start: i32,
    out: &mut [i16],
) {
    let f: unsafe extern "C" fn(
        *mut std::os::raw::c_void,
        *mut std::os::raw::c_void,
        i32,
        i32,
        *mut i16,
    ) = std::mem::transmute(jni_slot(env, SLOT_GET_SHORT_ARRAY_REGION));
    f(env, array, start, out.len() as i32, out.as_mut_ptr());
}

unsafe fn jni_direct_buffer(
    env: *mut std::os::raw::c_void,
    buffer: *mut std::os::raw::c_void,
) -> *mut u8 {
    let f: unsafe extern "C" fn(
        *mut std::os::raw::c_void,
        *mut std::os::raw::c_void,
    ) -> *mut std::os::raw::c_void = std::mem::transmute(jni_slot(env, SLOT_GET_DIRECT_BUFFER_ADDRESS));
    f(env, buffer) as *mut u8
}

// ---------------------------------------------------------------------------
// RegisterNatives hook
// ---------------------------------------------------------------------------

/// One entry of `JNINativeMethod`.
#[repr(C)]
pub(crate) struct NativeMethod {
    pub(crate) name: *const c_char,
    pub(crate) signature: *const c_char,
    pub(crate) fn_ptr: *mut std::os::raw::c_void,
}

/// The `class → method → implementation` table this module overrides.
fn override_for(class: &str, method: &str) -> Option<*mut std::os::raw::c_void> {
    let f: *mut std::os::raw::c_void = match (class, method) {
        ("android/media/AudioSystem", "newAudioSessionId") => new_session_id as *mut _,
        ("android/media/AudioSystem", "newAudioPlayerId") => new_session_id as *mut _,
        ("android/media/AudioTrack", "native_setup") => native_setup as *mut _,
        ("android/media/AudioTrack", "native_finalize") => native_release as *mut _,
        ("android/media/AudioTrack", "native_release") => native_release as *mut _,
        ("android/media/AudioTrack", "native_start") => native_start as *mut _,
        ("android/media/AudioTrack", "native_stop") => native_stop as *mut _,
        ("android/media/AudioTrack", "native_pause") => native_pause as *mut _,
        ("android/media/AudioTrack", "native_flush") => native_flush as *mut _,
        ("android/media/AudioTrack", "native_write_byte") => native_write_byte as *mut _,
        ("android/media/AudioTrack", "native_write_short") => native_write_short as *mut _,
        ("android/media/AudioTrack", "native_write_float") => native_write_float as *mut _,
        ("android/media/AudioTrack", "native_write_native_bytes") => {
            native_write_native_bytes as *mut _
        }
        ("android/media/AudioTrack", "native_get_position") => native_get_position as *mut _,
        ("android/media/AudioTrack", "native_get_timestamp") => native_get_timestamp as *mut _,
        ("android/media/AudioTrack", "native_get_latency") => native_get_latency as *mut _,
        ("android/media/AudioTrack", "native_get_underrun_count") => native_zero as *mut _,
        ("android/media/AudioTrack", "native_get_flags") => native_zero as *mut _,
        ("android/media/AudioTrack", "native_setVolume") => native_noop as *mut _,
        ("android/media/AudioTrack", "native_get_buffer_size_frames") => {
            native_buffer_size_frames as *mut _
        }
        ("android/media/AudioTrack", "native_get_buffer_capacity_frames") => {
            native_buffer_capacity_frames as *mut _
        }
        ("android/media/AudioTrack", "native_get_min_buff_size") => native_min_buff_size as *mut _,
        ("android/media/AudioTrack", "native_get_output_sample_rate") => {
            native_output_sample_rate as *mut _
        }
        ("android/media/AudioTrack", "native_setPlaybackParams") => native_noop as *mut _,
        ("android/media/AudioTrack", "native_getPlaybackParams") => native_null as *mut _,
        ("android/media/AudioTrack", "native_setLogSessionId") => native_noop as *mut _,
        ("android/media/AudioTrack", "native_setPlayerIId") => native_noop as *mut _,
        ("android/media/AudioTrack", "native_set_delay_padding") => native_noop as *mut _,
        ("android/media/AudioTrack", "native_set_position") => native_zero as *mut _,
        ("android/media/AudioTrack", "native_set_marker_pos") => native_zero as *mut _,
        ("android/media/AudioTrack", "native_get_marker_pos") => native_zero as *mut _,
        ("android/media/AudioTrack", "native_set_pos_update_period") => native_zero as *mut _,
        ("android/media/AudioTrack", "native_get_pos_update_period") => native_zero as *mut _,
        ("android/media/AudioTrack", "native_set_loop") => native_zero as *mut _,
        ("android/media/AudioTrack", "native_reload_static") => native_zero as *mut _,
        ("android/media/AudioTrack", "native_set_playback_rate") => native_zero as *mut _,
        ("android/media/AudioTrack", "native_get_playback_rate") => native_zero as *mut _,
        ("android/media/AudioTrack", "native_getRoutedDeviceId") => native_zero as *mut _,
        ("android/media/AudioTrack", "native_enableDeviceCallback") => native_noop as *mut _,
        ("android/media/AudioTrack", "native_disableDeviceCallback") => native_noop as *mut _,
        ("android/media/AudioTrack", "native_setStartThresholdInFrames") => native_set_start_threshold as *mut _,
        ("android/media/AudioTrack", "native_getStartThresholdInFrames") => {
            native_get_start_threshold as *mut _
        }
        ("android/media/AudioTrack", "native_set_audio_description_mix_level_db") => {
            native_zero as *mut _
        }
        ("android/media/AudioTrack", "native_attachAuxEffect") => native_zero as *mut _,
        ("android/media/AudioTrack", "native_setAuxEffectSendLevel") => native_zero as *mut _,
        ("android/media/AudioTrack", "native_setOutputDevice") => native_zero as *mut _,
        ("android/media/AudioTrack", "native_is_direct_output_supported") => native_zero as *mut _,
        ("android/media/AudioTrack", "native_getMetrics") => native_null as *mut _,
        ("android/media/AudioTrack", "native_applyVolumeShaper") => native_zero as *mut _,
        ("android/media/AudioTrack", "native_getVolumeShaperState") => native_null as *mut _,
        ("android/media/AudioTrack", "native_get_audio_description_mix_level_db") => {
            native_zero as *mut _
        }
        ("android/media/AudioTrack", "native_set_dual_mono_mode") => native_zero as *mut _,
        ("android/media/AudioTrack", "native_get_dual_mono_mode") => native_zero as *mut _,
        ("android/media/AudioTrack", "native_getPortId") => native_zero as *mut _,
        ("android/media/AudioTrack", "native_setPresentation") => native_zero as *mut _,
        _ => return crate::blast::override_for(class, method),
    };
    Some(f)
}

/// `AndroidRuntime::registerNativeMethods(JNIEnv*, const char*, const JNINativeMethod*, int)`
///
/// Every JNI library in this tree registers its natives through it, so this is
/// the seam where a class can be re-hosted without touching its Java side. The
/// table is copied before patching: `libmedia_jni` keeps its own copy in
/// `.data.rel.ro` and may register the same table again.
#[no_mangle]
pub unsafe extern "C" fn _ZN7android14AndroidRuntime21registerNativeMethodsEP7_JNIEnvPKcPK15JNINativeMethodi(
    env: *mut std::os::raw::c_void,
    class_name: *const c_char,
    methods: *const NativeMethod,
    count: i32,
) -> i32 {
    if env.is_null() || class_name.is_null() {
        return -1;
    }
    let name = CStr::from_ptr(class_name).to_string_lossy().into_owned();
    let should_patch = name == "android/media/AudioTrack"
        || name == "android/media/AudioSystem"
        || crate::blast::hosts(&name);
    if !should_patch || methods.is_null() || count <= 0 {
        let Some(real) = dlsym_next::<unsafe extern "C" fn(
            *mut std::os::raw::c_void,
            *const c_char,
            *const NativeMethod,
            i32,
        ) -> i32>(b"_ZN7android14AndroidRuntime21registerNativeMethodsEP7_JNIEnvPKcPK15JNINativeMethodi\0")
        else {
            return -1;
        };
        return real(env, class_name, methods, count);
    }

    let original = std::slice::from_raw_parts(methods, count as usize);
    let mut patched: Vec<NativeMethod> = Vec::with_capacity(original.len());
    let mut replaced = 0usize;
    for m in original {
        let method = if m.name.is_null() {
            String::new()
        } else {
            CStr::from_ptr(m.name).to_string_lossy().into_owned()
        };
        let fn_ptr = match override_for(&name, &method) {
            Some(f) => {
                replaced += 1;
                f
            }
            None => m.fn_ptr,
        };
        patched.push(NativeMethod {
            name: m.name,
            signature: m.signature,
            fn_ptr,
        });
    }

    let class = jni_find_class(env, &std::ffi::CString::new(name.clone()).unwrap_or_default());
    if class.is_null() {
        eprintln!("audio: registerNativeMethods: class {name} not found");
        return -1;
    }
    let register: unsafe extern "C" fn(
        *mut std::os::raw::c_void,
        *mut std::os::raw::c_void,
        *const NativeMethod,
        i32,
    ) -> i32 = std::mem::transmute(jni_slot(env, SLOT_REGISTER_NATIVES));
    let rc = register(env, class, patched.as_ptr(), patched.len() as i32);
    eprintln!("audio: hosted {replaced} native(s) of {name} (rc={rc})");
    rc
}

// ---------------------------------------------------------------------------
// Tracks
// ---------------------------------------------------------------------------

const ENCODING_PCM_16BIT: i32 = 2;
const ENCODING_PCM_FLOAT: i32 = 4;

/// Frames a track asks the sink for, when the app does not say.
const DEFAULT_FRAMES: u32 = 4096;
/// Bytes handed to the sink per write.
const SINK_CHUNK: usize = 2048;

static NEXT_SESSION: AtomicI32 = AtomicI32::new(1);
static NEXT_TRACK: AtomicU64 = AtomicU64::new(1);

/// State shared with the thread that feeds the sink.
struct Shared {
    queue: VecDeque<u8>,
    /// Frames already written into the sink (the playback head).
    played: u64,
    /// Playback head at the last flush: `played - base` is what Java sees.
    base: u64,
    playing: bool,
    alive: bool,
}

impl Shared {
    fn queue_frames(&self, bytes_per_frame: u32) -> u64 {
        (self.queue.len() as u64) / (bytes_per_frame.max(1) as u64)
    }
}

struct Track {
    rate: u32,
    channels: u32,
    /// Bytes per sample *in the encoding the app writes*.
    bytes_per_sample: u32,
    capacity_bytes: usize,
    start_threshold_frames: u32,
    started: Instant,
    shared: Arc<Mutex<Shared>>,
    /// The thread that feeds `pw-cat`; it owns the sink so that dropping it
    /// closes the stream (EOF) and reaps the child.
    pump: Option<std::thread::JoinHandle<()>>,
}

impl Track {
    fn bytes_per_frame(&self) -> u32 {
        self.bytes_per_sample * self.channels.max(1)
    }

    /// Bytes of *sink* PCM (`s16`) that fit in the track's buffer.
    fn queue_capacity(&self) -> usize {
        let frames = (self.capacity_bytes / (self.bytes_per_frame() as usize)).max(1);
        frames * (self.channels.max(1) as usize) * 2
    }

    fn position_frames(&self) -> u64 {
        let sh = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        sh.played.saturating_sub(sh.base)
    }
}

fn tracks() -> &'static Mutex<std::collections::HashMap<u64, Track>> {
    static TRACKS: std::sync::OnceLock<Mutex<std::collections::HashMap<u64, Track>>> =
        std::sync::OnceLock::new();
    TRACKS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn channel_count(channel_mask: i32, channel_index_mask: i32) -> u32 {
    let mask = if channel_index_mask != 0 {
        channel_index_mask
    } else {
        channel_mask
    };
    let n = mask.count_ones();
    if n == 0 {
        2
    } else {
        n
    }
}

fn new_session_id(
    _env: *mut std::os::raw::c_void,
    _class: *mut std::os::raw::c_void,
) -> i32 {
    let id = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    // Stay inside the positive range Java accepts and wrap before it overflows.
    if id <= 0 || id >= 0x7fff_ff00 {
        NEXT_SESSION.store(2, Ordering::Relaxed);
        1
    } else {
        id
    }
}

/// Resolve the `AudioTrack` this call belongs to.
unsafe fn track_for(
    env: *mut std::os::raw::c_void,
    thiz: *mut std::os::raw::c_void,
) -> Option<u64> {
    if thiz.is_null() {
        return None;
    }
    let class = jni_get_object_class(env, thiz);
    if class.is_null() {
        return None;
    }
    let field = jni_get_field_id(
        env,
        class,
        &std::ffi::CString::new("mNativeTrackInJavaObj").unwrap_or_default(),
        &std::ffi::CString::new("J").unwrap_or_default(),
    );
    if field.is_null() {
        return None;
    }
    let handle = jni_get_long_field(env, thiz, field);
    if handle <= 0 {
        None
    } else {
        Some(handle as u64)
    }
}

/// Convert whatever the app wrote into the sink's `s16` PCM.
fn to_s16(encoding: i32, bytes: &[u8]) -> Vec<u8> {
    match encoding {
        ENCODING_PCM_FLOAT => {
            let mut out = Vec::with_capacity(bytes.len() / 2);
            for chunk in bytes.chunks_exact(4) {
                let v = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                let clamped = v.clamp(-1.0, 1.0);
                let s = (clamped * 32767.0).round() as i32;
                out.extend_from_slice(&(s as i16).to_le_bytes());
            }
            out
        }
        // AudioTrack also supports 8-bit PCM, but nothing in this tree writes
        // it; treating it as 16-bit keeps the queue consistent.
        _ => bytes.to_vec(),
    }
}

/// Append PCM to the track's queue and return how many **source** bytes fit.
fn push_pcm(track: &Track, bytes: &[u8]) -> i32 {
    let src_frame = track.bytes_per_frame() as usize;
    if src_frame == 0 || bytes.is_empty() { return 0; }
    let dst_frame = track.channels.max(1) as usize * 2;
    let capacity = track.queue_capacity();
    let mut sh = track.shared.lock().unwrap_or_else(|e| e.into_inner());
    // A zero short-write means "retry later". Once the sink has died that
    // would leave ExoPlayer retrying a permanently full queue forever.
    if !sh.alive { return -6; } // AudioTrack.ERROR_DEAD_OBJECT
    let fit_frames = (capacity.saturating_sub(sh.queue.len()) / dst_frame)
        .min(bytes.len() / src_frame);
    if fit_frames == 0 { return 0; }
    let consumed = fit_frames * src_frame;
    let converted = to_s16(if track.bytes_per_sample == 4 { ENCODING_PCM_FLOAT } else { ENCODING_PCM_16BIT }, &bytes[..consumed]);
    sh.queue.extend(converted);
    consumed as i32
}

/// How many times a dead audio sink is respawned before the track is declared
/// dead. Bounded so a permanently broken audio server cannot spin.
const MAX_SINK_RESTARTS: u32 = 5;

/// Start the thread that drains the queue into `pw-cat`.
fn start_pump(track: &mut Track) {
    if track.pump.is_some() {
        return;
    }
    let Some(sink) = crate::spawn_sink(track.rate, track.channels) else {
        eprintln!("audio: no sink (pw-cat/aplay) for AudioTrack");
        return;
    };
    let shared = Arc::clone(&track.shared);
    let bytes_per_frame = (track.channels.max(1) as u32) * 2;
    let (rate, channels) = (track.rate, track.channels);
    track.pump = Some(std::thread::spawn(move || {
        let mut sink = sink;
        let mut stdin = sink.stdin.take();
        // A sink can die for reasons that have nothing to do with the app: the
        // PipeWire daemon restarts, the default device changes, a USB headset is
        // unplugged. Treating that as fatal stalled ExoPlayer forever, so the
        // sink is respawned a bounded number of times instead.
        let mut restarts: u32 = 0;
        loop {
            let chunk = {
                let mut sh = shared.lock().unwrap_or_else(|e| e.into_inner());
                if !sh.alive {
                    break;
                }
                if !sh.playing || sh.queue.is_empty() {
                    drop(sh);
                    std::thread::sleep(std::time::Duration::from_millis(3));
                    continue;
                }
                let n = sh.queue.len().min(SINK_CHUNK);
                let mut bytes = Vec::with_capacity(n);
                for _ in 0..n {
                    bytes.push(sh.queue.pop_front().unwrap_or(0));
                }
                bytes
            };
            let wrote = match stdin.as_mut() {
                Some(pipe) => {
                    use std::io::Write as _;
                    match pipe.write_all(&chunk) {
                        Ok(()) => true,
                        Err(e) => {
                            eprintln!("audio: AudioTrack output failed: {e}");
                            false
                        }
                    }
                }
                None => false,
            };
            if !wrote {
                if restarts >= MAX_SINK_RESTARTS {
                    let mut sh = shared.lock().unwrap_or_else(|e| e.into_inner());
                    sh.alive = false;
                    break;
                }
                restarts += 1;
                // Put the chunk back: it was popped before the write, and
                // dropping it would punch a hole in the audio at every restart.
                {
                    let mut sh = shared.lock().unwrap_or_else(|e| e.into_inner());
                    for byte in chunk.iter().rev() {
                        sh.queue.push_front(*byte);
                    }
                }
                // Give the audio server a moment to come back before retrying;
                // a device change is not instantaneous.
                std::thread::sleep(std::time::Duration::from_millis(150));
                match crate::spawn_sink(rate, channels) {
                    Some(mut fresh) => {
                        stdin = fresh.stdin.take();
                        sink = fresh;
                        eprintln!(
                            "audio: AudioTrack sink restarted (attempt {restarts}/{MAX_SINK_RESTARTS})"
                        );
                        continue;
                    }
                    None => {
                        let mut sh = shared.lock().unwrap_or_else(|e| e.into_inner());
                        sh.alive = false;
                        break;
                    }
                }
            }
            let mut sh = shared.lock().unwrap_or_else(|e| e.into_inner());
            sh.played += (chunk.len() / bytes_per_frame as usize) as u64;
        }
    }));
}

/// Stop the pump and forget the track.
unsafe fn release_track(handle: u64) {
    let track = tracks()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&handle);
    if let Some(track) = track {
        {
            let mut sh = track.shared.lock().unwrap_or_else(|e| e.into_inner());
            sh.alive = false;
            sh.playing = false;
        }
        // Not joined: the pump owns the sink and reaps it when the pending write
        // returns. Joining here could block the Java thread on a stalled sink.
        drop(track.pump);
    }
}

// ---------------------------------------------------------------------------
// Natives
// ---------------------------------------------------------------------------

/// `native_setup(...)`: build the track and hand its handle back through the
/// `sessionId` array and the Java field, exactly like `libmedia_jni` does.
unsafe extern "C" fn native_setup(
    env: *mut std::os::raw::c_void,
    thiz: *mut std::os::raw::c_void,
    _this_ref: *mut std::os::raw::c_void,
    _attributes: *mut std::os::raw::c_void,
    sample_rate: *mut std::os::raw::c_void,
    channel_mask: i32,
    channel_index_mask: i32,
    encoding: i32,
    buffer_bytes: i32,
    _mode: i32,
    session: *mut std::os::raw::c_void,
    _native_track: i64,
    _offload: u8,
    _encapsulation: i32,
    _tuner: *mut std::os::raw::c_void,
    _op_package: *mut std::os::raw::c_void,
) -> i32 {
    let mut rate = 0i32;
    if !sample_rate.is_null() && jni_array_length(env, sample_rate) >= 1 {
        jni_get_int_array(env, sample_rate, 0, std::slice::from_mut(&mut rate));
    }
    if rate <= 0 {
        rate = 44_100;
    }
    let channels = channel_count(channel_mask, channel_index_mask);
    let bytes_per_sample = if encoding == ENCODING_PCM_FLOAT { 4 } else { 2 };
    let requested = if buffer_bytes > 0 { buffer_bytes as usize } else { 0 };
    let frame_bytes = bytes_per_sample as usize * channels as usize;
    let capacity_bytes = if requested >= frame_bytes {
        requested
    } else {
        DEFAULT_FRAMES as usize * frame_bytes
    };

    let session_id = new_session_id(env, thiz);
    if !session.is_null() && jni_array_length(env, session) >= 1 {
        jni_set_int_array(env, session, 0, &[session_id]);
    }

    let handle = NEXT_TRACK.fetch_add(1, Ordering::Relaxed);
    let capacity_frames = (capacity_bytes / frame_bytes).max(1) as u32;
    let track = Track {
        rate: rate as u32,
        channels,
        bytes_per_sample,
        capacity_bytes,
        start_threshold_frames: capacity_frames / 4,
        started: Instant::now(),
        shared: Arc::new(Mutex::new(Shared {
            queue: VecDeque::new(),
            played: 0,
            base: 0,
            playing: false,
            alive: true,
        })),
        pump: None,
    };
    tracks()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(handle, track);

    let class = jni_get_object_class(env, thiz);
    let field = jni_get_field_id(
        env,
        class,
        &std::ffi::CString::new("mNativeTrackInJavaObj").unwrap_or_default(),
        &std::ffi::CString::new("J").unwrap_or_default(),
    );
    if !field.is_null() {
        jni_set_long_field(env, thiz, field, handle as i64);
    }
    0 // SUCCESS
}

unsafe extern "C" fn native_release(env: *mut std::os::raw::c_void, thiz: *mut std::os::raw::c_void) {
    if let Some(handle) = track_for(env, thiz) {
        release_track(handle);
        let class = jni_get_object_class(env, thiz);
        let field = jni_get_field_id(
            env,
            class,
            &std::ffi::CString::new("mNativeTrackInJavaObj").unwrap_or_default(),
            &std::ffi::CString::new("J").unwrap_or_default(),
        );
        if !field.is_null() {
            jni_set_long_field(env, thiz, field, 0);
        }
    }
}

unsafe extern "C" fn native_start(env: *mut std::os::raw::c_void, thiz: *mut std::os::raw::c_void) {
    let Some(handle) = track_for(env, thiz) else {
        return;
    };
    let mut all = tracks().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(track) = all.get_mut(&handle) {
        start_pump(track);
        let mut sh = track.shared.lock().unwrap_or_else(|e| e.into_inner());
        sh.playing = true;
    }
}

unsafe extern "C" fn native_pause(env: *mut std::os::raw::c_void, thiz: *mut std::os::raw::c_void) {
    if let Some(handle) = track_for(env, thiz) {
        if let Some(track) = tracks().lock().unwrap_or_else(|e| e.into_inner()).get(&handle) {
            track
                .shared
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .playing = false;
        }
    }
}

unsafe extern "C" fn native_stop(env: *mut std::os::raw::c_void, thiz: *mut std::os::raw::c_void) {
    native_pause(env, thiz);
}

unsafe extern "C" fn native_flush(env: *mut std::os::raw::c_void, thiz: *mut std::os::raw::c_void) {
    if let Some(handle) = track_for(env, thiz) {
        if let Some(track) = tracks().lock().unwrap_or_else(|e| e.into_inner()).get(&handle) {
            let mut sh = track.shared.lock().unwrap_or_else(|e| e.into_inner());
            sh.queue.clear();
            sh.base = sh.played;
        }
    }
}

/// Shared body of the three `write` variants.
unsafe fn write_bytes(
    env: *mut std::os::raw::c_void,
    thiz: *mut std::os::raw::c_void,
    data: &[u8],
) -> i32 {
    let Some(handle) = track_for(env, thiz) else {
        return -1;
    };
    let all = tracks().lock().unwrap_or_else(|e| e.into_inner());
    let Some(track) = all.get(&handle) else {
        return -1;
    };
    push_pcm(track, data)
}

unsafe extern "C" fn native_write_byte(
    env: *mut std::os::raw::c_void,
    thiz: *mut std::os::raw::c_void,
    array: *mut std::os::raw::c_void,
    offset: i32,
    size: i32,
    _mode: i32,
    _blocking: u8,
) -> i32 {
    if array.is_null() || size <= 0 {
        return 0;
    }
    let mut buffer = vec![0u8; size as usize];
    jni_get_byte_array(env, array, offset, &mut buffer);
    write_bytes(env, thiz, &buffer)
}

unsafe extern "C" fn native_write_short(
    env: *mut std::os::raw::c_void,
    thiz: *mut std::os::raw::c_void,
    array: *mut std::os::raw::c_void,
    offset: i32,
    size: i32,
    _mode: i32,
    _blocking: u8,
) -> i32 {
    if array.is_null() || size <= 0 {
        return 0;
    }
    // `native_write_short(short[] audioData, int offsetInShorts, int sizeInShorts, …)`
    // — both the offset and the size count *shorts*, not bytes.
    let mut buffer = vec![0i16; size as usize];
    jni_get_short_array(env, array, offset, &mut buffer);
    let mut bytes = Vec::with_capacity(buffer.len() * 2);
    for s in &buffer {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    write_bytes(env, thiz, &bytes)
}

unsafe extern "C" fn native_write_float(
    _env: *mut std::os::raw::c_void,
    _thiz: *mut std::os::raw::c_void,
    _array: *mut std::os::raw::c_void,
    _offset: i32,
    _size: i32,
    _mode: i32,
    _blocking: u8,
) -> i32 {
    // AudioTrack converts float PCM to the track's encoding before the write
    // reaches the native side only for 16-bit tracks; a float track goes
    // through `native_write_float`. Nothing in this tree does, and reading a
    // `jfloatArray` would need another region call, so accept-and-drop instead
    // of pretending to have consumed it.
    0
}

unsafe extern "C" fn native_write_native_bytes(
    env: *mut std::os::raw::c_void,
    thiz: *mut std::os::raw::c_void,
    buffer: *mut std::os::raw::c_void,
    position: i32,
    size: i32,
    _mode: i32,
    _blocking: u8,
) -> i32 {
    if buffer.is_null() || size <= 0 {
        return 0;
    }
    let base = jni_direct_buffer(env, buffer);
    if base.is_null() {
        return 0;
    }
    let data = std::slice::from_raw_parts(base.add(position.max(0) as usize), size as usize);
    write_bytes(env, thiz, data)
}

unsafe extern "C" fn native_get_position(
    env: *mut std::os::raw::c_void,
    thiz: *mut std::os::raw::c_void,
) -> i32 {
    let Some(handle) = track_for(env, thiz) else {
        return 0;
    };
    let all = tracks().lock().unwrap_or_else(|e| e.into_inner());
    match all.get(&handle) {
        Some(track) => track.position_frames() as i32,
        None => 0,
    }
}

unsafe extern "C" fn native_get_timestamp(
    env: *mut std::os::raw::c_void,
    thiz: *mut std::os::raw::c_void,
    out: *mut std::os::raw::c_void,
) -> i32 {
    if out.is_null() || jni_array_length(env, out) < 2 {
        return -1;
    }
    let Some(handle) = track_for(env, thiz) else {
        return -1;
    };
    let all = tracks().lock().unwrap_or_else(|e| e.into_inner());
    let Some(track) = all.get(&handle) else {
        return -1;
    };
    let frames = track.position_frames();
    let nanos = track.started.elapsed().as_nanos() as i64;
    jni_set_long_array(env, out, 0, &[frames as i64, nanos]);
    0 // SUCCESS
}

unsafe extern "C" fn native_get_latency(
    env: *mut std::os::raw::c_void,
    thiz: *mut std::os::raw::c_void,
) -> i32 {
    let Some(handle) = track_for(env, thiz) else {
        return 0;
    };
    let all = tracks().lock().unwrap_or_else(|e| e.into_inner());
    match all.get(&handle) {
        // `AudioTrack.getLatency()` is documented in **milliseconds**, and
        // ExoPlayer multiplies it by 1000 to get microseconds — returning a
        // frame count here made it warn "Ignoring impossibly large audio
        // latency" and skew its position estimate. One buffer's worth of frames
        // is the honest latency of a pipe sink.
        Some(track) => {
            let frames = track.capacity_bytes / track.bytes_per_frame().max(1) as usize;
            ((frames as u64 * 1000) / track.rate.max(1) as u64) as i32
        }
        None => 0,
    }
}

unsafe extern "C" fn native_buffer_size_frames(
    env: *mut std::os::raw::c_void,
    thiz: *mut std::os::raw::c_void,
) -> i32 {
    native_buffer_capacity_frames(env, thiz)
}

unsafe extern "C" fn native_buffer_capacity_frames(
    env: *mut std::os::raw::c_void,
    thiz: *mut std::os::raw::c_void,
) -> i32 {
    let Some(handle) = track_for(env, thiz) else {
        return DEFAULT_FRAMES as i32;
    };
    let all = tracks().lock().unwrap_or_else(|e| e.into_inner());
    match all.get(&handle) {
        Some(track) => {
            (track.capacity_bytes / track.bytes_per_frame().max(1) as usize).max(1) as i32
        }
        None => DEFAULT_FRAMES as i32,
    }
}

unsafe extern "C" fn native_min_buff_size(
    _env: *mut std::os::raw::c_void,
    _class: *mut std::os::raw::c_void,
    sample_rate: i32,
    channel_count: i32,
    audio_format: i32,
) -> i32 {
    let rate = if sample_rate > 0 { sample_rate } else { 44_100 };
    let channels = if channel_count > 0 { channel_count } else { 2 };
    let bytes = if audio_format == ENCODING_PCM_FLOAT { 4 } else { 2 };
    // 20 ms, rounded up to a frame.
    let frames = (rate / 50).max(1);
    frames * channels * bytes
}

unsafe extern "C" fn native_output_sample_rate(
    _env: *mut std::os::raw::c_void,
    _class: *mut std::os::raw::c_void,
    sample_rate: i32,
) -> i32 {
    if sample_rate > 0 {
        sample_rate
    } else {
        48_000
    }
}

unsafe extern "C" fn native_set_start_threshold(
    _env: *mut std::os::raw::c_void,
    _thiz: *mut std::os::raw::c_void,
    frames: i32,
) -> i32 {
    frames
}

unsafe extern "C" fn native_get_start_threshold(
    env: *mut std::os::raw::c_void,
    thiz: *mut std::os::raw::c_void,
) -> i32 {
    let Some(handle) = track_for(env, thiz) else {
        return 0;
    };
    let all = tracks().lock().unwrap_or_else(|e| e.into_inner());
    all.get(&handle)
        .map(|t| t.start_threshold_frames as i32)
        .unwrap_or(0)
}

unsafe extern "C" fn native_zero(
    _env: *mut std::os::raw::c_void,
    _thiz: *mut std::os::raw::c_void,
) -> i32 {
    0
}

unsafe extern "C" fn native_noop(_env: *mut std::os::raw::c_void, _thiz: *mut std::os::raw::c_void) {}

unsafe extern "C" fn native_null(
    _env: *mut std::os::raw::c_void,
    _thiz: *mut std::os::raw::c_void,
) -> *mut std::os::raw::c_void {
    std::ptr::null_mut()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(encoding_bytes: u32, channels: u32, capacity: usize) -> Track {
        Track {
            rate: 48_000,
            channels,
            bytes_per_sample: encoding_bytes,
            capacity_bytes: capacity,
            start_threshold_frames: 0,
            started: Instant::now(),
            shared: Arc::new(Mutex::new(Shared {
                queue: VecDeque::new(),
                played: 0,
                base: 0,
                playing: true,
                alive: true,
            })),
            pump: None,
        }
    }

    #[test]
    fn float_pcm_is_converted_to_s16() {
        let out = to_s16(
            ENCODING_PCM_FLOAT,
            &[0.0f32.to_le_bytes(), 1.0f32.to_le_bytes(), (-1.0f32).to_le_bytes(), 2.0f32.to_le_bytes()].concat(),
        );
        let samples: Vec<i16> = out
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(samples, vec![0, 32767, -32767, 32767]);
    }

    #[test]
    fn sixteen_bit_pcm_passes_through() {
        let pcm = vec![1u8, 0, 2, 0, 3, 0, 4, 0];
        assert_eq!(to_s16(ENCODING_PCM_16BIT, &pcm), pcm);
    }

    #[test]
    fn writes_stop_at_the_buffer_limit_and_report_source_bytes() {
        // 8 frames of stereo s16 capacity.
        let t = track(2, 2, 8 * 4);
        let first = vec![0u8; 4 * 10];
        assert_eq!(push_pcm(&t, &first), 8 * 4, "only the buffer's worth fits");
        assert_eq!(push_pcm(&t, &first), 0, "a full buffer accepts nothing");
        t.shared.lock().unwrap().queue.clear();
        assert_eq!(push_pcm(&t, &first), 8 * 4);
    }

    #[test]
    fn dead_sink_reports_an_error_even_when_the_buffer_is_full() {
        let t = track(2, 2, 32);
        assert_eq!(push_pcm(&t, &[0; 32]), 32);
        t.shared.lock().unwrap().alive = false;
        assert_eq!(push_pcm(&t, &[0; 4]), -6);
        t.shared.lock().unwrap().queue.clear();
        assert_eq!(push_pcm(&t, &[0; 4]), -6);
    }

    #[test]
    fn float_writes_are_reported_in_float_bytes() {
        // 4 frames of stereo float capacity: 4 * 4 * 2 bytes requested.
        let t = track(4, 2, 4 * 8);
        let frames = |n: usize| vec![0u8; n * 8];
        assert_eq!(push_pcm(&t, &frames(6)), 4 * 8, "clamped to the buffer");
        assert_eq!(t.shared.lock().unwrap().queue.len(), 4 * 4, "s16 in the queue");
    }

    #[test]
    fn position_is_relative_to_the_last_flush() {
        let t = track(2, 2, 1024);
        {
            let mut sh = t.shared.lock().unwrap();
            sh.played = 100;
        }
        assert_eq!(t.position_frames(), 100);
        {
            let mut sh = t.shared.lock().unwrap();
            sh.base = 40;
        }
        assert_eq!(t.position_frames(), 60);
    }

    #[test]
    fn channel_mask_decides_the_channel_count() {
        // FRONT_LEFT | FRONT_RIGHT.
        assert_eq!(channel_count(0x3, 0), 2);
        // Empty masks fall back to stereo.
        assert_eq!(channel_count(0, 0), 2);
        // An index mask wins over the positional mask.
        assert_eq!(channel_count(0x3, 0x7), 3);
    }
}
