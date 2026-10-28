//! Exact API37 JNI entry points. Never call a real AudioTrack instance native.
use super::contract::{self, required_class, required_method, JniType, Spec};
use super::engine::{self, ChannelMasks, Track, BAD_VALUE, INVALID_OPERATION};
#[cfg(test)]
use super::engine::DEAD_OBJECT;
#[cfg(test)]
#[path = "jni_tests.rs"]
mod jni_tests;
use super::jni::{self, Env, Id, Object};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

static INSTALL_LOCK: Mutex<()> = Mutex::new(());
static READY: AtomicBool = AtomicBool::new(false);
static METADATA: OnceLock<Metadata> = OnceLock::new();

/// Four boot-class globals for one VM's lifetime; no per-track Java references.
/// Store opaque IDs as integers so JNIEnv/local references are never shared.
struct Metadata {
    vm: usize,
    track: usize,
    masks: usize,
    attributes: usize,
    params: usize,
    handle: usize,
    jni_data: usize,
    stream: usize,
    position_mask: usize,
    index_mask: usize,
    acn_mask: usize,
    channel_count: usize,
    attribute_flags: usize,
    volume_stream: usize,
    params_ctor: usize,
    params_defaults: usize,
}

unsafe fn required_field(env: Env, class: Object, name: &std::ffi::CStr, descriptor: &std::ffi::CStr) -> Result<usize, ()> {
    let id = jni::field(env, class, name, descriptor);
    if id.is_null() || jni::exception_check(env) { Err(()) } else { Ok(id as usize) }
}
impl Metadata {
    unsafe fn resolve(env: Env, vm: usize, track: Object) -> Result<Self, ()> {
        let masks = required_class(env, c"android/media/AudioFormat$ChannelMasks")?;
        let attributes = required_class(env, c"android/media/AudioAttributes")?;
        let params = required_class(env, c"android/media/PlaybackParams")?;
        let mut metadata = Self {
            vm, track: 0, masks: 0, attributes: 0, params: 0,
            handle: required_field(env, track, c"mNativeTrackInJavaObj", c"J")?,
            jni_data: required_field(env, track, c"mJniData", c"J")?,
            stream: required_field(env, track, c"mStreamType", c"I")?,
            position_mask: required_method(env, masks, c"getPositionMask", c"()I")? as usize,
            index_mask: required_method(env, masks, c"getIndexMask", c"()I")? as usize,
            acn_mask: required_method(env, masks, c"getAcnMask", c"()I")? as usize,
            channel_count: required_method(env, masks, c"getChannelCount", c"()I")? as usize,
            attribute_flags: required_method(env, attributes, c"getFlags", c"()I")? as usize,
            volume_stream: required_method(env, attributes, c"getVolumeControlStream", c"()I")? as usize,
            params_ctor: required_method(env, params, c"<init>", c"()V")? as usize,
            params_defaults: required_method(env, params, c"allowDefaults", c"()Landroid/media/PlaybackParams;")? as usize,
        };
        let mut globals = Vec::new();
        for class in [track, masks, attributes, params] {
            let global = jni::new_global_ref(env, class);
            if global.is_null() || jni::exception_check(env) {
                if !global.is_null() { jni::delete_global_ref(env, global); }
                for global in globals { jni::delete_global_ref(env, global); }
                return Err(());
            }
            globals.push(global);
        }
        metadata.track = globals[0] as usize;
        metadata.masks = globals[1] as usize;
        metadata.attributes = globals[2] as usize;
        metadata.params = globals[3] as usize;
        Ok(metadata)
    }
}

pub(super) unsafe fn register(env: Env) -> i32 {
    if env.is_null() || !cfg!(target_arch = "x86_64") { return -1; }
    if jni::exception_check(env) { return -1; }
    let _install = INSTALL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    READY.store(false, Ordering::Release);
    match install(env) {
        Ok(()) => { READY.store(true, Ordering::Release); 0 }
        Err(message) => { eprintln!("audio: API37 adapter installation FAILED: {message}"); -1 }
    }
}
unsafe fn install(env: Env) -> Result<(), String> {
    let _frame = jni::LocalFrame::new(env, 64).map_err(|_| "PushLocalFrame failed")?;
    if jni::sdk_int(env).map_err(|_| "cannot read SDK_INT")? != 37 {
        return Err("only Android 17 / SDK 37 is supported".into());
    }
    let vm = jni::java_vm(env).map_err(|_| "GetJavaVM failed")?;
    if METADATA.get().is_some_and(|m| m.vm != vm) {
        return Err("audio metadata belongs to another VM".into());
    }
    let track = required_class(env, c"android/media/AudioTrack").map_err(|_| "AudioTrack class missing")?;
    let system = required_class(env, c"android/media/AudioSystem").map_err(|_| "AudioSystem class missing")?;
    let track_specs = track_specs();
    let system_specs = system_specs();
    let track_declarations = contract::declarations(env, track).map_err(|_| "AudioTrack reflection failed")?;
    let system_declarations = contract::declarations(env, system).map_err(|_| "AudioSystem reflection failed")?;
    contract::validate(&track_specs, &track_declarations, true)?;
    contract::validate(&system_specs, &system_declarations, false)?;
    if METADATA.get().is_none() {
        let metadata = Metadata::resolve(env, vm, track).map_err(|_| "audio JNI metadata resolution failed")?;
        // Serialized by INSTALL_LOCK. Metadata is immutable and bounded, even
        // after a RegisterNatives failure. No setup is allowed until READY.
        if METADATA.set(metadata).is_err() { return Err("audio metadata already installed".into()); }
    }
    // Validate both classes and every pointer before the first registration.
    // RegisterNatives has no transactional rollback; a failure is fatal to the
    // caller's framework gate, and READY remains false (no new hosted handles).
    for (class, name, specs) in [(track, "android/media/AudioTrack", track_specs), (system, "android/media/AudioSystem", system_specs)] {
        let methods: Vec<_> = specs.iter().map(Spec::native).collect();
        let result = jni::register_natives(env, class, &methods);
        eprintln!("audio: API37 RegisterNatives {name} descriptors={} rc={result}", methods.len());
        if result != 0 || jni::exception_check(env) { return Err(format!("RegisterNatives({name}) rc={result}, pending exception={}", jni::exception_check(env))); }
        for spec in &specs {
            eprintln!("audio: API37 installed {name}.{}{} static={}", spec.name.to_string_lossy(), spec.descriptor.to_string_lossy(), spec.is_static);
        }
    }
    eprintln!("audio: hosted API37 AudioSystem=2 AudioTrack=57 output_rate={} (original groups must precede this call)", engine::OUTPUT_RATE);
    Ok(())
}

// Each entry first coerces to its exact extern-C function type. Descriptor ABI
// validation independently checks the primitive/reference arguments and result
// before that pointer is passed to RegisterNatives.
macro_rules! entry {
    ($name:expr, $descriptor:expr, $static:expr, $function:ident, ($($arg:ty),*) -> $result:ty) => {{
        let typed: unsafe extern "C" fn(Env, Object, $($arg),*) -> $result = $function;
        Spec { name: $name, descriptor: $descriptor, is_static: $static,
            pointer: typed as *const () as *mut std::os::raw::c_void,
            arguments: &[$(<$arg as JniType>::ABI),*], result: <$result as JniType>::ABI }
    }};
}
pub(super) fn system_specs() -> Vec<Spec> {
    vec![entry!(c"newAudioSessionId", c"()I", true, native_new_session, () -> i32),
         entry!(c"newAudioPlayerId", c"()I", true, native_new_player, () -> i32)]
}
pub(super) fn track_specs() -> Vec<Spec> {
    vec![
        entry!(c"native_is_direct_output_supported", c"(IILjava/lang/Object;III)Z", true, native_direct_supported, (i32, i32, Object, i32, i32, i32) -> u8),
        entry!(c"native_start", c"()V", false, native_start, () -> ()),
        entry!(c"native_stop", c"()V", false, native_stop, () -> ()),
        entry!(c"native_pause", c"()V", false, native_pause, () -> ()),
        entry!(c"native_flush", c"()V", false, native_flush, () -> ()),
        entry!(c"native_setup", contract::SETUP_DESCRIPTOR, false, native_setup, (Object, Object, Object, Object, i32, i32, i32, Object, Object, i64, u8, i32, Object, Object, Object) -> i32),
        entry!(c"native_finalize", c"()V", false, native_release, () -> ()),
        entry!(c"native_release", c"()V", false, native_release, () -> ()),
        entry!(c"native_write_byte", c"([BIIIZ)I", false, native_write_byte, (Object, i32, i32, i32, u8) -> i32),
        entry!(c"native_write_native_bytes", c"(Ljava/nio/ByteBuffer;IIIZ)I", false, native_write_native_bytes, (Object, i32, i32, i32, u8) -> i32),
        entry!(c"native_write_short", c"([SIIIZ)I", false, native_write_short, (Object, i32, i32, i32, u8) -> i32),
        entry!(c"native_write_float", c"([FIIIZ)I", false, native_write_float, (Object, i32, i32, i32, u8) -> i32),
        entry!(c"native_setVolume", c"(FF)V", false, native_set_volume, (f32, f32) -> ()),
        entry!(c"native_get_buffer_size_frames", c"()I", false, native_buffer_size, () -> i32),
        entry!(c"native_set_buffer_size_frames", c"(I)I", false, native_resize, (i32) -> i32),
        entry!(c"native_get_buffer_capacity_frames", c"()I", false, native_buffer_capacity, () -> i32),
        entry!(c"native_set_playback_rate", c"(I)I", false, native_set_rate, (i32) -> i32),
        entry!(c"native_get_playback_rate", c"()I", false, native_get_rate, () -> i32),
        entry!(c"native_set_playback_params", c"(Landroid/media/PlaybackParams;)V", false, native_set_params, (Object) -> ()),
        entry!(c"native_get_playback_params", c"()Landroid/media/PlaybackParams;", false, native_get_params, () -> Object),
        entry!(c"native_set_marker_pos", c"(I)I", false, native_set_marker, (i32) -> i32),
        entry!(c"native_get_marker_pos", c"()I", false, native_get_marker, () -> i32),
        entry!(c"native_set_pos_update_period", c"(I)I", false, native_set_period, (i32) -> i32),
        entry!(c"native_get_pos_update_period", c"()I", false, native_get_period, () -> i32),
        entry!(c"native_get_written_frames_count", c"()J", false, native_written_frames, () -> i64),
        entry!(c"native_set_position", c"(I)I", false, native_set_position, (i32) -> i32),
        entry!(c"native_get_position", c"()I", false, native_position, () -> i32),
        entry!(c"native_get_latency", c"()I", false, native_latency, () -> i32),
        entry!(c"native_get_underrun_count", c"()I", false, native_underruns, () -> i32),
        entry!(c"native_get_flags", c"()I", false, native_flags, () -> i32),
        entry!(c"native_get_timestamp", c"([J)I", false, native_timestamp, (Object) -> i32),
        entry!(c"native_getMetrics", c"()Landroid/os/PersistableBundle;", false, native_metrics, () -> Object),
        entry!(c"native_set_loop", c"(III)I", false, native_set_loop, (i32, i32, i32) -> i32),
        entry!(c"native_reload_static", c"()I", false, native_reload, () -> i32),
        entry!(c"native_get_output_sample_rate", c"(I)I", true, native_output_rate, (i32) -> i32),
        entry!(c"native_get_min_buff_size", c"(III)I", true, native_min_buffer, (i32, i32, i32) -> i32),
        entry!(c"native_setAuxEffectSendLevel", c"(F)I", false, native_aux_level, (f32) -> i32),
        entry!(c"native_attachAuxEffect", c"(I)I", false, native_attach_aux, (i32) -> i32),
        entry!(c"native_setOutputDevice", c"(I)Z", false, native_set_device, (i32) -> u8),
        entry!(c"native_getRoutedDeviceIds", c"()[I", false, native_routed_devices, () -> Object),
        entry!(c"native_enableDeviceCallback", c"()V", false, native_enable_callback, () -> ()),
        entry!(c"native_disableDeviceCallback", c"()V", false, native_disable_callback, () -> ()),
        entry!(c"native_applyVolumeShaper", c"(Landroid/media/VolumeShaper$Configuration;Landroid/media/VolumeShaper$Operation;)I", false, native_apply_shaper, (Object, Object) -> i32),
        entry!(c"native_getVolumeShaperState", c"(I)Landroid/media/VolumeShaper$State;", false, native_shaper_state, (i32) -> Object),
        entry!(c"native_setPresentation", c"(II)I", false, native_presentation, (i32, i32) -> i32),
        entry!(c"native_getPortId", c"()I", false, native_port, () -> i32),
        entry!(c"native_set_delay_padding", c"(II)V", false, native_delay_padding, (i32, i32) -> ()),
        entry!(c"native_set_audio_description_mix_level_db", c"(F)I", false, native_set_description, (f32) -> i32),
        entry!(c"native_get_audio_description_mix_level_db", c"([F)I", false, native_get_description, (Object) -> i32),
        entry!(c"native_set_dual_mono_mode", c"(I)I", false, native_set_dual_mono, (i32) -> i32),
        entry!(c"native_get_dual_mono_mode", c"([I)I", false, native_get_dual_mono, (Object) -> i32),
        entry!(c"native_setLogSessionId", c"(Ljava/lang/String;)V", false, native_log_session, (Object) -> ()),
        entry!(c"native_setPlayerIId", c"(I)V", false, native_player_id, (i32) -> ()),
        entry!(c"native_setStartThresholdInFrames", c"(I)I", false, native_set_threshold, (i32) -> i32),
        entry!(c"native_getStartThresholdInFrames", c"()I", false, native_get_threshold, () -> i32),
        entry!(c"native_flushFromFrame", c"(IJ)J", false, native_flush_from_frame, (i32, i64) -> i64),
        entry!(c"native_getFlushWrittenFramesFromPositionSupport", c"(IILandroid/media/AudioFormat$ChannelMasks;Landroid/media/AudioAttributes;)I", true, native_flush_support, (i32, i32, Object, Object) -> i32),
    ]
}

unsafe fn track_for(env: Env, thiz: Object) -> Result<Arc<Track>, i32> {
    if env.is_null() || thiz.is_null() || jni::exception_check(env) { return Err(INVALID_OPERATION); }
    let Some(metadata) = METADATA.get() else { return Err(INVALID_OPERATION); };
    if !jni::is_instance(env, thiz, metadata.track as Object) || jni::exception_check(env) { return Err(INVALID_OPERATION); }
    let handle = jni::get_long(env, thiz, metadata.handle as Id);
    if jni::exception_check(env) { return Err(INVALID_OPERATION); }
    engine::get(handle).ok_or_else(|| {
        jni::throw(env, c"java/lang/IllegalStateException", "AudioTrack is released, uninitialized, or not a hosted handle");
        INVALID_OPERATION
    })
}
unsafe fn unsupported(env: Env, message: &str) {
    jni::throw(env, c"java/lang/UnsupportedOperationException", message);
}
unsafe fn valid_array(env: Env, array: Object, minimum: i32) -> bool {
    if array.is_null() || jni::exception_check(env) { return false; }
    let length = jni::array_length(env, array);
    !jni::exception_check(env) && length >= minimum
}
unsafe fn read_masks(env: Env, masks: Object, metadata: &Metadata) -> Result<ChannelMasks, i32> {
    if !jni::is_instance(env, masks, metadata.masks as Object) || jni::exception_check(env) { return Err(BAD_VALUE); }
    let mut values = [0; 4];
    for (value, id) in values.iter_mut().zip([metadata.position_mask, metadata.index_mask, metadata.acn_mask, metadata.channel_count]) {
        *value = jni::call_int(env, masks, id as Id, &[]);
        if jni::exception_check(env) { return Err(BAD_VALUE); }
    }
    Ok(ChannelMasks { position: values[0], index: values[1], acn: values[2], count: values[3] })
}

unsafe extern "C" fn native_new_session(_env: Env, _class: Object) -> i32 { engine::new_session() }
unsafe extern "C" fn native_new_player(_env: Env, _class: Object) -> i32 { engine::new_player() }
unsafe extern "C" fn native_min_buffer(_env: Env, _class: Object, rate: i32, channel_count: i32, encoding: i32) -> i32 {
    engine::min_buffer(rate, channel_count, encoding)
}
unsafe extern "C" fn native_output_rate(_env: Env, _class: Object, _stream_type: i32) -> i32 {
    // All stream types use the same hosted output; this argument is NEVER Hz.
    engine::OUTPUT_RATE
}
unsafe extern "C" fn native_direct_supported(_env: Env, _class: Object, _encoding: i32, _rate: i32, _masks: Object, _content: i32, _usage: i32, _flags: i32) -> u8 { 0 }
unsafe extern "C" fn native_flush_support(_env: Env, _class: Object, _encoding: i32, _rate: i32, _masks: Object, _attributes: Object) -> i32 { 0 } // UNSUPPORTED

unsafe extern "C" fn native_setup(
    env: Env, thiz: Object, _weak_this: Object, attributes: Object,
    sample_rates: Object, masks: Object, encoding: i32, buffer_bytes: i32,
    memory_mode: i32, session_ids: Object, attribution_parcel: Object,
    native_audio_track: i64, offload: u8, encapsulation: i32,
    tuner_configuration: Object, _op_package_name: Object, _codec_provenance: Object,
) -> i32 {
    if env.is_null() || !READY.load(Ordering::Acquire) { return INVALID_OPERATION; }
    let Some(metadata) = METADATA.get() else { return INVALID_OPERATION; };
    if jni::exception_check(env) { return INVALID_OPERATION; }
    if native_audio_track != 0 || offload != 0 || encapsulation != 0 || !tuner_configuration.is_null() || memory_mode != 1 {
        return INVALID_OPERATION;
    }
    if !jni::is_instance(env, thiz, metadata.track as Object) || jni::exception_check(env) { return BAD_VALUE; }
    let old_handle = jni::get_long(env, thiz, metadata.handle as Id);
    if jni::exception_check(env) { return BAD_VALUE; }
    let old_jni = jni::get_long(env, thiz, metadata.jni_data as Id);
    if jni::exception_check(env) { return BAD_VALUE; }
    // Do not overwrite/release real C++ state or a previous hosted setup.
    if old_handle != 0 || old_jni != 0 { return INVALID_OPERATION; }
    if attribution_parcel.is_null() || !valid_array(env, sample_rates, 1) || !valid_array(env, session_ids, 1) { return BAD_VALUE; }
    if jni::is_same(env, sample_rates, session_ids) || jni::exception_check(env) { return BAD_VALUE; }
    if !jni::is_instance(env, attributes, metadata.attributes as Object) || jni::exception_check(env) { return BAD_VALUE; }
    let flags = jni::call_int(env, attributes, metadata.attribute_flags as Id, &[]);
    if jni::exception_check(env) { return BAD_VALUE; }
    if flags & 0x10 != 0 { return INVALID_OPERATION; } // FLAG_HW_AV_SYNC needs timestamped/encapsulated writes.
    let stream = jni::call_int(env, attributes, metadata.volume_stream as Id, &[]);
    if jni::exception_check(env) { return BAD_VALUE; }
    let masks = match read_masks(env, masks, metadata).and_then(ChannelMasks::channels) { Ok(channels) => channels, Err(error) => return error };
    let mut rate = [0]; let mut requested_session = [0];
    jni::get_ints(env, sample_rates, 0, &mut rate);
    if jni::exception_check(env) { return BAD_VALUE; }
    jni::get_ints(env, session_ids, 0, &mut requested_session);
    if jni::exception_check(env) { return BAD_VALUE; }
    let rate = if rate[0] == 0 { engine::OUTPUT_RATE } else { rate[0] };
    let track = match Track::new(rate, masks, encoding, buffer_bytes) { Ok(track) => track, Err(error) => return error };
    let session = match engine::session(requested_session[0]) { Ok(session) => session, Err(error) => return error };
    let handle = match engine::insert(track) { Ok(handle) => handle as i64, Err(error) => return error };
    jni::set_ints(env, sample_rates, &[rate]);
    if jni::exception_check(env) { engine::remove(handle); return BAD_VALUE; }
    jni::set_ints(env, session_ids, &[session]);
    if jni::exception_check(env) { engine::remove(handle); return BAD_VALUE; }
    jni::set_int(env, thiz, metadata.stream as Id, stream);
    if jni::exception_check(env) { engine::remove(handle); return BAD_VALUE; }
    // mJniData stays zero: there is no AudioTrackJniStorage/C++ ownership here.
    jni::set_long(env, thiz, metadata.handle as Id, handle);
    if jni::exception_check(env) { engine::remove(handle); return BAD_VALUE; }
    eprintln!("audio: API37 setup handle={handle:#x} rate={rate} channels={masks} encoding={encoding} session={session} capacity_bytes={buffer_bytes}");
    0
}

unsafe extern "C" fn native_release(env: Env, thiz: Object) {
    if env.is_null() || thiz.is_null() || jni::exception_check(env) { return; }
    let Some(metadata) = METADATA.get() else { return; };
    if !jni::is_instance(env, thiz, metadata.track as Object) || jni::exception_check(env) { return; }
    let handle = jni::get_long(env, thiz, metadata.handle as Id);
    if jni::exception_check(env) || handle == 0 { return; }
    if engine::get(handle).is_none() {
        jni::throw(env, c"java/lang/IllegalStateException", "refusing to release foreign AudioTrack native state");
        return;
    }
    jni::set_long(env, thiz, metadata.handle as Id, 0);
    if jni::exception_check(env) { return; }
    engine::remove(handle);
    // Never call setFieldSp or a real release/finalize with an integer handle.
}
unsafe extern "C" fn native_start(env: Env, thiz: Object) {
    if let Ok(track) = track_for(env, thiz) {
        if track.start().is_err() { jni::throw(env, c"java/lang/IllegalStateException", "host AudioTrack sink is unavailable or dead"); }
    }
}
unsafe extern "C" fn native_pause(env: Env, thiz: Object) { if let Ok(track) = track_for(env, thiz) { track.pause(); } }
unsafe extern "C" fn native_stop(env: Env, thiz: Object) { if let Ok(track) = track_for(env, thiz) { track.stop(); } }
unsafe extern "C" fn native_flush(env: Env, thiz: Object) { if let Ok(track) = track_for(env, thiz) { track.flush(); } }

#[derive(Clone, Copy)]
enum WriteKind { Byte, Short, Float }
impl WriteKind {
    fn bytes(self) -> usize { match self { Self::Byte => 1, Self::Short => 2, Self::Float => 4 } }
    fn accepts(self, encoding: i32) -> bool { match self { Self::Byte => true, Self::Short => encoding == engine::PCM_16, Self::Float => encoding == engine::PCM_FLOAT } }
}

unsafe fn write_array(env: Env, thiz: Object, array: Object, offset: i32, size: i32, format: i32, blocking: u8, kind: WriteKind) -> i32 {
    let track = match track_for(env, thiz) { Ok(track) => track, Err(error) => return error };
    if array.is_null() || !kind.accepts(track.encoding) || format != track.encoding || blocking > 1 { return BAD_VALUE; }
    let length = jni::array_length(env, array);
    if jni::exception_check(env) || engine::checked_range(length as i64, offset, size).is_err() { return BAD_VALUE; }
    let element = kind.bytes();
    let aligned_bytes = (size as usize * element / track.source_frame()) * track.source_frame();
    let mut consumed = 0;
    while consumed < aligned_bytes {
        let bytes = ((aligned_bytes - consumed).min(64 * 1024) / track.source_frame()) * track.source_frame();
        let start = offset + (consumed / element) as i32;
        let mut data = Vec::with_capacity(bytes);
        match kind {
            WriteKind::Byte => { data.resize(bytes, 0); jni::get_bytes(env, array, start, &mut data); }
            WriteKind::Short => {
                let mut samples = vec![0; bytes / 2]; jni::get_shorts(env, array, start, &mut samples);
                for sample in samples { data.extend_from_slice(&sample.to_le_bytes()); }
            }
            WriteKind::Float => {
                let mut samples = vec![0.0; bytes / 4]; jni::get_floats(env, array, start, &mut samples);
                for sample in samples { data.extend_from_slice(&sample.to_le_bytes()); }
            }
        }
        if jni::exception_check(env) { return if consumed == 0 { BAD_VALUE } else { (consumed / element) as i32 }; }
        let result = track.write(&data, blocking != 0);
        if result < 0 { return if consumed == 0 { result } else { (consumed / element) as i32 }; }
        consumed += result as usize;
        if blocking == 0 || result as usize != data.len() { break; }
    }
    (consumed / element) as i32
}
unsafe extern "C" fn native_write_byte(env: Env, thiz: Object, array: Object, offset: i32, size: i32, format: i32, blocking: u8) -> i32 {
    write_array(env, thiz, array, offset, size, format, blocking, WriteKind::Byte)
}
unsafe extern "C" fn native_write_short(env: Env, thiz: Object, array: Object, offset: i32, size: i32, format: i32, blocking: u8) -> i32 {
    write_array(env, thiz, array, offset, size, format, blocking, WriteKind::Short)
}
unsafe extern "C" fn native_write_float(env: Env, thiz: Object, array: Object, offset: i32, size: i32, format: i32, blocking: u8) -> i32 {
    write_array(env, thiz, array, offset, size, format, blocking, WriteKind::Float)
}
unsafe extern "C" fn native_write_native_bytes(env: Env, thiz: Object, buffer: Object, position: i32, size: i32, format: i32, blocking: u8) -> i32 {
    let track = match track_for(env, thiz) { Ok(track) => track, Err(error) => return error };
    if buffer.is_null() || format != track.encoding || blocking > 1 { return BAD_VALUE; }
    let capacity = jni::direct_capacity(env, buffer);
    if jni::exception_check(env) { return BAD_VALUE; }
    let range = match engine::checked_range(capacity, position, size) { Ok(range) => range, Err(error) => return error };
    if range.is_empty() { return 0; }
    let base = jni::direct_address(env, buffer);
    if base.is_null() || jni::exception_check(env) { return BAD_VALUE; }
    // Capacity/range checks precede pointer arithmetic and slice construction.
    track.write(std::slice::from_raw_parts(base.add(range.start), range.len()), blocking != 0)
}

unsafe extern "C" fn native_set_volume(env: Env, thiz: Object, left: f32, right: f32) {
    if !left.is_finite() || !right.is_finite() || !(0.0..=1.0).contains(&left) || !(0.0..=1.0).contains(&right) {
        jni::throw(env, c"java/lang/IllegalArgumentException", "invalid AudioTrack volume"); return;
    }
    if let Ok(track) = track_for(env, thiz) { track.shared.lock().unwrap_or_else(|e| e.into_inner()).volume = [left, right]; }
}
unsafe extern "C" fn native_position(env: Env, thiz: Object) -> i32 {
    match track_for(env, thiz) { Ok(t) => t.shared.lock().unwrap_or_else(|e| e.into_inner()).position() as u32 as i32, Err(e) => e }
}
unsafe extern "C" fn native_written_frames(env: Env, thiz: Object) -> i64 {
    match track_for(env, thiz) { Ok(t) => t.shared.lock().unwrap_or_else(|e| e.into_inner()).written.min(i64::MAX as u64) as i64, Err(e) => e as i64 }
}
unsafe extern "C" fn native_timestamp(env: Env, thiz: Object, out: Object) -> i32 {
    let track = match track_for(env, thiz) { Ok(track) => track, Err(error) => return error };
    if !valid_array(env, out, 2) { return BAD_VALUE; }
    let values = { let sh = track.shared.lock().unwrap_or_else(|e| e.into_inner());
        if sh.timestamp_ns == 0 { return INVALID_OPERATION; }
        [sh.position() as i64, sh.timestamp_ns]
    };
    jni::set_longs(env, out, &values);
    if jni::exception_check(env) { BAD_VALUE } else { 0 }
}
unsafe extern "C" fn native_buffer_size(env: Env, thiz: Object) -> i32 {
    match track_for(env, thiz) { Ok(t) => t.shared.lock().unwrap_or_else(|e| e.into_inner()).buffer_frames as i32, Err(e) => e }
}
unsafe extern "C" fn native_buffer_capacity(env: Env, thiz: Object) -> i32 {
    match track_for(env, thiz) { Ok(t) => t.capacity_frames as i32, Err(e) => e }
}
unsafe extern "C" fn native_resize(env: Env, thiz: Object, frames: i32) -> i32 {
    match track_for(env, thiz) { Ok(t) => t.resize(frames), Err(e) => e }
}
unsafe extern "C" fn native_latency(env: Env, thiz: Object) -> i32 {
    match track_for(env, thiz) { Ok(t) => (u64::from(t.capacity_frames) * 1000 / u64::from(t.rate)) as i32, Err(e) => e }
}
unsafe extern "C" fn native_get_rate(env: Env, thiz: Object) -> i32 {
    match track_for(env, thiz) { Ok(t) => t.rate as i32, Err(e) => e }
}
unsafe extern "C" fn native_set_rate(env: Env, thiz: Object, rate: i32) -> i32 {
    match track_for(env, thiz) { Ok(t) if t.rate as i32 == rate => 0, Ok(_) if !engine::valid_rate(rate) => BAD_VALUE, Ok(_) => INVALID_OPERATION, Err(e) => e }
}
unsafe extern "C" fn native_set_threshold(env: Env, thiz: Object, frames: i32) -> i32 {
    match track_for(env, thiz) { Ok(t) => t.set_threshold(frames), Err(e) => e }
}
unsafe extern "C" fn native_get_threshold(env: Env, thiz: Object) -> i32 {
    match track_for(env, thiz) { Ok(t) => t.shared.lock().unwrap_or_else(|e| e.into_inner()).start_threshold as i32, Err(e) => e }
}
unsafe extern "C" fn native_get_params(env: Env, thiz: Object) -> Object {
    if track_for(env, thiz).is_err() { return std::ptr::null_mut(); }
    let Some(metadata) = METADATA.get() else { return std::ptr::null_mut(); };
    let params = jni::new_object(env, metadata.params as Object, metadata.params_ctor as Id);
    if params.is_null() || jni::exception_check(env) { return std::ptr::null_mut(); }
    let defaults = jni::call_object(env, params, metadata.params_defaults as Id, &[]);
    jni::delete_local_ref(env, params);
    if jni::exception_check(env) { std::ptr::null_mut() } else { defaults }
}
unsafe extern "C" fn native_set_params(env: Env, thiz: Object, params: Object) {
    if track_for(env, thiz).is_err() { return; }
    if params.is_null() {
        jni::throw(env, c"java/lang/IllegalArgumentException", "PlaybackParams is null");
    } else {
        unsupported(env, "host AudioTrack does not support PlaybackParams speed/pitch processing");
    }
}
unsafe extern "C" fn native_set_marker(env: Env, thiz: Object, marker: i32) -> i32 {
    match track_for(env, thiz) { Ok(_) if marker == 0 => 0, Ok(_) => INVALID_OPERATION, Err(e) => e }
}
unsafe extern "C" fn native_get_marker(env: Env, thiz: Object) -> i32 {
    match track_for(env, thiz) { Ok(_) => 0, Err(e) => e }
}
unsafe extern "C" fn native_set_period(env: Env, thiz: Object, period: i32) -> i32 {
    match track_for(env, thiz) { Ok(_) if period < 0 => BAD_VALUE, Ok(_) if period == 0 => 0, Ok(_) => INVALID_OPERATION, Err(e) => e }
}
unsafe extern "C" fn native_get_period(env: Env, thiz: Object) -> i32 {
    match track_for(env, thiz) { Ok(_) => 0, Err(e) => e }
}
unsafe extern "C" fn native_set_position(env: Env, thiz: Object, _position: i32) -> i32 {
    match track_for(env, thiz) { Ok(_) => INVALID_OPERATION, Err(e) => e }
}
unsafe extern "C" fn native_set_loop(env: Env, thiz: Object, _start: i32, _end: i32, _count: i32) -> i32 {
    match track_for(env, thiz) { Ok(_) => INVALID_OPERATION, Err(e) => e }
}
unsafe extern "C" fn native_reload(env: Env, thiz: Object) -> i32 {
    match track_for(env, thiz) { Ok(_) => INVALID_OPERATION, Err(e) => e }
}
unsafe extern "C" fn native_underruns(env: Env, thiz: Object) -> i32 {
    if track_for(env, thiz).is_ok() { unsupported(env, "host pipe sink does not expose hardware underrun counters"); }
    INVALID_OPERATION
}
unsafe extern "C" fn native_flags(env: Env, thiz: Object) -> i32 {
    match track_for(env, thiz) { Ok(_) => 0, Err(e) => e } // AUDIO_OUTPUT_FLAG_NONE
}
unsafe extern "C" fn native_metrics(env: Env, thiz: Object) -> Object {
    if track_for(env, thiz).is_ok() { unsupported(env, "host AudioTrack has no Android MediaMetrics service"); }
    std::ptr::null_mut()
}
unsafe extern "C" fn native_aux_level(env: Env, thiz: Object, level: f32) -> i32 {
    match track_for(env, thiz) { Ok(_) if !level.is_finite() || !(0.0..=1.0).contains(&level) => BAD_VALUE, Ok(_) if level == 0.0 => 0, Ok(_) => INVALID_OPERATION, Err(e) => e }
}
unsafe extern "C" fn native_attach_aux(env: Env, thiz: Object, effect: i32) -> i32 {
    match track_for(env, thiz) { Ok(_) if effect == 0 => 0, Ok(_) => INVALID_OPERATION, Err(e) => e }
}
unsafe extern "C" fn native_set_device(env: Env, thiz: Object, _device: i32) -> u8 {
    let _ = track_for(env, thiz); 0 // No Android device routing, including no claimed default-route binding.
}
unsafe extern "C" fn native_routed_devices(env: Env, thiz: Object) -> Object {
    if track_for(env, thiz).is_err() { return std::ptr::null_mut(); }
    jni::new_int_array(env, 0) // No Android AudioPort IDs; a non-null empty array is the truthful route.
}
unsafe extern "C" fn native_enable_callback(env: Env, thiz: Object) {
    // `AudioTrack.startImpl` calls this unconditionally and only catches
    // `IllegalStateException`, so refusing here would make `play()` fail for
    // every hosted track. The host sink has exactly one fixed output and no
    // routing changes, so accepting the registration and never invoking the
    // listener is the truthful result; a real device also reports no callback
    // until a route actually changes.
    let _ = track_for(env, thiz);
}
unsafe extern "C" fn native_disable_callback(env: Env, thiz: Object) { let _ = track_for(env, thiz); }
unsafe extern "C" fn native_apply_shaper(env: Env, thiz: Object, _configuration: Object, _operation: Object) -> i32 {
    let _ = track_for(env, thiz);
    // VolumeShaper's JNI status namespace is NOT AudioTrack.ERROR_INVALID_OPERATION.
    // Pinned android_media_AudioTrack.cpp maps it explicitly to -38.
    -38
}
unsafe extern "C" fn native_shaper_state(env: Env, thiz: Object, _id: i32) -> Object {
    let _ = track_for(env, thiz); std::ptr::null_mut() // Nullable: no shaper was accepted.
}
unsafe extern "C" fn native_presentation(env: Env, thiz: Object, _presentation: i32, _program: i32) -> i32 {
    match track_for(env, thiz) { Ok(_) => INVALID_OPERATION, Err(e) => e }
}
unsafe extern "C" fn native_port(env: Env, thiz: Object) -> i32 {
    match track_for(env, thiz) { Ok(_) => 0, Err(e) => e } // AUDIO_PORT_HANDLE_NONE
}
unsafe extern "C" fn native_delay_padding(env: Env, thiz: Object, _delay: i32, _padding: i32) {
    if track_for(env, thiz).is_ok() { unsupported(env, "delay/padding controls require unsupported compressed offload"); }
}
unsafe extern "C" fn native_set_description(env: Env, thiz: Object, _level: f32) -> i32 {
    match track_for(env, thiz) { Ok(_) => INVALID_OPERATION, Err(e) => e }
}
unsafe extern "C" fn native_get_description(env: Env, thiz: Object, out: Object) -> i32 {
    if !valid_array(env, out, 1) { return BAD_VALUE; }
    match track_for(env, thiz) { Ok(_) => INVALID_OPERATION, Err(e) => e } // No success with an unwritten output.
}
unsafe extern "C" fn native_set_dual_mono(env: Env, thiz: Object, mode: i32) -> i32 {
    match track_for(env, thiz) { Ok(_) if mode == 0 => 0, Ok(_) => INVALID_OPERATION, Err(e) => e }
}
unsafe extern "C" fn native_get_dual_mono(env: Env, thiz: Object, out: Object) -> i32 {
    if !valid_array(env, out, 1) { return BAD_VALUE; }
    if let Err(error) = track_for(env, thiz) { return error; }
    jni::set_ints(env, out, &[0]); // DUAL_MONO_MODE_OFF
    if jni::exception_check(env) { BAD_VALUE } else { 0 }
}
unsafe extern "C" fn native_log_session(env: Env, thiz: Object, session: Object) {
    let Ok(track) = track_for(env, thiz) else { return; };
    let value = if session.is_null() { String::new() } else {
        match jni::string(env, session) {
            Ok(value) if value.len() <= 256 => value,
            _ => { jni::throw(env, c"java/lang/IllegalArgumentException", "invalid log session ID"); return; }
        }
    };
    track.shared.lock().unwrap_or_else(|e| e.into_inner()).log_session_id = value;
}
unsafe extern "C" fn native_player_id(env: Env, thiz: Object, id: i32) {
    if let Ok(track) = track_for(env, thiz) { track.shared.lock().unwrap_or_else(|e| e.into_inner()).player_id = id; }
}
unsafe extern "C" fn native_flush_from_frame(env: Env, thiz: Object, accuracy: i32, position: i64) -> i64 {
    if track_for(env, thiz).is_err() { return INVALID_OPERATION as i64; }
    if !(0..=1).contains(&accuracy) || position < 0 {
        jni::throw(env, c"java/lang/IllegalArgumentException", "invalid flush accuracy or frame position");
    } else {
        unsupported(env, "host AudioTrack does not support flushWrittenFramesFromPosition");
    }
    INVALID_OPERATION as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn extern_c_session_and_stream_preflight_semantics() {
        unsafe {
            let allocator: unsafe extern "C" fn(Env, Object) -> i32 = native_new_session;
            let first = allocator(std::ptr::null_mut(), std::ptr::null_mut());
            assert!(first > 0); assert!(allocator(std::ptr::null_mut(), std::ptr::null_mut()) > first);
            assert_eq!(native_output_rate(std::ptr::null_mut(), std::ptr::null_mut(), 3), 48_000);
            assert_eq!(native_min_buffer(std::ptr::null_mut(), std::ptr::null_mut(), 44_100, 2, engine::PCM_16), 3528);
            assert_eq!(native_setup(std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), 2, 4096, 1, std::ptr::null_mut(), std::ptr::null_mut(), 0, 0, 0, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()), INVALID_OPERATION);
            assert_eq!(register(std::ptr::null_mut()), -1);
        }
    }
    #[test]
    fn array_element_types_and_sizes_match_supported_encodings() {
        assert_eq!(WriteKind::Byte.bytes(), 1);
        assert_eq!(WriteKind::Short.bytes(), 2);
        assert_eq!(WriteKind::Float.bytes(), 4);
        assert!(WriteKind::Float.accepts(engine::PCM_FLOAT));
        assert!(!WriteKind::Float.accepts(engine::PCM_16));
        assert!(!WriteKind::Short.accepts(engine::PCM_FLOAT));
    }
}
