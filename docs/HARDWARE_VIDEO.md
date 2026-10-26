# Host hardware video decoding

## Current scope

PenguDroid has a host-only VA-API decoder and a direct multi-plane Wayland import path. They have been tested together on Intel UHD 630 with the iHD driver and GNOME/Mutter. **NewPipe still uses Android's software Codec2 plugins.** The host decoder is not yet registered as an Android Codec2 component, and no compressed-packet broker protocol has been added.

| Path | Evidence / status |
| --- | --- |
| VA-API discovery | Real VLD profiles queried per DRM render node |
| H.264 compressed input → GPU decode | Real 128×128 and 1280×720 constrained-baseline gradient fixtures decoded |
| GPU surface → DMA-BUF | Synchronized, composed NV12 export with both planes, object indices, offsets, pitches and modifiers |
| DMA-BUF → Wayland | 32 decoded frames imported directly on GNOME; no RGB conversion, CPU scaling or pixel upload |
| Release and backpressure | Four returned decode frames maximum; three outstanding compositor buffers per surface; source held through actual `wl_buffer.release` |
| Flush / EOS / timestamps | Fixture integration test checks flush, EOS drain and microsecond output timestamps |
| NewPipe / MediaCodec adapter | Pending |
| AMD | Same VA-API API path is available; real hardware validation pending |
| NVIDIA | NVDEC backend and its GPU interop are pending; VA-API support is not inferred from the NVIDIA name |
| Vulkan | Separate work; loading the host Vulkan loader does not provide Android surface integration |

The normal decode/export path never calls `av_hwframe_transfer_data`. An explicit fixture diagnostic can download one frame to verify pixel values; that diagnostic is not part of playback. If hardware format negotiation, device initialization or export fails, the decoder reports an error rather than selecting a software decoder.

## Native bridge and build

The optional `libpengudroid_video.so` is a small C bridge compiled against the host's public FFmpeg and libva headers. Its versioned C ABI exposes no FFmpeg structure layouts to Rust or Android. `pengudroid-hostgpu::video::Decoder` loads it at runtime. It belongs in the **host/broker process**, not the Bionic guest's linker namespace.

Dependencies are a C11 compiler, pkg-config and development packages for libavcodec, libavutil and libva (on Debian: `libavcodec-dev libavutil-dev libva-dev`). Ordinary Cargo builds remain usable without these development packages. Build the optional plugin after building the CLI:

```sh
cargo build --workspace
tools/build-video-bridge.sh
```

The default output is `target/debug/libpengudroid_video.so`. Pass an output path for another layout, such as `target/release/libpengudroid_video.so`. At runtime the Rust API searches beside the executable, or uses the explicit `PENGUDROID_VIDEO_BRIDGE` path. The plugin links to the host FFmpeg/libva SONAMEs; rebuild it when changing those ABI versions. This machine used locally extracted Debian development headers, without changing installed system packages.

## Reproduce the real hardware tests

```sh
python3 tools/make_h264.py /tmp/pd-gradient.h264 8 8
target/debug/pengudroid video-decode-test /tmp/pd-gradient.h264 --verify-gradient

export PENGUDROID_TEST_VIDEO_BRIDGE="$PWD/target/debug/libpengudroid_video.so"
export PENGUDROID_TEST_VIDEO_FIXTURE=/tmp/pd-gradient.h264
cargo test -p pengudroid-hostgpu --test hardware_video -- --nocapture
cargo test -p pengudroid-daemon --lib hardware_video_import -- --nocapture
```

Set `PENGUDROID_TEST_VIDEO_DEVICE` for a render node other than `/dev/dri/renderD128`. The CLI also accepts `--device`. These tests require real hardware; they do not substitute software decoding. Without the explicit plugin/fixture environment, the hardware integration cases skip and print/return accordingly; a passing ordinary workspace run alone does not prove hardware decode.

The CLI probe accepts **one complete Annex-B H.264 access unit**, including SPS/PPS. It is not a container demuxer or a complete video player. `--verify-gradient` expects the fixture's first luma sample to be 64. The generator now correctly marks its stream as constrained baseline and selects a level for its macroblock dimensions, matching its actual coding tools and the driver's supported H.264 profile.

The hardware lifecycle test performs 2,000 decode/flush/export cycles after warming caches, checks descriptor counts and bounded RSS, holds four GPU surfaces to exercise backpressure, and verifies a frame remains valid after decoder destruction. On the recorded run, descriptor count stayed 5 → 5 and RSS changed 31,496 → 31,580 KiB. This is an isolated decoder test, not a long-running NewPipe memory result.

The compositor test preserves both NV12 planes, waits for release after replacing a frame, and closes a window with the last GPU frame attached. Its source surface is retained until the compositor releases it. DMA-BUF v3 is bound deliberately for explicit format/modifier advertisements; the v4 feedback protocol is not implemented. Unsupported format/modifier pairs are rejected before import. Ordinary submit acknowledgments are not sufficient to recycle a decoder surface.

## NewPipe component selection and output surface

The captured NewPipe run selected `c2.android.avc.decoder` (`video/avc`). Its current BLAST consumer receives 1280×720 buffers in Android HAL format 4 (`RGB_565`) and presents them through software `wl_shm`. The app therefore requests an H.264 decoder through the Android Codec2 surface path; the independent VA-API fixture result does not affect that running pipeline.

A Codec2 replacement has to return the exact work and graphic-buffer ownership the framework expects. Directly presenting a host DMA-BUF as a separate Wayland video layer would still require the Codec2 work to complete, the Android output surface not to cover or discard that frame, and format/flush/reconfigure behavior to remain correct. Returning a YUV frame through the existing RGB565 memfd allocator would reintroduce a CPU copy and conversion. Component advertisement must only happen when that full contract is implemented; a name in the codec list alone is not a working decoder.

## Next integration boundary

The existing shim's `codec2.rs` builds the real Android `C2PlatformComponentStore`, which currently opens `libcodec2_soft_*` plugins. Advertising a hardware codec name without implementing its component would be incorrect. The next adapter must connect complete compressed access units and codec configuration to the host decoder while preserving Android work completion, timestamps, flush/EOS, surface association, color metadata and ownership. Its packet queues and returned frames must remain bounded. Per-frame presentation pacing/audio synchronization and resolution changes need real playback coverage.

The current decoder can request H.264, HEVC, VP8, VP9 and AV1 from FFmpeg, but only H.264 has been tested with compressed input. Driver profile discovery does not establish end-to-end support for the other codecs. Color-space/range metadata and HDR handling are not yet carried into compositor presentation. Host Vulkan, GLES and multi-GPU selection must be validated independently of media decode.

References: [FFmpeg hardware decode example](https://github.com/FFmpeg/FFmpeg/blob/master/doc/examples/hw_decode.c), [VA-API core synchronization/export](https://intel.github.io/libva/group__api__core.html), [DRM PRIME surface descriptors](https://github.com/intel/libva/blob/master/va/va_drmcommon.h).
