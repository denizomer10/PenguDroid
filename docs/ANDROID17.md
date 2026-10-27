# Android 17 / API 37 x86_64

## Runtime contract

**Android 17 / API 37 on x86_64 is the only runtime target.** There is no
API-33/36 runtime selection, ABI retry, or old framework test fixture. An
incompatible runtime must be rejected, not made to look compatible by changing
its advertised SDK. The expected local tree is `.toolchain/sysroot-android17`;
`PENGUDROID_ART_SYSROOT`/`--sysroot` may name another tree satisfying the same
contract.

An APK's declared `minSdk` and `targetSdk` are application metadata, not a
runtime selection mechanism. Android 17 itself has target-SDK-dependent
application behavior. PenguDroid keeps that metadata and reads the selected
image's compat XML against it; it does not substitute the platform level for
an application's declared target. Missing target metadata defaults to 37.

The product goals are stable NewPipe playback and interaction without unbounded
memory/resource growth, GPU graphics and hardware video decode across Intel,
AMD and NVIDIA, and Beach Buggy Racing 2. None is established merely by building
the workspace or reporting a GPU capability.

## Current checkout: 2026-10-27 (runtime restored)

`.toolchain/sysroot-android17` now exists in this checkout. It was produced by
`tools/prepare-android17.py` from the archive below after verifying its SHA-256,
and is validated on every use: SDK 37 / release 17 / build `CP2A.260605.012`, an
actual ELF64 x86_64 `libart.so`, and per-file hashes in
`pengudroid-runtime.json`. All **three** real API-37 CheckJNI gates pass against
it: core services/main-thread attach, BLAST lifecycle, and media discovery
without a hosted HIDL Codec2 backend.

`.toolchain/sysroot` (SDK 33) is still present and is **not** a target and not a
fixture; the CLI rejects it before ART loads.

Two extraction details matter for anyone reproducing this:

* The GSI stores `/apex` as an **empty directory** standing in for a mount point,
  so absolute `/apex/...` links did not resolve inside the extracted tree and
  `system/lib64/libc.so` appeared missing. `tools/art/sysroot-relink.py` now
  replaces exactly that empty directory with `system/apex` and refuses to touch
  a populated one.
* `-Ximage:` is a *location*, not a file: ART appends `<arch>/<basename>` to its
  directory. `framework/boot.art` therefore resolves to the real
  `framework/x86_64/boot.art`; passing `framework/x86_64/boot.art` makes ART look
  for `framework/x86_64/x86_64/boot.art` and miss.

**Open at this checkpoint:** the boot image does not load — ART reports
`ValidateOatFile found read barrier state mismatch (oat file: false, runtime:
true)` and falls back to imageless running. All 154 `-X`/`-XX` options exported
by this `libart.so` were enumerated and none controls read barriers, so this is
not currently configurable. Imageless mode works (NewPipe runs) but costs
startup time and boot-image sharing. See [STATUS.md](STATUS.md) for the measured
gaps.

An earlier checkpoint recorded real API-37 bringup using the official x86_64
GSI `CP2A.260605.012`, SHA-256
`3da2ca5599d304c4d92fe8c72f31d035a59db57c93ce073740ed4015ee034a8e`.
That checkpoint was historical; this checkout has since restored the same
verified archive and rerun the gates above. Source:
[Android GSI releases](https://developer.android.com/topic/generic-system-image/releases).

### Implemented hard-lock boundaries

- Shared platform identity is API 37 / Android 17 / x86_64. Shim and daemon
  reject platform-property downgrades, 32-bit ABI enabling and appcompat aliases
  that would override this identity. Old persisted daemon properties cannot
  replace it. Ordinary configuration remains writable.
- Sysroot detection checks SDK metadata and the actual ART ELF class,
  endianness and machine. The framework boot classpath must come from valid
  `bootclasspath.pb` metadata; no hardcoded framework-jar fallback is used.
- Keyboard parcels always use the API-37 String8/remapping representation and
  `InputDevice.Builder`. Activity launch, WindowLayout and audio-focus probes
  use their API-37 signatures only.
- ServiceManager, application, input, audio-focus, media-session/metrics, user
  and thermal transaction tables are installed from the loaded framework's
  generated Stub constants. Uninstalled tables do not substitute old numbers.
  ServiceManager requires both binder and Service-union methods because both
  are present on API 37; these are not alternate platform selections.
- SurfaceComposer uses the covered `android.gui.ISurfaceComposer` AIDL
  contracts only. The display event layout is 224 bytes and requires API 37.
  Unsupported revised contracts return `UNKNOWN_TRANSACTION`, not fake success.
- `ContentProviderHolder` includes the API-37 third trailing flag,
  `noReleaseNeededIfUnstable`. Resized callbacks carry a framework-marshalled
  `WindowRelayoutResult` followed by the five API-37 scalar arguments. The
  CheckJNI window probe now decodes that payload with the framework's CREATOR.
- BLAST obtains the native C buffer through
  `GraphicBuffer::getNativeBuffer() const`; the old private C++ handle offset
  was removed. `ANativeWindowBuffer` follows the pinned native C declaration.
  Unknown allocator handles are rejected before payload dereference. A host
  C++ fixture exercises a deliberately unrelated GraphicBuffer layout, reference
  counting, imports, and mapping cleanup. It is not a target-runtime test.

### HIDL discovery corrected on the real runtime

The HIDL manager returned by `get()` is the manager itself, under one canonical
handle for all its version aliases. Its `IBase.interfaceChain` code is
`0x0f43484e`, **not** IBinder's `INTERFACE_TRANSACTION`; the descriptor and ping
codes are `0x0f445343` and `0x0f504e47`. The reply is HIDL Status followed by
pointer-backed LP64 string/vector records, not inline strings. Empty vectors
retain the null child record without adding its offset. `Transport` occupies
four bytes after Parcel padding, and HIDL binders omit the AIDL stability word.

Code 5 lists registered instances; code 13 lists manifest-declared ones.
PenguDroid has no lazy hosted HALs, so both use its actual hosted set, not the
unstartable services listed in the GSI. An unavailable OMX/Codec2 service is a
successful typed absence (EMPTY/null/empty vector). Returning a transport error
for an understood manifest query makes `Codec2Client` retry forever. Unsupported
methods and wrong interface tokens still return `UNKNOWN_TRANSACTION`.

Source pins (source contracts, not an attestation of the GSI's entire build):

- libhwbinder `2f5282448223b97c541181a5b94a05c2ac17ff17`:
  [reserved transaction codes](https://android.googlesource.com/platform/system/libhwbinder/+/2f5282448223b97c541181a5b94a05c2ac17ff17/include/hwbinder/IBinder.h),
  [Parcel padding/null/embedded-buffer rules](https://android.googlesource.com/platform/system/libhwbinder/+/2f5282448223b97c541181a5b94a05c2ac17ff17/Parcel.cpp).
- libhidl `061055fc4dad78e5d25a77d9217001794656f16b`:
  [manager 1.2](https://android.googlesource.com/platform/system/libhidl/+/061055fc4dad78e5d25a77d9217001794656f16b/transport/manager/1.2/IServiceManager.hal).
- hwservicemanager `60271965cf6b055ed82b2e1ae0bf74b2c34a6af2`:
  [manager lookup and list semantics](https://android.googlesource.com/platform/system/hwservicemanager/+/60271965cf6b055ed82b2e1ae0bf74b2c34a6af2/ServiceManager.cpp).
- frameworks/av `475269ec44acec82792be6b01fb8e16357d4d2c8`:
  [Codec2Client manifest discovery and retry loop](https://android.googlesource.com/platform/frameworks/av/+/475269ec44acec82792be6b01fb8e16357d4d2c8/media/codec2/hal/client/client.cpp).

32 verified source/license blobs and provenance are local under
`.toolchain/references/hidl-android17/`. These HIDL interface versions coexist
on Android 17; none selects an older Android runtime. The new CheckJNI gate
queries MediaCodecList 16 times without an APK or compositor. The default
runtime currently finds a real APEX AAC codec, but no usable HIDL video store.

### Still open — do not call the migration complete

1. Stable NewPipe playback. The restored runtime passes all three CheckJNI
   gates and a 35.18 s UI/playback-attempt run without the HIDL fatal. APEX AAC
   produces output, but AudioTrack initialization fails. Implement a closed,
   descriptor-checked API-37 audio JNI adapter; the bypassed legacy name-only
   hook has an incompatible `native_setup` and must not be reenabled unchanged.
2. Replace/audit the remaining private native ABI assumptions. BLAST retains
   RefBase virtual-base/refcount handling, opaque allocation capacities and
   libc++ representation assumptions. Codec2 still fabricates C++ HIDL callback
   objects and accesses private Parcel layouts. The fabricated decoder-list and
   component-handle fallback has been removed: missing services are not
   advertised, understood discovery returns typed absence, and unsupported or
   failed component dispatch rejects explicitly. A frozen HIDL HAL version alone
   does not validate those implementation-private C++ layouts.
3. Implement and validate the Codec2 adapter and bounded compressed-packet /
   GPU-frame ownership pipeline. Preserve timestamps, flush/EOS, reconfiguration,
   color metadata and A/V pacing. The host decoder is not yet NewPipe hardware
   decoding. See [HARDWARE_VIDEO.md](HARDWARE_VIDEO.md).
4. Measure RSS/PSS, mappings, descriptors and retained native/JNI objects during
   sustained playback, seek, resize, minimize/restore, exit and repeated reopen.
   Passing isolated mapping tests is not proof of leak-free application playback.
5. Validate hardware paths separately on Intel, AMD and NVIDIA. This host reports
   Intel UHD 630, Mesa GLES 3.2 and Intel iHD VA-API VLD profiles. This is capability
   discovery, not an end-to-end decode test. AMD and NVIDIA remain unvalidated
   here; NVDEC and Android Vulkan WSI are separate work.
6. Supply a working ARM64 execution path for the available BBR2 package and
   exercise its framework/graphics/input/audio requirements. Do not infer that
   booting system_server or adopting another OS is necessary from metadata alone.

## Reference use

Available local source trees inspected in this checkout:

| Source | Useful boundary | Not evidence of |
| --- | --- | --- |
| `.toolchain/manifest-refs_heads_android17-release/default.xml` | AOSP project index | Framework implementation or extracted runtime binaries |
| `.toolchain/native-refs_heads_main-libs-binder-rust` | Userspace Rust Binder interfaces | A complete Android runtime or identical private transaction numbers |
| `.toolchain/common-30c0eaeef78d5c19f687ad8418c27b76353ab261/include/uapi/linux/android/binder.h` | Actual LP64 Binder ioctl/structure regression | Android framework AIDL method order |
| `.toolchain/common-refs_heads_android-mainline-drivers-android-binder` | Kernel Binder driver design | A userspace service implementation |
| `.toolchain/gbos-x86-main/README.md`, `HANDOFF.md`, `kernel/`, `isa-emu/` | PC hardware bringup, memfd/ashmem constraints, CPU instruction gaps | An ARM64 translator or a no-boot compatibility layer |

Googlebook/gbos-x86 boots a complete Android OS using its own kernel, mounts and
Android init. PenguDroid does not import that boot/container architecture. Its
instruction emulator handles missing **x86 SIMD instructions on x86**; it is not
ARM64-to-x86_64 translation. Its hardware findings are references, not validation
of PenguDroid on the same drivers.

Pinned official source contracts used for this migration:

- frameworks/base `94b4c163b7dfe5ce3607f7bb8456f9573f7de57d`:
  [IWindow](https://android.googlesource.com/platform/frameworks/base/+/94b4c163b7dfe5ce3607f7bb8456f9573f7de57d/core/java/android/view/IWindow.aidl),
  [ContentProviderHolder](https://android.googlesource.com/platform/frameworks/base/+/94b4c163b7dfe5ce3607f7bb8456f9573f7de57d/core/java/android/app/ContentProviderHolder.java),
  [IUserManager](https://android.googlesource.com/platform/frameworks/base/+/94b4c163b7dfe5ce3607f7bb8456f9573f7de57d/core/java/android/os/IUserManager.aidl),
  [IThermalService](https://android.googlesource.com/platform/frameworks/base/+/94b4c163b7dfe5ce3607f7bb8456f9573f7de57d/core/java/android/os/IThermalService.aidl).
- frameworks/native android17-release
  `ae266dcb706d083868578cfedce381ef44488a07`:
  [GraphicBuffer accessor](https://android.googlesource.com/platform/frameworks/native/+/ae266dcb706d083868578cfedce381ef44488a07/libs/ui/GraphicBuffer.cpp),
  [native buffer C ABI](https://android.googlesource.com/platform/frameworks/native/+/ae266dcb706d083868578cfedce381ef44488a07/libs/nativebase/include/nativebase/nativebase.h).
- hardware/interfaces `0162af698935100a590b7359581ac8b1b80693e5`:
  [Codec2 AIDL ComponentStore](https://android.googlesource.com/platform/hardware/interfaces/+/0162af698935100a590b7359581ac8b1b80693e5/media/c2/aidl/android/hardware/media/c2/IComponentStore.aidl).

Kernel Binder protocol version **8** is distinct from Android API level **37**.
Stable NDK C/JNI/kernel contracts are not removed simply because an older
Android release also used them. Private framework contracts require independent
source/runtime evidence.

## Beach Buggy Racing 2 artifact boundary

Offline ZIP/ELF-header inventory of
`Beach+Buggy+Racing+2_2026.10.05_APKPure.xapk`:

- SHA-256: `ce9a6c4ae00ec749166e72504e8ee831b6929356576eb783e9f419226cb6cd00`.
- XAPK manifest: `com.vectorunit.cobalt.googleplay`, version `2026.10.05`,
  min SDK 24, target SDK 36.
- All **18** packaged native libraries are in `config.arm64_v8a.apk`,
  `lib/arm64-v8a/`; their ELF machine is **183 (AArch64)**. No x86_64 native
  payload was found across the package's APK members.
- This is ordinary offline archive/header inspection, not decompilation, REA
  Evidence, an app launch, or proof of runtime compatibility.

The package's target SDK does not change the API-37 runtime goal. The native
architecture is an additional, independent blocker. The bridge ABI, the guest
path rule and the ordered requirements are in [NATIVE_BRIDGE.md](NATIVE_BRIDGE.md);
retained inventory: `target/beach-buggy-2-native-inventory.json`.

## Verification

```sh
cargo build --workspace --offline
cargo test --workspace --offline
python3 -B -m unittest discover -s tools -p 'test_*.py' -v
cargo test -p pengudroid-shim --test graphicbuffer_native --offline
PENGUDROID_TEST_KERNEL_SOURCE=/absolute/kernel-source cargo test -p pengudroid-shim --lib android17_kernel_uapi_matches --offline
PENGUDROID_TEST_ANDROID17_SYSROOT=$PWD/.toolchain/sysroot-android17 cargo test -p pengudroid-cli --test art_framework --offline -- --nocapture
./target/debug/pengudroid launch NewPipe_v0.29.1.apk --sysroot .toolchain/sysroot-android17
```

Build all runtime shared libraries before invoking the framework probes. Live
runs must have their daemon running first on a private `PENGUDROID_SOCKET`, be
bounded, and retain logs. See [STATUS.md](STATUS.md) for current validation and
next action. A workspace run that skips the optional image/hardware gates must
not be reported as a successful Android-17 or hardware playback run.
