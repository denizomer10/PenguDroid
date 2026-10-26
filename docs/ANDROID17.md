# Android 17 migration target

The runtime target is **Android 17 / API 37 only**. Existing APKs keep their
declared minSdk/targetSdk; the platform level is not substituted for either.
The Android 17 runtime boots and passes core framework and BLAST lifecycle
probes. Window/media integration remains incomplete. API-33 artifacts are
historical regression fixtures, not a window/input fallback. See STATUS.md
for the latest handoff; earlier checkpoints below record migration history.

## Actual API-37 runtime bringup

The official x86_64 GSI `CP2A.260605.012` is downloaded and verified locally.
SHA-256: `3da2ca5599d304c4d92fe8c72f31d035a59db57c93ce073740ed4015ee034a8e`.
Source: [Android 17 GSI releases](https://developer.android.com/topic/generic-system-image/releases).
Archive metadata is `.toolchain/android17/source.json`; binaries stay outside
Git. The separate tree is `.toolchain/sysroot-android17`; its own build.prop
reports SDK 37, SDK_FULL 37.0 and release 17. No API-level spoofing was used.

38 APEX packages are expanded, including compressed CAPEX and an EROFS package.
`tools/art/unpack-apex.py` handles both payload filesystems without mounting
anything. The EROFS utility was extracted locally from a distribution package;
no system package or kernel configuration was changed. Fresh ext4 and EROFS
extraction smoke checks passed (`/tmp/pd-android17/extractor-test.log`).

Verified on the real API-37 ART with CheckJNI:

- `JNI_CreateJavaVM` returns JNI_OK. 174 framework native groups and two HWUI
  native groups register. SystemProperties registration now precedes groups
  whose class initializers query it.
- ServiceManager legacy binder replies coexist with the new typed
  Service/ServiceWithMetadata union. Generated codes are `[1, 3, 2, 4]`.
- Settings scalar/missing/list replies pass Java Binder/Parcel validation.
- Audio-focus AIDL request/current/abandon pass with the API-37 nullable token
  parameter and generated transaction codes `[82, 83, 84, 85]`.
- WindowLayout's requested-visible-types signature computes 720x1280.
- InputDevice.Builder, the new native KeyCharacterMap String8/remapping format,
  and generated input codes `[2, 3, 0, 4, 5, 6, 7, 39]` pass six Unicode cases.
  Zero marks the removed isInputDeviceEnabled transaction; API 37 stores that
  state on InputDevice itself.
- SystemFeaturesCache and ApplicationSharedMemory initialize and pass a real
  native shared-memory write/read roundtrip. This mapping is currently local
  to each hosted guest, not a shared system-server mapping across all apps.

Bionic additions were validated with an independent C fixture: real queued
signal payloads through sigwaitinfo/64, timed-wait errno/timeout behavior,
canary-protected buffers, and stack-guard regeneration in a forked child that
never returns through old protected frames. `__stack_chk_guard` is now a scalar
initialized from host TLS. The API-35+ property reload hook enables the hosted
app-compat property view. The 16 KiB app-compat linker switch is unnecessary on
this 4 KiB host; larger-page host emulation remains unsupported.

NewPipe on API 37 now creates its Application and reaches Activity.onCreate.
The image's framework bootclasspath metadata supplies newly split framework
jars. Default compat changes are read from the image XML against the APK's
actual targetSdk (35 here); 143 changes are disabled, including the optional
virtual-gamepad override. Per-package user overrides are not implemented.
The modern SurfaceComposer status/method mapping and 224-byte vsync packet
replace the incompatible legacy paths for the covered API-37 calls.

HWUI's EGL_TELEMETRY_HINT_ANDROID (0x3570) is consumed by the host boundary:
this renderer does not collect Android GPU stats. Other context attributes
continue to receive host validation. The real GPU regression accepts this
HWUI hint and still rejects an unknown attribute.

**API-37 NewPipe UI now renders; playback remains unverified.** Input-channel and SurfaceControl
reply contracts were subsequently corrected (see STATUS.md). The real native
BLAST create/update/destroy/recreate gate now passes with CheckJNI, after mapping
the C++ queue handle to separate Rust state and installing the framework's
ServiceManager ABI before construction. This headless probe does not exercise
frame presentation or transaction/release callbacks. A subsequent bounded
Wayland run passed native BLAST update and hwui's physical-display discovery
after correcting API-37 static/dynamic display and composition-preference
parcelables. The native `ADisplay` API now has a CheckJNI regression verifying
the synthetic internal 720x1280/60 Hz SDR display. The subsequent gralloc abort
came from the optional `ANativeWindow_tryAllocateBuffers` preallocation hint.
The shim now declines that hint, as allowed by the NDK contract; host EGL renders
into its own FBOs. A real 20-second Wayland run remained alive and reached UI
frame counter 550, with 52 distinct hashes among 55 sampled images. See
`screenshots/newpipe-android17.png` and STATUS.md. This does not implement
mandatory API-37 GraphicBuffer allocation: LTO-inlined mapper/backend accesses
still require an ABI-matched backend. No video decode/playback success is claimed.

Other observed limits: boot images have a read-barrier mismatch; ART successfully
falls back to imageless execution. Aconfig metadata under /metadata/aconfig is
not provisioned, Binder death/threadpool semantics remain incomplete, and some
service calls still return incomplete replies. Neither API-37 video playback
nor broad backward application compatibility has been verified.

Reproduction (build runtime shared libraries before tests):

```sh
cargo build --workspace --offline
cargo test -p pengudroid-cli --test art_framework --offline
# Includes dedicated API-37 core-services and BLAST lifecycle gates.
# Explicit full API-37 gate:
PENGUDROID_TEST_SYSROOT="$PWD/.toolchain/sysroot-android17" cargo test -p pengudroid-cli --test art_framework real_art_keyboard_and_local_window_layout --offline
pengudroid launch NewPipe_v0.29.1.apk --sysroot .toolchain/sysroot-android17
```

The earlier workspace checkpoint had **251 passing tests**, with both installed
runtime fixtures exercised. The API-37 lifecycle probe now passes separately;
this is not a claim that API-37 window/media integration works. See STATUS.md
for the current validation scope.

Logs are under `/tmp/pd-android17/`: `shared-framework-tests.log`,
`full-framework17-test.log`, `guard-abi.log`, `properties-abi.log`,
`newpipe-egl17.log`, and workspace/build logs. Historical pre-bringup text below
records the first transaction-table change, not the final migration state.

## Reference review (2026-10-26)

The AOSP manifest is a repository index, not the platform implementation. The
inspected android-latest-release manifest selects android17-release. Its ART,
Bionic, frameworks/base, frameworks/native, frameworks/av and hardware/interfaces
projects must be tracked together, with pinned revisions for reproducibility.

The linked Rust Binder is a **Linux kernel driver**, using kernel memory,
locking, task and file APIs. It does not establish that Android framework or
media services have been rewritten in Rust. PenguDroid's host service broker and
Bionic interception layer run in userspace and have different responsibilities.
We use published interfaces and behavior as references and write our own code;
no driver or service implementation from these references is vendored here.

Inspected source revisions:

| Reference | Pinned revision | What was inspected |
| --- | --- | --- |
| platform/manifest | ad156f32caaa06dae91c02d443f6a8fe210eaa54 | default.xml and project boundaries |
| kernel/common android-mainline | 721ad43cbf3c182a07edf6a2a8127c587313f6a7 | Rust driver entry, defs, public Binder UAPI |
| frameworks/base android17-release | 94b4c163b7dfe5ce3607f7bb8456f9573f7de57d | IAudioService AIDL |
| hardware/interfaces android17-release | 0162af698935100a590b7359581ac8b1b80693e5 | media/c2 AIDL IComponentStore |

Source links: [manifest](https://android.googlesource.com/platform/manifest/+/ad156f32caaa06dae91c02d443f6a8fe210eaa54/default.xml),
[Rust Binder](https://android.googlesource.com/kernel/common/+/721ad43cbf3c182a07edf6a2a8127c587313f6a7/drivers/android/binder/rust_binder_main.rs),
[Binder UAPI](https://android.googlesource.com/kernel/common/+/721ad43cbf3c182a07edf6a2a8127c587313f6a7/include/uapi/linux/android/binder.h),
[IAudioService](https://android.googlesource.com/platform/frameworks/base/+/94b4c163b7dfe5ce3607f7bb8456f9573f7de57d/media/java/android/media/IAudioService.aidl),
[Codec2 IComponentStore](https://android.googlesource.com/platform/hardware/interfaces/+/0162af698935100a590b7359581ac8b1b80693e5/media/c2/aidl/android/hardware/media/c2/IComponentStore.aidl).

## First implemented boundary: audio-focus transaction dispatch

Hosted audio focus now uses semantic operations. During ART binding, the host
reads all four TRANSACTION constants from the loaded IAudioService Stub and
installs the validated table atomically. Duplicate, zero and reserved codes are
rejected. A missing table fails binding with a diagnostic rather than using
old method numbers against a different framework. The historical local mapping
remains the native-only fallback. APK targetSdk does not select Binder wire ABI.

The Android 17 source adds focusEnvToken arguments to request/abandon/current
focus. The current host policy is still per-process: token-specific environments,
listener callbacks, shared arbitration and ducking are not implemented. The
Java AIDL focus probe now exercises both API-33 and API-37 frameworks. Its
nullable token test does not establish token-scoped arbitration or API-37 playback.

Validation: workspace build and **248 tests passed**, including the existing
real ART/CheckJNI audio AIDL probe and a remapped-table regression. Runtime
verification is API 33, not API 37. Logs: `/tmp/pd-android17/{build,tests}.log`.

## Remaining migration gates

1. Finish the prepared API-37 runtime environment: resolve boot-image/GC
   compatibility, provision Aconfig storage, and retain the API-33 regression
   fixture. The API-37 image acquisition, APEX extraction and basic ART boot
   steps are complete.
2. Extend the passing core CheckJNI probes to actual window/input and BLAST
   lifecycles. Version private parcel layouts, object/vtable offsets and
   service operations by the **loaded framework**, using generated constants
   where possible. Core native registration is already verified.
3. Port input, window/activity/configuration, settings, package and audio service
   contracts; test old and new APK targetSdk behavior independently.
4. Implement the Codec2 stable AIDL adapter and its ownership/error/version
   contracts. The inspected source contains a VINTF-stable AIDL ComponentStore;
   PenguDroid currently hosts HIDL objects with measured legacy C++ layouts.
   Do not reinterpret a new AIDL object as one of those HIDL objects.
5. Connect compressed packets to the existing host GPU decoder, preserve frame
   leases, timestamps, flush/EOS, format changes and color metadata; validate
   audio/video pacing with real playback. See HARDWARE_VIDEO.md.
6. Expand the compatibility matrix across API-33 and API-37 runtimes and APKs
   with older/newer minSdk/targetSdk, including native games, arm64, input,
   resize, pause/seek, exit and relaunch. No all-app compatibility claim yet.

Kernel Binder protocol version 8 (LP64) is distinct from API level 37 and private
AIDL transaction numbering. A Rust driver does not remove those userspace ABI
requirements. No kernel module replacement is required for this first step.
