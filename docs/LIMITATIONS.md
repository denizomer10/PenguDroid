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

**Where it stands now.** `pengudroid art-test --sysroot <tree>` against an
extracted LineageOS 20 (Android 13, x86_64) tree returns **`JNI_OK`**. A live
`JavaVM` and `JNIEnv` come back, `GetVersion()` reports `JNI_VERSION_1_6`, and
Java code executes: classes taken from a real APK's `classes.dex` are loaded and
linked through the system class loader, and `android.app.ActivityThread.main`
can be invoked.

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
* **The boot class path is not just the boot image.** The image covers 12 jars,
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

### 2.2 Reproducing the translator run

The translator and its guest runtime come from Google's Android 11 (API 30)
x86_64 Google-APIs image (`system-images;android-30;google_apis;x86_64`,
`x86_64-30_r16.zip`, sha1 `6ae21030eaadc041078444d3798e4b399f3e787d`). Inside
its `system.img` (raw ext4 at offset `0x300000`):

```
/system/lib64/libndk_translation.so          # the translator
/system/lib64/libndk_translation_proxy_*.so  # GLES/EGL/Vulkan proxies
/system/lib64/arm64/                         # 59 ARM64 guest libs
/system/lib/arm/                             # 32-bit guest libs
/system/bin/arm64/{app_process64,linker64}   # guest runtime entry points
```

The run needs `/system` and `/apex` at their real paths, because the translator
opens them by absolute path. `bwrap` gives that without root:

```sh
SYS=$PWD/.toolchain/sysroot/system                 # the Android 13 system tree
ARM=$PWD/.toolchain/ndk-arm                        # the extracted ARM runtime
bwrap --ro-bind /usr /usr --ro-bind /lib /lib --ro-bind /lib64 /lib64 \
      --ro-bind /bin /bin --ro-bind /sbin /sbin --ro-bind /etc /etc \
      --bind /home /home --bind /tmp /tmp --bind /run /run \
      --dev /dev --proc /proc --bind /sys /sys \
      --ro-bind $SYS /system --ro-bind $SYS/apex /apex \
      --ro-bind $ARM/lib64/arm64 /system/lib64 \
      --ro-bind $ARM/lib/arm /system/lib/arm \
      --ro-bind $ARM/bin/arm64 /system/bin/arm64 \
      --dir /data --dir /dev/socket --chdir $PWD \
      ./target/debug/pengudroid art-test --sysroot .toolchain/sysroot \
        --bridge-lib .toolchain/ndk-arm/lib64 \
        --native-bridge libndk_translation.so --init-bridge arm64 --xmx 128m
```

Note `/system/lib64` is bound to the **ARM64** directory: the guest linker
otherwise picks the x86-64 `libc++.so` that lives in the host system tree.
Nothing about this path is vendor specific — the translator's proxy libraries
forward the guest's EGL/GLES/Vulkan calls to whatever host driver is installed.

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
`libfmod*.so`, crash reporters and ad SDKs. So it needs both missing pieces at
once: ARM64 execution (§2.1) **and** an `Activity` start, i.e. `system_server`.
`pengudroid launch` gets as far as materialising the bundle into the per-app
prefix, creating the VM, registering the framework natives and invoking
`ActivityThread.main` — and stops exactly there, with the note
`starting com.vectorunit.cobalt.MainActivity needs system_server`.

### 2.4 Where the translator stops, precisely

Two things were fixed after the first bring-up, and two are still open.

**Fixed: ART and the translator both want SIGSEGV.** The bridge installs its
handlers inside `NativeBridgeItf.initialize` (during `LoadNativeBridge`, i.e.
before the VM exists), and then `Runtime::Init` → `FaultManager::Init` replaces
them with ART's crash dumper. Both runtimes share this one process, so the
guest's lazy-translation faults would be handed to ART. `NativeBridgeGetSignalHandler
(sig)` returns the bridge's own action; `--init-bridge` now reinstalls it for
SIGILL/SIGBUS/SIGFPE/SIGSEGV once the VM is up, converting Bionic's 32-byte
`struct sigaction` to glibc's 152-byte one through the shim. (On a device
`sigchain` does the equivalent.)

**Fixed: the guest ELF is validated strictly.** Swapping in a hand-built ARM64
binary produced `unsupported e_shentsize: 0x0 (expected 0x40)` and then
`invalid e_shstrndx`, i.e. ndk_translation insists on a complete section-header
table and a real `.shstrtab` even for a static executable.

**Open: the guest runtime's handshake.** `InitializeNativeBridge` calls into the
bridge's `exec` slot, which starts `/system/bin/arm64/app_process64` on a
thread. Two outcomes were observed:

* with the real guest `app_process64`, the process dies in
  `ndk_translation_HandleNoExec` (guest code translation);
* with a trivial static ARM64 program that only does `write(2)` + `exit(2)`,
  there is no crash and no output: every thread ends up blocked on a futex (one
  on `do_sigtimedwait`, the translator's signal catcher), so the guest is
  waiting for a handshake it never gets.

So the translator itself initialises and parses guest ELFs, but running a guest
*Android userspace* — guest bionic init, guest linker namespaces, guest ART for
the ARM ABI — is the remaining piece, and that is what the emulator's own
`system.img` boots. Reaching that from a bare `JNI_CreateJavaVM` means
implementing the second half of an emulator.

**What that means for “make the game open”.** Beach Buggy Racing 2 needs ARM64
execution *and* `system_server`; a working ARM guest userspace would provide the
first and would itself need the second. The only configuration known to open
this game is an Android system image with ARM translation booted as a real
system (the emulator, or Waydroid-style container). This host does have
`/dev/kvm`, so that is possible as a *VM/container* — a different architecture
from PenguDroid's “apps as host processes” premise, and a decision for the
project owner rather than something to silently switch to.

**Consequences for real games.**

* **Native x86_64 `NativeActivity` games run today, on the GPU** —
  `pengudroid native`, with `AAssetManager`, `ALooper`, EGL/GLES and Vulkan. This
  is the path `pengudroid launch` picks, and `fixtures/native-game` is a playable
  game at ~56 fps on an RTX 3080.
* **Genshin Impact still cannot run here.** Its Android build is `arm64-v8a`
  only; the NDK translator above is the right tool for that and now initialises
  and starts ARM64 code, but it does not yet complete guest execution (see 2.1).
  On top of that it ships ~20 GB of OBB data, a proprietary anti-cheat, and it
  needs `system_server` for login and rendering surfaces. There is no legitimate
  way to obtain or run it in this environment, so "Genshin renders" is not a
  claim this project can make. What it *can* claim is the same class of workload:
  NativeActivity games that call EGL/GLES/Vulkan directly.

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

**OpenSL ES is implemented for the common case.** `slCreateEngine`, the object
model, engine/output-mix/player interfaces and the Android simple buffer queue
are real: `Enqueue` forwards each buffer to a per-player `pw-cat`. The fixture
uses the canonical headers so the vtable ABI is genuinely verified. Recorders,
prefetch, effects and the other interfaces return `SL_RESULT_FEATURE_UNSUPPORTED`.

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

The camera reports "zero cameras"; `AudioFlinger` returns empty replies; the
OpenSL ES **buffer-queue player** is implemented (`Enqueue` → `pw-cat`), but
recorders, prefetch and effects are still `SL_RESULT_FEATURE_UNSUPPORTED`;
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

The control protocol is v2 and hand-rolled. It is tested for round-trips and
truncation handling, but has no negotiation for capability differences and no
framing for large zero-copy transfers beyond fd passing.

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
   **Still open**: the JIT (only `-Xint` works) and the platform services
   (`ActivityManager` lifecycle, input, `MediaCodec`).
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
11. `AHardwareBuffer`/`gralloc` on dma-buf, then hardware composition.
12. ~~`MediaCodec` (codec2), so video decodes.~~ **Done for ByteBuffer mode**:
    the in-process codec2 service decodes H.264 and the frame reaches Java with
    the right pixels. Surface (`SurfaceView`) output and
    `AudioTrack`/`AudioFlinger` are still missing.
13. Input dispatch (touch/key) from the Wayland seat into the app's window.
14. Guest ARM code execution, so arm64-only apps (Beach Buggy Racing 2,
    Genshin Impact) can run at all — see §2.1.
