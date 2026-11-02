# Intel Edge AOSP BSP → PenguDroid compatibility report

Scope: the [edge-aosp-bsp](https://github.com/edge-aosp-bsp) Intel platform
(Android 16) and [DigitalisX64/digitalis](https://github.com/DigitalisX64/digitalis)
(ARM64→x86-64 native bridge), compared against PenguDroid's Android 17 / API 37
Linux-hosted runtime.

Sources were read, not inferred. The BSP manifest is pinned to
`refs/tags/android-16.0.0_r2` (`manifest/default.xml`) and its project list is
`manifest/include/bsp-edge.xml`. Repositories examined at these revisions:

| Repository | Revision examined | Why |
| --- | --- | --- |
| `minigbm` | `upstream_main` (manifest pin) | gralloc buffer layout and Intel backends |
| `external-mesa` | `main` (manifest pin) | Android EGL/WSI and ANV imports |
| `drm-hwcomposer` | `main` | HWC2/HWC3 + DRM composition |
| `MemTrack` | default | legacy memtrack HAL |
| `powerhal` | default | legacy power HAL |
| `vendor-hardware-interfaces` | `a15/dev` default | Intel AIDL/HIDL interface versions |

## 1. Summary

The BSP and PenguDroid solve different problems and cannot share most code:

* The BSP is a **bare-metal Android 16 product** for Intel boards. It owns the
  kernel, DRM/KMS, firmware, HAL services and hardware drivers.
* PenguDroid is a **userspace compatibility runtime on a normal Linux desktop**.
  It has no root, no init/zygote, no kernel binder, no DRM master and no HAL
  service manager; it hosts Android libraries behind a shim and brokers services
  through a resident daemon (`docs/ARCHITECTURE.md`).

What *is* reusable is the **contract layer**: buffer geometry, HAL interface
versions and the assumptions Android's own libraries make about them. That is
where this pass delivered a tested change (§7).

| BSP area | Direct reuse | What PenguDroid should take |
| --- | --- | --- |
| minigbm gralloc handle | No (HIDL service) | Plane/stride/modifier model for the shim's `AHardwareBuffer` |
| Mesa (iris/ANV, EGL) | No | Confirmation that host EGL/Vulkan is the right backend; already implemented |
| drm-hwcomposer / IA-HWC | No | Nothing; desktop Wayland owns composition |
| MemTrack | No | Accounting idea (`/proc/<pid>/smaps`, memfd sizes), not the code |
| powerhal | No | Interface shape only; host cgroup v2 is the portable analogue |
| Intel media (MediaSDK/VA-API/oneVPL) | No | Host VA-API as a future codec2 backend, not the Intel HAL |
| Boot/firmware (coreboot, kernelflinger, Trusty, IKGT) | No | Out of scope |

## 2. PenguDroid's current contract

From `docs/ARCHITECTURE.md` and `docs/LIMITATIONS.md`:

* `pengudroid-daemon` owns Wayland surfaces, the userspace binder broker,
  PipeWire audio, properties and ashmem/memfd (`docs/ARCHITECTURE.md` §1-2).
* Guests are ordinary processes under `LD_PRELOAD` Bionic shim.
* Host GPU is reached by `pengudroid-gleshim`: it `dlopen`s the host
  `libEGL.so.1` / `libGLESv2.so.2` and presents `eglSwapBuffers` as a dma-buf
  (`crates/pengudroid-gleshim/src/lib.rs`, `crates/pengudroid-hostgpu/src/lib.rs`).
* Vulkan already maps the Bionic soname onto the host loader
  (`docs/LIMITATIONS.md`, "Vulkan works").
* The remaining graphics gap is stated plainly: "Still missing:
  `gralloc`/`AHardwareBuffer` compatibility (apps that allocate GPU buffers
  through it fail), hardware composition, and GLES coverage beyond the exported
  core functions" (`docs/LIMITATIONS.md`).

The shim therefore emulates gralloc itself: `crates/pengudroid-shim/src/ahardwarebuffer.rs`
backs buffers with memfd, and interposes the `Gralloc4Mapper` constructor because
API-37 `libui` probes Gralloc5, then Gralloc4, and only falls back to
Gralloc3/2 when `android_get_device_api_level() <= 35` (`docs/STATUS.md`,
"gralloc mapper is now provided").

## 3. What the BSP actually contains

### 3.1 gralloc: minigbm

* `cros_gralloc/cros_gralloc_handle.h` defines the allocation as one packed
  structure: `fds[5]`, per-plane `strides/offsets/sizes` for up to 4 planes, DRM
  `format`, `format_modifier`, Android `usage`, `pixel_stride`, `num_planes` and
  `total_size`.
* `cros_gralloc/gralloc4/Android.bp` builds the HIDL
  `android.hardware.graphics.mapper@4.0` and `allocator@4.0` services, with Intel
  backends in `i915.c`, `xe.c` and `intel_device.c`.
* The handle is a **descriptor of planes**, not a single pointer. Any consumer
  that gets only "base pointer + width*4" is wrong for every YUV buffer.

This is the part worth copying conceptually: PenguDroid's memfd buffers can carry
the same three-plane description even without a HAL service.

### 3.2 Mesa

* `src/egl/drivers/dri2/platform_android.c` drives a surface through
  `ANativeWindow_dequeueBuffer` / `ANativeWindow_queueBuffer` and reads buffer
  geometry through `u_gralloc_get_buffer_basic_info` / `color_info`.
* `src/intel/vulkan/anv_android.c` imports an `AHardwareBuffer` with
  `AHardwareBuffer_describe` + `AHardwareBuffer_getNativeHandle`, then resolves
  tiling through `u_gralloc`.

Both are interfaces PenguDroid already satisfies at the host boundary: the shim
provides `ANativeWindowBuffer`/`AHardwareBuffer`, and the host Mesa driver is
loaded directly. Vendoring the BSP's Mesa would duplicate the host driver and
break the "use the host GPU" design. The actionable part is that the shim's
`AHardwareBuffer` must describe planes the way these consumers expect.

### 3.3 Composition

* `drm-hwcomposer/Android.bp` builds `hwcomposer.drm` / `hwcomposer.drm_minigbm`
  and the `hwc2_device` implementation; `hwc3-server/` adds the composer3
  service, translating the AIDL composer API onto that HWC2 device.
* `IA-Hardware-Composer` is the older Intel HWC.

Both require DRM master and KMS planes. On a desktop session the Wayland
compositor owns the display, so none of this is portable; PenguDroid's
`SurfaceComposer` stub plus the Wayland broker is the correct architecture.

### 3.4 Memory and power

* `MemTrack/mali/memtrack_intel.c` implements the **legacy**
  `hw_module` memtrack HAL (`hardware/memtrack.h`) for Mali/ION/zram, built with
  `Android.mk`.
* `powerhal/power.cpp` includes `hardware/power.h` (HIDL-era) and drives cgroups.

Neither matches Android 17's clients: the API-37 sysroot ships
`android.hardware.memtrack-V1-ndk.so` and `android.hardware.power-V7-ndk.so`
(AIDL). The BSP's interface definitions are also older: Intel's
`vendor-hardware-interfaces/power/aidl` is a frozen AIDL interface up to **v5**,
and `thermal/aidl` up to **v2**, while 17 ships power V7.

The portable analogue is host-side accounting: memfd/ashmem sizes and
`/proc/<pid>/smaps` for memtrack, `/sys/fs/cgroup` v2 for power. That is a
future, optional stub, not a port.

### 3.5 Media and the rest

`MediaSDK`, `media-driver`, `libva`, `libvpl`, `vpl-gpu-rt`, `libstagefrighthw`
and `mediasdk-omx` are Intel GPU/QuickSync integrations. They only make sense
against Intel kernel drivers and Android HAL service plumbing. PenguDroid's
codec2 path is intentionally software (`docs/HARDWARE_VIDEO.md`); a host VA-API
backend would be a new, separate design, not a copy of these modules.

Boot firmware (`coreboot`, `kernelflinger`, `efiwrapper`, Trusty, IKGT),
`kernel-modules`, `libdmi`, USB/Wi-Fi/Bluetooth HALs and sensors are hardware
or firmware specific and out of scope for a Linux-hosted runtime.

## 4. Android 16 → 17 compatibility requirements

Measured against the API-37 sysroot in `.toolchain/sysroot-android17/system/lib64`:

| Contract | Android 16 BSP | Android 17 / API 37 sysroot | Consequence |
| --- | --- | --- | --- |
| gralloc | HIDL mapper/allocator 4.0 | HIDL mapper 2.0/3.0/4.0 **and** `allocator-V2-ndk` + `graphics.common-V7` still shipped | HIDL still loads, but new HAL work must target AIDL; `libui` in 37 only probes Gralloc5 then Gralloc4 |
| memtrack | legacy `hw_module` | `android.hardware.memtrack-V1-ndk.so` | the BSP module cannot satisfy a 17 client |
| power | HIDL header + cgroups | `android.hardware.power-V7-ndk.so` | Intel's frozen v5 AIDL is a version mismatch |
| GLES | Mesa GLES | `libEGL_angle.so`, `libGLESv1_CM_angle.so`, `libGLESv2_angle.so` present | ANGLE is a first-class GLES path in 17; do not bake a Mesa-GLES assumption into the shim |
| HIDL | primary transport | `libhidlbase`, `libhidltransport`, all graphics HIDL libs present | HIDL is deprecated but not removed; "won't load at all" would be wrong |
| build | several `Android.mk` modules (MemTrack, powerhal) | Soong/Blueprint | any port would need Android.bp conversion before it could even build |

The API-37 `GraphicBuffer` probe order is the sharpest compatibility rule:
Gralloc3/Gralloc2 are gated on `android_get_device_api_level() <= 35`
(see `docs/STATUS.md`). An Android-16-era HAL that only exposes gralloc 3.0
cannot be reached from 17 at all.

## 5. Reuse matrix

| Component | Class | Decision |
| --- | --- | --- |
| `minigbm` handle/plane model | contract | **Reuse the model** in the shim (§7) |
| `minigbm` HIDL service + Intel backends | hardware service | Not applicable |
| Mesa Android WSI expectations | contract | Already met by host Mesa; keep |
| Mesa Intel drivers | host driver | Use distro Mesa, do not vendor |
| `drm-hwcomposer`, `IA-Hardware-Composer` | display ownership | Not applicable (no DRM master) |
| `MemTrack` | legacy HAL | Reference only; implement honest host accounting if needed |
| `powerhal` | legacy HAL | Reference only; host cgroup v2 later |
| Intel power/thermal AIDL | versioned interfaces | Port definitions only if a v7-compatible adapter is written |
| Intel media stack | hardware codecs | Separate host VA-API phase |
| Boot/firmware/kernel | out of scope | Excluded |

## 6. Integration roadmap

1. **Gralloc geometry in the shim (done here).** Format-aware plane count,
   offsets, row/pixel strides and allocation size for RGBA/RGBX/RGB565/F16 and
   NV12/NV21/YV12; `AHardwareBuffer_isSupported` now only advertises modelled
   formats.
2. **dma-buf-backed `AHardwareBuffer`.** Allocate through the host GPU
   (`pengudroid-hostgpu` already exports dma-buf), record the same plane fields
   minigbm records, and import through `eglCreateImageKHR` / Vulkan external
   memory. Acceptance: allocate NV12 and RGBA, export dma-buf, import into host
   EGL, render one frame, read it back.
3. **BufferQueue/composer metadata through the broker.** Keep the Wayland
   presentation path; add the AIDL-shaped metadata (dataspace, HDR, fence) that
   `libgui` clients read, without pretending to be a real HWC.
4. **Honest memtrack/power replies.** Serve `android.hardware.memtrack.IMemtrack`
   from `/proc/<pid>/smaps` + memfd sizes, and power hints from cgroup v2. No
   Intel code, but the same information model.
5. **Media.** Evaluate host VA-API behind codec2 as a separate project; do not
   port MediaSDK/OMX.

Out of scope permanently for this architecture: HWC/DRM composition, kernel
modules, firmware and Intel sensor/USB/Wi-Fi HALs.

## 7. Change delivered and verified

`crates/pengudroid-shim/src/ahardwarebuffer.rs` now derives a single
gralloc-style `plane_layout` and uses it everywhere a buffer's geometry is
reported or sized:

* `AHardwareBuffer_allocate` / `bytes_for` size 4:2:0 buffers as 1.5 bytes per
  pixel instead of the previous 4-byte fallback, covering every plane;
* `AHardwareBuffer_lockPlanes` returns 3 planes for NV12/NV21/YV12 with the
  correct offsets, row strides and pixel strides, instead of always one plane at
  `stride * 4`;
* `lockYCbCr` uses the same layout, so `cstride` is the chroma row stride in
  bytes (`stride` for interleaved NV12/NV21, `stride/2` for planar YV12) rather
  than a hard-coded `stride/2` for every format;
* `GraphicBufferMapper::lock` reports the format's real bytes per pixel instead
  of always 4;
* `AHardwareBuffer_isSupported` reports 1 only for the seven formats with real
  geometry.

Verification:

```sh
cargo test -p pengudroid-shim --offline --lib     # 148 passed
cargo test -p pengudroid-cli --offline            # 80 + 7 + 2 + 3 + 2 passed
```

The new tests cover packed RGB, layers, NV12/NV21 plane swapping, YV12 U/V
order, allocation coverage for every plane, and the `isSupported` filtering.

## 8. Digitalis / Berberis (ARM64 → x86-64)

The bridge work is tracked separately in `docs/NATIVE_BRIDGE.md` and
`docs/STATUS.md`. The BSP does not carry a different translator: Digitalis is
the upstream project PenguDroid builds (`libberberis_arm64.so`,
`libberberis_proxy_libc.so`). The BSP's contribution here is none; the relevant
compatibility facts are the same guest linker config, TLS adapter and proxy
loader documented in `STATUS.md`. No BSP change is required for the ARM64 path.

## 9. Open questions

* Whether a dma-buf `AHardwareBuffer` can share the host GPU's modifier with the
  Wayland compositor without a gralloc HAL (minigbm's `format_modifier` is the
  field to carry).
* Whether API-37's `libui` ever calls the mapper's `createDescriptor`, which the
  shim currently leaves as a no-op.
* Whether a hosted memtrack service is worth implementing for apps that only use
  it for diagnostics; no shipped app has been measured failing on it yet.
