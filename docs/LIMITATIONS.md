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

## 2. ART on glibc is the central unsolved problem

Wine works because the Win32 ABI is stable, documented, and — crucially —
Wine reimplements the entire platform rather than hosting Microsoft's
`ntdll`. PenguDroid's plan is the harder, opposite bet: host *Google's* ART.

**What now works.** The ART APEX closure loads on glibc. `pengudroid de-bionic`
expands Android's packed relocations (`APS2`) into standard `RELA`/`RELR` and
retags the dynamic entries, the shim supplies the linker API
(`android_dlopen_ext`, `android_create_namespace`, the `__loader_*` entry
points), the CFI/fdsan/fortify/`_ctype_` surface, and the Bionic log and
property calls, and the whole set — `libart.so` (9 MB), `libartbase`,
`libdexfile`, `libc++`, `libnativebridge`, `libnativeloader`, `libunwindstack`,
`libsigchain`, `libartpalette` + `libartpalette-system`, and the system-library
chain they pull in — `dlopen`s. `JNI_CreateJavaVM` then **executes**: it enters
ART's real code, initialises the palette, and proceeds into runtime setup before
faulting.

**What remains.** Getting `JNI_CreateJavaVM` to return a live `JavaVM` is the
open problem. ART is deeply coupled to Bionic in ways a syscall shim does not
cover:

* the runtime's thread/GC machinery expects Bionic-specific suspend and TLS
  mechanisms;
* `sigchain` installs signal handlers through Bionic's `sigaction` contract;
* the boot classpath (`core-libart.jar`, `core-oj.jar`) and the JIT must be
  brought up, and the runtime expects a `/proc`, property and ashmem environment
  shaped like Android's.

The current fault is inside ART's runtime initialisation (after palette setup),
with no symbol gap left to close — it is a semantic mismatch, not a missing
import. Making this work is a research project measured in years, not a patch.
`pengudroid run` therefore still brings up a real process and a real window so
the surrounding plumbing (binder, ashmem, graphics, audio, properties, fd
passing) can be built and tested against a running system first.

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
five frames through it.

Still missing: `gralloc`/`AHardwareBuffer` compatibility, hardware composition,
Vulkan (the host Vulkan ICDs can be loaded, but no translation layer drives them
yet), and GLES coverage beyond the exported core functions (apps that call less
common GL entry points directly, rather than via `eglGetProcAddress`, may fail
to resolve). Note that the compat directory maps `libGLESv2.so` to the *host*
driver, which covers this in practice.

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

Telephony reports "radio off"; the camera reports "zero cameras"; `AudioFlinger`
returns empty replies; the OpenSL ES **buffer-queue player** is implemented
(`Enqueue` → `pw-cat`), but recorders, prefetch and effects are still
`SL_RESULT_FEATURE_UNSUPPORTED`; `PackageManager` and `ActivityManager` answer
minimally. These are honest placeholders, not partial implementations.

## 8. Security

There is **no** sandbox: guests are ordinary host processes with the user's
privileges and full host filesystem access. This is the opposite of Android's
process-per-app isolation. A production design would need to reintroduce
isolation (at least a seccomp filter and a private mount view) without
reintroducing a container. Do not run untrusted APKs under PenguDroid.

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
5. ~~Load the ART runtime and execute `JNI_CreateJavaVM`.~~ **Partial**: the whole
   ART closure `dlopen`s and `JNI_CreateJavaVM` executes into ART's runtime setup
   (packed relocations, the linker API, CFI/fdsan/fortify/`_ctype_` and the
   system-library chain are all handled). Returning a live `JavaVM` remains open
   — see §2.
6. ~~Wire the PipeWire audio pump.~~ **Done**: the daemon owns a shared ring and
   drains it into `pw-cat` (`audio-test --via-daemon`); silence by default,
   `--tone` audible. ~~OpenSL ES buffer-queue player~~ **Done**: `Enqueue` →
   `pw-cat`, verified against the canonical headers (`fixtures/opensl-app`).
7. Replace telephony/camera stubs with real AIDL marshalers.
8. Add seccomp + a private mount namespace for isolation without a full
   container.
