//! Frozen declarations from the API37 framework.jar DEX audit (2026-10-27).
//! framework.jar SHA256: 55f235f3b4e8d315545792092c94c2d3fb9968be6c9b2c1eb32016bff532bd04.
//! Independent test oracle: do not derive it from the implementation table.
use super::contract::Declaration;

pub(super) fn track_declarations() -> Vec<Declaration> {
    const DEX: &str = "
0102 native_applyVolumeShaper(Landroid/media/VolumeShaper$Configuration;Landroid/media/VolumeShaper$Operation;)I
0112 native_attachAuxEffect(I)I
0112 native_disableDeviceCallback()V
0112 native_enableDeviceCallback()V
0112 native_finalize()V
0112 native_flush()V
0102 native_flushFromFrame(IJ)J
010a native_getFlushWrittenFramesFromPositionSupport(IILandroid/media/AudioFormat$ChannelMasks;Landroid/media/AudioAttributes;)I
0102 native_getMetrics()Landroid/os/PersistableBundle;
0102 native_getPortId()I
0102 native_getRoutedDeviceIds()[I
0102 native_getStartThresholdInFrames()I
0102 native_getVolumeShaperState(I)Landroid/media/VolumeShaper$State;
0102 native_get_audio_description_mix_level_db([F)I
0112 native_get_buffer_capacity_frames()I
0112 native_get_buffer_size_frames()I
0102 native_get_dual_mono_mode([I)I
0112 native_get_flags()I
0112 native_get_latency()I
0112 native_get_marker_pos()I
011a native_get_min_buff_size(III)I
011a native_get_output_sample_rate(I)I
0112 native_get_playback_params()Landroid/media/PlaybackParams;
0112 native_get_playback_rate()I
0112 native_get_pos_update_period()I
0112 native_get_position()I
0112 native_get_timestamp([J)I
0112 native_get_underrun_count()I
0102 native_get_written_frames_count()J
010a native_is_direct_output_supported(IILjava/lang/Object;III)Z
0112 native_pause()V
0112 native_reload_static()I
0112 native_setAuxEffectSendLevel(F)I
0102 native_setLogSessionId(Ljava/lang/String;)V
0112 native_setOutputDevice(I)Z
0102 native_setPlayerIId(I)V
0112 native_setPresentation(II)I
0102 native_setStartThresholdInFrames(I)I
0112 native_setVolume(FF)V
0102 native_set_audio_description_mix_level_db(F)I
0112 native_set_buffer_size_frames(I)I
0102 native_set_delay_padding(II)V
0102 native_set_dual_mono_mode(I)I
0112 native_set_loop(III)I
0112 native_set_marker_pos(I)I
0112 native_set_playback_params(Landroid/media/PlaybackParams;)V
0112 native_set_playback_rate(I)I
0112 native_set_pos_update_period(I)I
0112 native_set_position(I)I
0112 native_setup(Ljava/lang/Object;Ljava/lang/Object;[ILjava/lang/Object;III[ILandroid/os/Parcel;JZILjava/lang/Object;Ljava/lang/String;Ljava/lang/String;)I
0112 native_start()V
0112 native_stop()V
0112 native_write_byte([BIIIZ)I
0112 native_write_float([FIIIZ)I
0112 native_write_native_bytes(Ljava/nio/ByteBuffer;IIIZ)I
0112 native_write_short([SIIIZ)I
0111 native_release()V
";
    DEX.lines().filter(|line| !line.is_empty()).map(|line| {
        let (flags, method) = line.split_once(' ').unwrap();
        let (name, rest) = method.split_once('(').unwrap();
        Declaration { name: name.into(), descriptor: format!("({rest}"), modifiers: i32::from_str_radix(flags, 16).unwrap() }
    }).collect()
}
