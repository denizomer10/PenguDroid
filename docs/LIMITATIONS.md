# Limitations and the hard problems

PenguDroid's infrastructure works, but the truly hard part of "Wine for Android"
is not the broker — it is executing Android user space on glibc. This document
states the open problems plainly so nobody mistakes the MVP for a finished
compatibility layer.

## 1. `system.img` cannot be removed, only relocated

The request was "no `system.img`". Taken literally that is not achievable, and
it is worth being precise about why. An Android app is not a self-contained
binary: to run it you need, at minimum,

* **ART** (`libart.so`, `libdexfile.so`, ...) to execute DEX bytecode,
* **Bionic** (`libc.so`, `libm.so`, `libdl.so`) that ART and native libraries
  link against,
* the **framework** (`framework.jar`, `libandroid_runtime.so`, `libhwui.so`,
  `libgui.so`, `libbinder.so`, `libEGL.so`, `libGLESv2.so`, gralloc, ...).

Those are binaries and they must come from somewhere — a system image, the APEX
modules, or an extracted `/system` tree. What PenguDroid removes is the
*container/VM*: there is no boot, no `init`, no zygote process tree, and no
namespace. The Android libraries are meant to be loaded into ordinary host
processes behind the shim. Removing the container is achievable; removing the
need for the libraries is not.

## 2. ART on glibc: the VM runs, the *system* does not

Wine works because the Win32 ABI is stable, documented, and — crucially —
Wine reimplements the entire platform rather than hosting Microsoft's
`ntdll`. PenguDroid's plan is the harder, opposite bet: host *Google's* ART.

**Current runtime contract: Android 17 / API 37 x86_64 only.** Earlier Android-17
bringup reached JNI_OK, core framework probes, UI rendering and service binding;
that does not establish complete media/application compatibility. The verified
runtime tree is absent from the current checkout, so new live validation is
blocked, not redirected to another Android release. See [ANDROID17.md](ANDROID17.md)
and [STATUS.md](STATUS.md). Older diagnostic observations below are historical
lessons, not supported runtime configurations.

The path there was ABI translation, not environment emulation:

* Android's packed relocations (`APS2`) are expanded into standard
  `RELA`/`RELR` by `pengudroid de-bionic` (new `PT_LOAD` + retagged dynamic
  entries), so a modern Android `.so` no longer loads with zero relocations.
* The linker API (`android_dlopen_ext`, namespaces, `__loader_*`), CFI/fdsan/
  fortify/`_ctype_`, liblog's logger API, Bionic `sysconf` numbering, the log and
  property calls, and libc++'s needs are all supplied by the shim.
* **Signal ABI.** This was the last hard blocker and is worth spelling out:
  Bionic's `sigset_t` is 8 bytes and its `struct sigaction` 32; glibc's are 128
  and 152. `libart`'s static initialiser for `fault_handler.cc` does
  `sigaction(SIGSEGV, nullptr, &gFaultManager.sa_)` into a *32-byte* field of an
  88-byte global, so glibc's 152-byte write silently zeroed the object's
  `initialized_` flag and every later `FaultManager::Init()` hit
  `CHECK(!initialized_)`. The shim now interposes the whole signal API
  (`sigaction`, `sigemptyset`/`fillset`/`addset`/`delset`/`ismember`,
  `sigprocmask`, `pthread_sigmask`, `sigpending`, `sigsuspend`, `sigwait`,
  `ppoll`, plus the `*64` variants) and converts between the two layouts.
  The LP64 field order is also significant: flags at offset 0, handler at 8,
  mask at 16, restorer at 24. Using the LP32 order left the real handler
  uninstalled and reproduced NewPipe's fatal signal 34 during search. A C ABI
  fixture now installs the handler and delivers the real kernel signal.
* **Framework natives.** `libandroid_runtime.so` has no `JNI_OnLoad`; on a
  device its ~150 `register_android_*` entry points are called by `app_process`
  from a static table. `--register-natives` reads the library's exported
  `register_*` symbols and calls them in the same shape as `gRegJNI` (logging
  natives first, `Binder` last), which is what makes `Trace` and `Log` work.
* **A virtual root, not a mount namespace.** ART records the boot class path as
  *locations* (`/system/framework/framework.jar`, `/apex/.../core-oj.jar`)
  because the boot image was built with those names, and libcore's
  `VMClassLoader` later re-opens those exact strings. Rather than `chroot`,
  bind-mount or boot a container, the shim rewrites absolute paths under the
  guest mount points (`/apex`, `/system`, `/system_ext`, `/vendor`, `/product`,
  `/odm`, `/data`, ...) into `$PENGUDROID_ROOT` at the libc boundary —
  `open`/`openat`, `fopen`, `stat`, `access`, `readlink`, `opendir`, `realpath`.
  Host paths (`/proc`, `/dev`, `/etc`) are untouched.
* **The boot class path is not just the boot image.** The image covers 14
  components (`boot.art` records `component_count=14`),
  but Android 13 moved connectivity, bluetooth, conscrypt, permission,
  tethering, wifi, mediaprovider and friends into mainline APEX modules, each
  publishing `etc/classpaths/bootclasspath.pb`. On a device init concatenates
  those fragments into `BOOTCLASSPATH`; `pengudroid` now does the same (31 jars
  here). Missing them makes a native `FindClass` for `android.net.ProxyInfo`
  fail with ART's *preallocated* `NoClassDefFoundError`, which has no message
  and no stack trace.
* **`AndroidRuntime::setArgv0` without `app_process`.** `ActivityThread.main`
  calls `Process.setArgV0`, whose native half passes
  `AndroidRuntime::getRuntime()` to `setArgv0`. That global is installed by
  `app_process`, which PenguDroid deliberately does not run, so it is null and
  the function dereferences `this->mArgv0` and segfaults. The shim interposes
  the symbol (reached through the PLT) and sets the thread name only.

**What remains — and why it is not a small patch.** An *Activity* used to be
unreachable, because starting one means `system_server` calls back into the app
with `bindApplication`, and there is no `system_server`: no package manager, no
window manager, no SurfaceFlinger. PenguDroid now closes that gap from the other
side: with `--bind-app` the *host* plays `system_server`'s part over JNI — it
builds `AppBindData`, calls `handleBindApplication`, then
`handleLaunchActivity`/`handleResumeActivity` for the manifest's launcher
activity, fakes the window session's `relayout`/`setInsets` replies, and lets
`ViewRootImpl` drive its own traversal. NewPipe 0.29.1 goes all the way through
`performDraw` and presents on Wayland.

What that does **not** give you is a real system: there is still no package
manager (installed packages are synthesised per launch), no `ActivityManager`
lifecycle (no `onPause`/`onStop`/task switching), no input dispatch, no
`SurfaceFlinger` (the app's frames are exported straight to the compositor), and
no `AudioFlinger`. `MediaCodec` decodes in ByteBuffer mode (H.264 through
`libcodec2_soft_avcdec.so`, verified byte-exact against `ffmpeg`) but not yet to
a `Surface`. Apps that only need to draw a view hierarchy work; apps that need
the platform's services do not.

### 2.1 ARM translation (Intel's NDK translation / Houdini lineage)

Most real Android games ship `arm64-v8a` only, so a translator is needed on an
x86_64 host. PenguDroid uses the same one the official emulator uses:
`libndk_translation.so` from Google's Android 11 (API 30) x86_64 Google-APIs
system image — the maintained descendant of Intel's Houdini.

What is wired up and verified:

* `art-test --bridge-lib <dir> --native-bridge libndk_translation.so --init-bridge arm64`
  runs the three-step sequence `app_process` uses, because each step asserts the
  previous one happened: `LoadNativeBridge(name, callbacks)` →
  `PreInitializeNativeBridge(<data dir>, "arm64")` (creates
  `<dir>/arm64/code_cache`) → `InitializeNativeBridge(env, "arm64")`.
* The translator's guest runtime is unpacked next to the system tree:
  `/system/lib64/arm64/*` (59 ARM64 libs), `/system/lib/arm/*`,
  `/system/bin/arm64/{app_process64,linker64}`, and the
  `libndk_translation_proxy_*` graphics shims that forward the guest's
  EGL/GLES/Vulkan calls to the host driver — so translation stays
  vendor-neutral.
* Output on this host:
  `ndk_translation: Initialized NDK translation (aarch64)`, then the guest
  linker resolves the ARM64 libraries, loads `/system/bin/arm64/app_process64`,
  and `ndk_translation::ExecuteGuest` begins executing real ARM64 code.

Three findings worth keeping:

1. **Bionic VERNEED entries.** The translator is a normal x86-64 `.so`, but it
   carries Android version needs (`LIBLOG`, `LIBLOG_R`, `LIBC`, ...) and imports
   such as `pthread_exit@LIBC` and `sigsetjmp` (which glibc only has as
   `__sigsetjmp`). It is therefore run through `de-bionic` as well, and the shim
   exports `sigsetjmp` as a **tail jump** to `__sigsetjmp` so `longjmp` still
   returns to the original caller.
2. **The bridge eats address space.** Once the translator has reserved the ARM
   guest layout, ART's default 1 GiB region space no longer fits and
   `JNI_CreateJavaVM` aborts with `region_space_mem_map.IsValid()` /
   `Cannot allocate memory`. `--xmx 128m` is what makes the VM come up with the
   bridge loaded.
3. **Absolute paths are mandatory.** The translator opens
   `/system/bin/arm64/app_process64` and `/system/lib64/arm64/...` by absolute
   path, and the guest linker will happily pick `libc++.so` from `/system/lib64`
   if an x86-64 library is there. The run needs `/system` and `/apex` mounted at
   their real locations (a `bwrap` mount namespace does this without root) with
   `/system/lib64` pointing at the **ARM** directory.

**Not solved yet.** `ExecuteGuest` reaches the translator's lazy code-translation
fault handler (`ndk_translation_HandleNoExec`) and dies there with a SIGSEGV.
That is inside the translator's own page-fault/trampoline machinery, not in the
shim's ABI surface, and it is where this line of work currently stops: ARM64
code *starts* executing but does not yet run. Booting the guest ARM
`app_process64`/zygote is additionally the emulator-style "second Android
runtime" path, so even a fixed handler would need the same `system_server` work
that blocks Activities above.

### 2.2 ARM64 execution is an independent migration gate

The old mixed-runtime/container diagnostic recipe is retired. It is not a
supported launch path and must not be used as a fallback for API 37.
An ARM64 implementation must work within the Android-17 host boundary and keep
its loader, libc/JNI, graphics and memory ownership contracts explicit.

The currently supplied Beach Buggy Racing 2 XAPK contains 18 native libraries,
all ELF AArch64, and no x86_64 native payload. This was checked offline across
all its APK members; the application was not executed. Exact artifact identity
and the migration gates are recorded in [ANDROID17.md](ANDROID17.md). The older
APKM example in the next section is a different artifact, not current validation.

### 2.3 Worked example: Beach Buggy Racing 2 (APKM bundle)

A real Play bundle (`com.vectorunit.cobalt.googleplay`, 262 MB, APKMirror
`.apkm`) is a good end-to-end test of the pieces above. `pengudroid info`
resolves it without manual unzipping:

```
bundle:  ...apkm (apkm)   base apk: base.apk
splits:  36
  split_config.armeabi_v7a.apk   abi armeabi-v7a
  split_config.arm64_v8a.apk     abi arm64-v8a
  split_asset_pack.apk           (213 MB of assets)
  split_gpdeku.apk / split_gpdeku.config.arm64_v8a.apk
  ... density and language splits
abis:    arm64-v8a, armeabi-v7a
```

`pengudroid extract ... --abi arm64-v8a` writes the base APK plus the arm64 split
(and the OBB payload, if a bundle ships one), and `doctor` then reports the truth
about the *game* rather than about `base.apk`:

```
bundle splits:    36
native libraries: 37        (these live in the ABI split, not in base.apk)
ABIs:             arm64-v8a, armeabi-v7a
host ABI (x86_64): no
main activity:    com.vectorunit.cobalt.MainActivity
DEX:              9188 classes
verdict:          not runnable natively: ARM-only code (needs the ARM translator)
```

Its engine is `lib/arm64-v8a/libmain.so` (17.8 MB, SDL3-based) plus `libSDL3.so`,
`libfmod*.so`, crash reporters and ad SDKs. It needs both the ARM64 translator
(§2.4) **and** an `Activity` start, i.e. `system_server`.

**Update (2026-11-05): this game now runs.** With the translator bridge fixed
(guest/host `pthread_attr_t` layout, TLS handoff) and the manifest's real
`versionName`/`versionCode` reported to `PackageInfo`, BBR2 2026.10.05 boots
splash → update check → title → **live gameplay** (22,911 presents in a 120 s
run before frame pacing; ~47/s after, capped at `PENGUDROID_MAX_FPS`=60). See
[STATUS.md](STATUS.md) and the screenshots in `docs/screenshots/`.

### 2.4 Where the translator stops, precisely

The ARM64-to-x86_64 translator is now built from the pinned Digitalis source
tree, not inferred from an older `ndk_translation` artifact:

* `tools/build-nativebridge.py --towards-bridge --seconds 1800` produces
  `libberberis_arm64.so` (37,719,928 bytes,
  SHA-256 `11fec1897d6ec4cb5faa19eb14753db980e7d4ea43d5dcf2383d36581e976f35`)
  with a global dynamic `NativeBridgeItf` export (version 8).
* The matching ARM64 guest runtime is extracted from the *same*
  `CP2A.260605.012` `aosp_arm64` GSI by `tools/prepare-arm64-guest.py`, which
  verifies SHA-256
  `8e545e1e1e977c3b43b4e54a3af463f3e7d1c3177b120e772138334e15f48f75`, expands
  `system.img` without mounting it, and stages 832 files under
  `system/bin/arm64/` and `system/lib64/arm64/` in the API-37 x86_64 sysroot.

A real `art-test --init-bridge arm64` run now reaches:

```
art: LoadNativeBridge(libberberis_arm64.so) = true
art: PreInitializeNativeBridge(/tmp/pengudroid-bridge, arm64) = true
pengudroid:log INFO berberis: Initialized Berberis (aarch64)
pengudroid:log FATAL berberis: native_bridge_initialize:
  couldn't find "__native_bridge_config_static_tls" symbol in linker
```

**Open: the guest linker is the stock GSI linker.** Digitalis expects the
native-bridge product's *modified* ARM64 linker
(`frameworks/libs/native_bridge_support/android_api/linker`), which exports
`__native_bridge_config_static_tls` and configures the guest's static TLS. The
AOSP ARM64 GSI ships the stock `linker64`, which does not. The matching guest
`libc` is likewise built with the native-bridge support hooks. Building those
two ARM64 artifacts (and replacing the staged stock copies) is the next concrete
step; the translator itself no longer has a bring-up blocker in this tree.

**Update (2026-11-05): superseded.** The guest linker/TLS handshake works now
(the bridge carries a matching guest runtime with native-bridge linker
support), and BBR2 — ARM64-only — runs end to end. The standing rule is
unchanged: never substitute an x86_64 copy, patch the game binaries, or
silently fall back to a container.

**Measured throughput (2026-11-12).** A 170 s run with
`PENGUDROID_TRACE_GLESHIM=1` presented 5,280 frames and ended with
`app_exit: null`. The gleshim swap counter read 240 at t=100 s and 2,640 at
t=140 s: the game spends roughly its first 100 s loading and compiling shaders,
then holds a vsync-locked **60 fps**. Frame production is therefore not the
blocker it was long assumed to be — the earlier "well under 1 fps" reading came
from a 60 s sample that landed entirely inside the warm-up. What remains slow is
the path *to* the first frame, which is CPU-bound interpretation (`-Xint`).
`-Xint` is kept because ART's image cannot be used yet, not because the JIT is
broken: the boot image now validates and loads once
`ro.dalvik.vm.enable_uffd_gc` is set and the boot classpath matches the image's
OAT header, but ART's userfaultfd GC then faults right after its first
mark-compact. See [STATUS.md](STATUS.md) for the full chain and repro.

**Consequences for real games.**

* **Native x86_64 `NativeActivity` games run today, on the GPU** —
  `pengudroid native`, with `AAssetManager`, `ALooper`, EGL/GLES and Vulkan. This
  is the path `pengudroid launch` picks, and `fixtures/native-game` is a playable
  game at ~56 fps on an RTX 3080.
* **Genshin Impact is not a claim this project makes.** Its Android build is
  `arm64-v8a` only, and the translator completes guest execution for the class
  of workload it targets (see the BBR2 evidence in
  [NATIVE_BRIDGE.md](NATIVE_BRIDGE.md)). Genshin is still out of reach for
  reasons the translator does not address: ~20 GB of OBB data, a proprietary
  anti-cheat, and `system_server`-backed login and surfaces. There is no
  legitimate way to obtain or run it in this environment, so no measurement of
  it exists here. What the project does verify is the same class of workload on
  a real ARM64 game: Beach Buggy Racing 2, an SDL3 engine calling EGL/GLES
  directly, which loads, renders and presents frames at 60 fps once warm (see
  "Measured throughput" above).

## 3. Native `.so` compatibility

An APK's native libraries are Bionic-linked ELF objects. PenguDroid can now
**load and call** real ones on glibc: `pengudroid-elf` reads their imports, the
shim supplies the Bionic symbols (`__errno`, `__strlen_chk`, `__sF`, ...) and the
Android version nodes (`LIBC`/`LIBM`/`LD`), the per-symbol versioning is
stripped, and the sonames are mapped. Verified by loading real x86-64 APK
libraries and by calling `fixtures/native-demo`'s entry point, which returns the
correct result through the shim.

What remains is the gap between *running a function* and *running an app*:

* **Teardown.** Mitigated: the shim suppresses Bionic C++ static destructors
  (`PENGUDROID_SUPPRESS_DESTRUCTORS`), which is what crashed `dlclose`; verified
  that a faulting destructor goes 139 → 0. Suppression leaks rather than frees,
  which is acceptable for a process that is about to exit.
* **`__sF`.** Partially supported: the three standard streams are mapped to
  glibc, assuming Bionic's LP64 `sizeof(FILE)` = 152. Other `FILE` objects and
  vararg formatting in `fprintf` are not supported (`vfprintf` is).
* **libc++.** Symbols like `_ZNSt3__1...` come from the APK's own bundled
  `libc++_shared.so`. `nativelib --apk` now stages the APK's whole `lib/<abi>`
  set so that dependency resolves; `fixtures/cpp-cascade` verifies a consumer
  calling through the bundled runtime. (The F-Droid apps checked statically link
  libc++, so a real `libc++_shared.so` APK was not available to test against.)
* **The Android linker namespace.** Implemented as a translation: the shim
exports `android_dlopen_ext`, `android_create_namespace`,
`android_link_namespaces`, the `__loader_*` entry points and the rest of the
linker API, mapping Android's per-app namespaces onto glibc's single flat
namespace. `libdl_android.so` and `ld-android.so` resolve to the shim.
* **Packed relocations.** Implemented: `de-bionic` expands `APS2` into standard
  `RELA`/`RELR`. Without this a modern Android `.so` (anything built in the last
  several years) loads with *no* relocations applied and crashes in its first
  constructor.
* **Graphics.** `libEGL`/`libGLESv2` must be bound to host EGL before apps draw.

The shim exports around 45 Bionic entry points plus the version nodes; a large
C++/NDK app will still hit missing symbols, and `pengudroid deps` measures the
remaining gap per library.

## 4. Graphics: from `wl_shm` to the GPU

The MVP initially presented pixels through `wl_shm` (CPU memory). PenguDroid now
has a working **zero-copy GPU path**: `pengudroid-hostgpu` binds the host's
EGL/GLES via `dlopen`, renders into an FBO on the real GPU, exports the result
as a dma-buf, and the daemon imports it with `zwp_linux_dmabuf_v1`. The guest
runtime uses this automatically (`pengudroid run` reports "rendering on the host
GPU via dma-buf"), falling back to `wl_shm` when unavailable. `pengudroid
gpu-test` verifies rendering by reading pixels back, and `pengudroid-daemon
--dmabuf-selftest` verifies the compositor accepts the buffer.

An app's own EGL calls now do reach the GPU: `crates/pengudroid-gleshim` is a
Bionic-compatible `libEGL` (plus a small `libGLESv2`) that binds the host's
EGL/GLES, renders the app's drawing into an FBO, and exports each
`eglSwapBuffers` as a dma-buf presented by the daemon. `fixtures/egl-app` swaps
five frames through it, and NewPipe's whole UI goes through the same path.

Two details make hwui work through that shim, and both are easy to miss:

* hwui renders into the **default framebuffer** (id 0), which in a surfaceless
  context is nothing. `glBindFramebuffer` is interposed so that binding 0 binds
  the current surface's FBO instead.
* hwui calls `eglSetDamageRegionKHR` before every swap and `LOG_ALWAYS_FATAL`s
  when it fails, so the shim has to answer it (and `eglGetProcAddress` has to
  return the shim's own entry points rather than the host's).

**Vulkan works.** Android's `libvulkan.so` is a loader, and Vulkan's ABI is
identical on Android and Linux, so the compat directory maps the Bionic soname
onto the *host* loader (`libvulkan.so.1`). An Android app's `vkCreateInstance`,
`vkEnumeratePhysicalDevices` and `vkCreateDevice` therefore reach the real
driver: `fixtures/native-vulkan` enumerates an RTX 3080 (API 1.4.329) and
creates a logical device and graphics queue.

Still missing: `gralloc`/`AHardwareBuffer` compatibility (apps that allocate GPU
buffers through it fail), hardware composition, and GLES coverage beyond the
exported core functions. The compat directory maps `libGLESv2.so` to the *host*
driver and the shim loads it `RTLD_GLOBAL`, so an app's direct GL imports resolve
in practice.

### 4.1 Window resize keeps the internal resolution

A resized window does **not** make the guest render at a new pixel size, and it
does not re-configure the guest's `Configuration`. The daemon's viewporter path
is deliberate about this (`configure_window` in
`crates/pengudroid-daemon/src/graphics.rs`): `surface.width/height` — the guest's
own buffer — is left untouched, and only `display_width/display_height` and the
viewport destination change, so the compositor scales the fixed-size guest frame
into the new window. Input is mapped back into guest space by
`guest_coordinates`, so a touch lands on the widget the user sees even though
the guest never learned the window changed.

Consequences, stated plainly:

* **Aspect ratio is preserved and letterboxed.** Free-form drags do not stretch
  the picture, and the app's internal resolution is stable — a game keeps its
  render target, a video keeps its decoded size.
* **Density is never re-derived mid-session.** It is chosen at launch
  (`crates/pengudroid-cli/src/geometry.rs::for_window`). Re-deriving it on resize
  would rescale every touch target under the user's finger, so
  `Geometry::for_resize` keeps the launch density and only the dp size — and
  therefore the phone/tablet classification — follows the new window.
* **Tablet layouts are a launch-time choice unless the resize follower is on.**
  `smallestScreenWidthDp >= 600` switches the framework to tablet layouts, and
  the density that makes that true is picked from the window size at launch
  (`--width`/`--height`, or the defaults 720x1280 at 320 dpi). A phone-launched
  session that is later maximized crosses 600 dp only if the new window is at
  least `Geometry::tablet_side_px()` wide (1200 px at 320 dpi).
  `PENGUDROID_FOLLOW_RESIZE=1` makes that transition happen live — see §4.2.
  Without it, `apphost` builds `Configuration`/`DisplayInfo` once and nothing
  changes.
* **An app that forces a fixed orientation may letterbox more.** BBR2 asks for
  landscape (`requestedOrientation=11`) on a portrait 720x1280 surface, so its
  content is presented inside that frame rather than filling a landscape window.

`pengudroid geometry` prints the resolved policy without launching anything:

```
$ pengudroid geometry --resize 1920x1200
720x1280 px at 320 dpi -> 360x640 dp, smallestScreenWidthDp 360, phone
tablet layouts start at a 1200 px window side at this density
resized to 1920x1200 px -> 960x600 dp, smallestScreenWidthDp 600, tablet (density kept at 320 dpi)
  note: the guest keeps rendering at 720x1280 px; the compositor scales that frame into the new window, and the app is not told about the size change
```

**OpenSL ES is implemented for the common case.** `slCreateEngine`, the object
model, engine/output-mix/player interfaces and the Android simple buffer queue
are real: `Enqueue` forwards each buffer to a per-player `pw-cat`. The fixture
uses the canonical headers so the vtable ABI is genuinely verified. Recorders,
prefetch, effects and the other interfaces return `SL_RESULT_FEATURE_UNSUPPORTED`.

### 4.2 Following a live resize (`PENGUDROID_FOLLOW_RESIZE`)

Relayout-on-resize — replaying `IWindow.resized` into a live guest so `hwui`
re-measures at the new dp size — is wired behind `PENGUDROID_PUMP_RESIZED` and
`PENGUDROID_FOLLOW_RESIZE`. The second implies the first.

Verified live on BBR2 (60 s and 100 s runs with `PENGUDROID_FOLLOW_RESIZE=1`):

```
apphost: window is now 496x883; guest relayout at 496x883 px (248x441 dp), 1 root(s) poked
```

— one poll, one publish, one root poked — and the game kept presenting
afterwards: 246 presents on the live surface, `app_exit: null`. So the pull, the
geometry publish and the poke all run against a real daemon and a real guest
window.

The transition itself is now verified too, because the window state can be
driven without a seat: `Message::SetWindowState` (protocol v5) asks the daemon
for `WINDOW_MAXIMIZED`/`WINDOW_FULLSCREEN`, the daemon calls the same
`xdg_toplevel` requests a title-bar button issues, and Mutter answers with a real
`configure` — the same event an interactive drag produces.

```
$ pengudroid window 1
surface 1: 720x1280 px internal, 668x1189 px window (334x594 dp)
$ pengudroid window 1 --maximize
surface 1: 720x1280 px internal, 2560x1373 px window (1280x686 dp, tablet)
$ pengudroid window 1 --fullscreen
surface 1: 720x1280 px internal, 2560x1440 px window (1280x720 dp, tablet)
$ pengudroid window 1 --unmaximize
surface 1: 720x1280 px internal, 668x1189 px window (334x594 dp)
```

and the follower published each step in the guest, in both directions:

```
apphost: window is now 668x1189; guest relayout at 668x1189 px (334x594 dp), 1 root(s) poked
apphost: window is now 2560x1373; guest relayout at 2560x1373 px (1280x686 dp, tablet), 1 root(s) poked
apphost: window is now 2176x1167; guest relayout at 2176x1167 px (1088x583 dp), 1 root(s) poked
```

Four things those runs established:

* **Internal resolution survives.** The guest buffer stayed `720x1280` (and
  `668x1189` on BBR2's own window) while the window became `2560x1373`; the
  compositor scaled it and never reallocated. Frame size and window size are
  independent, in every direction.
* **The tablet switch is dynamic and reversible.** `334x594 dp` classifies as a
  phone, `1280x686 dp` as a tablet, and `1088x583 dp` as a phone again — the
  600 dp line is crossed without a relaunch, because `Geometry::for_resize`
  keeps the density.
* **The guest reacts.** After the first relayout BBR2 created a *new* surface
  already sized to the maximized window (`surface 7 configured (2560×1373)`),
  which is the app replacing its window in response to the configuration change.
  That is also why the second step above is `2176x1167`: un-maximizing the *old*
  window does nothing to the new one, so the state command has to be aimed at the
  window that is on screen — `pengudroid window` with no id lists them.
* **No relaunch is needed for any of it.** Each step is a `configure` and a
  relayout inside the same process.

An interactive pointer drag is still untested as such — Mutter, not a pointer,
sent these configures — but the configure handler is the same one a drag ends up
in, and the compositor-gated test
`native_frame_resize_maximize_and_close_keep_video_and_input_aligned` covers the
frame math.

The shape of it is a **pull**, not a daemon push:

* `pengudroid-gleshim` runs in a separate `.so` mapping from the rest of the
  process, and the app host lives in the child the daemon spawns, not in the CLI
  parent. There is no shared static to signal through and no thread the daemon
  can call, so a `SurfaceResized` push would need new plumbing on both sides.
  Instead the host asks: `SurfaceGeometry` (protocol v4) returns the guest
  buffer size and the window size for one surface, and the follower compares the
  window size against what it last applied.
* When it changed, the host republishes the geometry into the environment with
  `Geometry::for_resize(width, height).export()` and pokes every window root.
  The environment is the same channel `Geometry::from_env` reads at launch, so
  `Configuration`/`DisplayInfo` and the daemon agree by construction.
* `relayoutAsync` is one-way (the client keeps its locally computed frames), so
  the poke sets `mForceNextWindowRelayout` and `mLayoutRequested` and calls
  `scheduleTraversals()`. That forces `IWindowSession.relayout`, whose reply is
  what actually updates the guest's frame.

To find the surface it must ask about, the shim publishes
`PENGUDROID_LAST_SURFACE_ID` when it creates a window. **The newest window wins,
and the follower re-reads it on every tick**, because apps replace their window:
BBR2 creates five surfaces during loading and destroys each when the next
appears, so at any moment the daemon holds exactly one — the last. A follower
that latched onto surface 1 (the first) spent the whole run asking about a
surface that no longer existed; that is what the live run showed before this was
fixed.

The variable is deliberately **not** `PENGUDROID_SURFACE_ID`: that one is an
*input* meaning "attach to an existing surface instead of creating one", so
writing it from the shim would alias a game's second window onto its first.

The daemon answers this query with `-3` when it has no such surface — normal
while a window is being replaced, and the client turns it into `Ok(None)` rather
than an error — and `-19` when it has no compositor at all, which is a real
failure. The follower retries quietly on `-3` and reports anything else, backing
off to one line per ten seconds so a dead surface cannot flood the log.

`ViewRootImpl.mClientWindowFrames.frame` is forced best-effort, and logs
`apphost: Android 17 ViewRootImpl.mClientWindowFrames.frame is unavailable` when
the field or its `Rect` cannot be resolved.

## 5. SurfaceFlinger is bypassed, not emulated

Apps that talk to `SurfaceFlinger` directly (surface control, layer stacks,
secure layers, `Transaction` queues) will not get faithful behaviour. The
`SurfaceComposer` stub answers introspection only. Multi-window, overlays, and
hardware composition are out of scope for the MVP.

## 6. Kernel binder may be unavailable

Where `/dev/binderfs/binder` is missing and the module cannot be loaded, the
userspace broker is used. It preserves the transaction shape but not kernel
binder's guarantees: no kernel-mediated oneway ordering, no true death
notifications, no `mmap`-based transaction buffers, no priority inheritance.
Some framework code depends on those semantics.

## 7. Stubs are stubs

**Telephony is no longer a stub.** `ITelephony` uses its real AIDL transaction
codes and marshals replies in the real `Parcel` format (`crate::parcel`),
modelling a no-SIM, radio-off device: `getCallState` → idle, `isRadioOn` →
false, `hasIccCard` → false, `getDeviceId` → `null`, `getAllCellInfo` → empty.
`pengudroid telephony` transacts against the `phone` service and decodes each
reply, so the marshalling is observable. The implemented subset is the methods a
normal app reaches through `TelephonyManager`; the rest of the ~250-method
interface returns `-ENOSYS`. The transaction codes are the AIDL declaration
order for a given AOSP release and can shift when methods are inserted.

The camera reports "zero cameras"; `AudioFlinger` returns empty replies, and the
audio **policy** service answers `listAudioPorts`/`listAudioPatches` with an
empty list (the codes are read from the shipped `IAudioPolicyService$Stub`, so
they are never guessed). Empty is truthful here — audio reaches the host without
a framework port — but it also means anything that enumerates ports or patches
to *choose* a route sees none.
**`AudioTrack` no longer goes through it**: the Java class is hosted at the JNI
boundary (`crates/pengudroid-shim/src/audiotrack.rs` interposes
`AndroidRuntime::registerNativeMethods` and replaces the native table
`libmedia_jni` installs), so playback is a bounded queue drained into `pw-cat`.
What that does *not* provide is mixing, effects, volume scaling or routing —
those were `AudioFlinger`'s job and there is no replacement. `AudioRecord`, the
`AudioEffect`s and audio routing are therefore still unimplemented. The OpenSL ES
**buffer-queue player** is implemented (`Enqueue` → `pw-cat`), but recorders,
prefetch and effects are still `SL_RESULT_FEATURE_UNSUPPORTED`;
`PackageManager` and `ActivityManager` answer minimally. These are honest
placeholders, not partial implementations.

## 8. Security

By default there is **no** sandbox: guests are ordinary host processes with the
user's privileges and full host filesystem access. This is the opposite of
Android's process-per-app isolation.

`--isolate` narrows the gap without a container: it sets `PR_SET_NO_NEW_PRIVS`
and installs a seccomp filter that refuses 33 syscalls an app never needs and
that are the usual escape hatches (`ptrace`, `process_vm_*`, `mount`/`umount2`,
`pivot_root`, `chroot`, `unshare`/`setns`, module loading, `bpf`,
`perf_event_open`, `keyctl`, `reboot`, `kexec_*`, `swapon`, `acct`,
`open_by_handle_at`, `syslog`, `iopl`/`ioperm`, clock/hostname changes). It does
**not** restrict the filesystem or the network; a production design would add a
private mount view. Do not run untrusted APKs under PenguDroid.

## 9. Protocol is versioned but young

The control protocol is v5 (`PROTO_VERSION` in
`crates/pengudroid-proto/src/lib.rs`) and hand-rolled. It is tested for
round-trips and truncation handling, but has no negotiation for capability differences and no
framing for large zero-copy transfers beyond fd passing. The version is checked
exactly, so a stale peer is refused rather than misparsed — including a stale
`libpengudroid_shim.so`, which shows up in the daemon log as
`client protocol v4 != daemon v5; rebuild/restart matching peers`, not as a
mystery hang.

Guest-service calls are forwarded per connection with reply ordering implicit
(the oldest outstanding call is answered first). A call waits up to
`GUEST_CALL_TIMEOUT` (5 s); after that the caller gets `-ETIMEDOUT` rather than
hanging forever, so a wedged guest cannot freeze its peers.

## Roadmap (rough order)

1. ~~Load a single trivial Bionic `.so` under the shim and satisfy its imports.~~
   **Done**: `pengudroid-elf` reads imports and strips Android versioning; real
   x86-64 APK libraries (`librtb.so`, `libgtb.so`, `libnativeutil.so`) `dlopen`,
   and `fixtures/native-demo` is loaded **and called** with a correct result
   (see `docs/ARCHITECTURE.md` §8).
2. ~~Close the *control-plane* gaps: enumerate properties and services, forward
   guest-service calls, detach launches (`RunApk`) and report their exit status,
   stop guests when their window closes, and format variadic Bionic log calls.~~
   **Done** (end-to-end tested against a real daemon).
3. ~~Close the *running* gap: Bionic-compatible teardown, a real `__sF`, and
   loading an APK's own `libc++_shared.so`.~~ **Done**: destructors are
   suppressed (a faulting teardown goes 139 → 0), the three standard streams map
   through `__sF` (`fprintf` varargs remain literal), and `nativelib --apk`
   stages an APK's library set (`fixtures/cpp-cascade`).
4. ~~Present a GPU frame over `dmabuf`, including an app's own EGL calls.~~
   **Done**: `pengudroid-hostgpu` (host EGL/GLES/GBM + dma-buf export),
   `zwp_linux_dmabuf_v1` import in the daemon, zero-copy frames from
   `pengudroid run`, and `pengudroid-gleshim` — a Bionic `libEGL` that renders an
   app's own EGL/GL calls and swaps them to Wayland (`fixtures/egl-app`).
5. ~~Load the ART runtime and execute `JNI_CreateJavaVM`.~~ **Done**: the whole
   ART closure `dlopen`s, `JNI_CreateJavaVM` returns `JNI_OK`, framework natives
   register, an APK's DEX loads, and the host drives `handleBindApplication` +
   `handleLaunchActivity` so a real Activity draws and presents (§2).
   **Still open**: swapping `-Xint` for the JIT/AOT image (the image loads, but
   ART's uffd GC faults after its first mark-compact — see [STATUS.md](STATUS.md))
   and the platform services (`ActivityManager` lifecycle, input, `MediaCodec`).
6. ~~Wire the PipeWire audio pump.~~ **Done**: the daemon owns a shared ring and
   drains it into `pw-cat` (`audio-test --via-daemon`); silence by default,
   `--tone` audible. ~~OpenSL ES buffer-queue player~~ **Done**: `Enqueue` →
   `pw-cat`, verified against the canonical headers (`fixtures/opensl-app`).
7. ~~Replace telephony/camera stubs with real AIDL marshalers.~~ **Telephony done**:
   real `ITelephony` transaction codes and `Parcel` replies for the
   `TelephonyManager` subset, exercised by `pengudroid telephony`. Camera
   remains a stub.
8. ~~Add seccomp for isolation without a full container.~~ **Done** (`--isolate`:
   seccomp + `no_new_privs`). A private mount namespace remains.
9. ~~Run NativeActivity apps without ART.~~ **Done**: `pengudroid native` hosts
   the lifecycle, `AAssetManager` reads the APK, `ALooper` is a real `poll(2)`
   loop, and `fixtures/native-game` renders a playable game on the host GPU.
10. ~~Vulkan.~~ **Done**: the Bionic soname maps to the host loader;
    `fixtures/native-vulkan` enumerates the GPU and creates a device.
11. `AHardwareBuffer`/`gralloc` on dma-buf, then hardware composition.12. ~~`MediaCodec` (codec2), so video decodes.~~ **Done for ByteBuffer mode**:
    the in-process codec2 service decodes H.264 and the frame reaches Java with
    the right pixels. Surface (`SurfaceView`) output works for an `ImageReader`
    consumer. ~~`AudioTrack`~~ **Done**: the class is hosted at the JNI boundary
    and streams into `pw-cat`, so ExoPlayer plays. **Next**: `SurfaceView` —
    decoded frames still have no `BLASTBufferQueue` to be composited from, which
    is what keeps the picture off the window.
13. Input dispatch (touch/key) from the Wayland seat into the app's window.
14. Guest ARM code execution, so arm64-only apps (Beach Buggy Racing 2,
    Genshin Impact) can run at all — see §2.1.
