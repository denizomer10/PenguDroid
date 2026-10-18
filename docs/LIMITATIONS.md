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

ART is deeply coupled to Bionic in ways a syscall shim does not cover:

* linker **namespace** semantics (`libnativeloader`, `android_dlopen_ext`) that
  glibc's `dlopen` does not implement;
* `__system_property_*`, `pthread` internals, and TLS layout assumptions baked
  into compiled `.so`s;
* a GC that expects to find and suspend threads through Bionic-specific
  mechanisms.

Making this work is a research project measured in years, not a patch. The MVP
deliberately does **not** claim to run DEX bytecode; `pengudroid run` brings up a
real process and a real window so the surrounding plumbing (binder, ashmem,
graphics, audio, properties, fd passing) can be built and tested against a
running system first.

## 3. Native `.so` compatibility

An APK's native libraries are Bionic-linked ELF objects. PenguDroid can now
**load and call** real ones on glibc: `pengudroid-elf` reads their imports, the
shim supplies the Bionic symbols (`__errno`, `__strlen_chk`, `__sF`, ...) and the
Android version nodes (`LIBC`/`LIBM`/`LD`), the per-symbol versioning is
stripped, and the sonames are mapped. Verified by loading real x86-64 APK
libraries and by calling `fixtures/native-demo`'s entry point, which returns the
correct result through the shim.

What remains is the gap between *running a function* and *running an app*:

* **Teardown.** `dlclose`/`atexit` run C++ static destructors that assume Bionic
  runtime state and can segfault (seen on `librtb.so`), so PenguDroid loads and
  uses libraries without unloading them.
* **`__sF`.** Bionic's stdio array satisfies linkage but is not ABI-compatible
  with glibc's `FILE`, so stdio routed through it is unsupported.
* **libc++.** Symbols like `_ZNSt3__1...` come from the APK's own bundled
  `libc++_shared.so`. `pengudroid nativelib --apk` now strips the APK's whole
  `lib/<abi>` set into one directory so that dependency resolves, but a large
  C++ app may still hit missing Bionic symbols that `pengudroid deps` will
  report.
* **The Android linker namespace.** `android_dlopen_ext` is not implemented; it
  carries namespace semantics glibc lacks.
* **Graphics.** `libEGL`/`libGLESv2` must be bound to host EGL before apps draw.

The shim exports around 45 Bionic entry points plus the version nodes; a large
C++/NDK app will still hit missing symbols, and `pengudroid deps` measures the
remaining gap per library.

## 4. Graphics: from `wl_shm` to GPU

The MVP path is `wl_shm` (CPU pixels), which is correct but slow and does not
give an app a GPU. Real game/app performance needs:

* `EGL`/`GLES` implemented on top of the **host** EGL/GLES (or Vulkan via
  `libvulkan`),
* `gralloc`/`AHardwareBuffer` mapped to `dmabuf`,
* `wl_shm` replaced with `zwp_linux_dmabuf_v1`.

None of that exists yet; only the buffer-ownership groundwork (the daemon owns
the allocation and the fd) is in place.

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
returns empty replies; the PipeWire pump is not written; `PackageManager` and
`ActivityManager` answer minimally. These are honest placeholders, not partial
implementations.

## 8. Security

There is **no** sandbox: guests are ordinary host processes with the user's
privileges and full host filesystem access. This is the opposite of Android's
process-per-app isolation. A production design would need to reintroduce
isolation (at least a seccomp filter and a private mount view) without
reintroducing a container. Do not run untrusted APKs under PenguDroid.

## 9. Protocol is versioned but young

The control protocol is v1 and hand-rolled. It is tested for round-trips and
truncation handling, but has no negotiation for capability differences and no
framing for large zero-copy transfers beyond fd passing.

## Roadmap (rough order)

1. ~~Load a single trivial Bionic `.so` under the shim and satisfy its imports.~~
   **Done**: `pengudroid-elf` reads imports and strips Android versioning; real
   x86-64 APK libraries (`librtb.so`, `libgtb.so`, `libnativeutil.so`) `dlopen`,
   and `fixtures/native-demo` is loaded **and called** with a correct result
   (see `docs/ARCHITECTURE.md` §8).
2. Close the *running* gap: Bionic-compatible teardown, a real `__sF`, and
   loading an APK's own `libc++_shared.so` via the compat path.
3. Bring up enough of the Android linker namespace to `dlopen` `libEGL` and bind
   it to host EGL, then present a GPU frame over `dmabuf`.
4. Attempt ART bring-up against a minimal `libandroid_runtime` with no
   `system_server`.
5. Wire the PipeWire audio pump.
6. Replace telephony/camera stubs with real AIDL marshalers.
7. Add seccomp + a private mount namespace for isolation without a full
   container.
