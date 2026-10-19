# PenguDroid

<img src="logo.png" alt="PenguDroid logo" width="180">

**A no-KVM, no-container, no-VM Android compatibility layer for Linux — in Rust.**

PenguDroid runs Android applications as ordinary Linux processes. There is no
virtual machine, no LXC/Waydroid-style container, no namespace sandbox, and no
booted `system.img`. Like Wine/Proton translate Windows to POSIX, PenguDroid
translates Android's user-space interfaces — binder, Bionic libc, the surface
stack, audio — directly onto Linux primitives and onto the user's own session
(Wayland, PipeWire, the host's Vulkan/GL drivers).

A small always-on daemon (`pengudroid-daemon`, analogous to Proton's
`wineserver`) brokers shared resources and stays resident; apps come and go as
normal processes that you can see in `ps aux`.

```
$ pengudroid run app.apk
package:      com.example.app
targetSdk:    36
main activity:com.example.app/.MainActivity
started guest: pid 26829 (com.example.app)
the window is live; press Ctrl-C to stop the guest

$ pengudroid ps
PID      PACKAGE                      SURFACE  COMPONENT
26829    com.example.app              1        com.example.app/.MainActivity

$ ps -eo pid,comm,args | grep pengudroid
26822 pengudroid-daem  pengudroid-daemon --daemonize
26829 pengudroid       pengudroid __guest --package com.example.app ...
```

---

## Status: what actually works vs. what is stubbed

This is an **MVP skeleton with a genuinely working spine**. The infrastructure
is real and tested; the Android runtime integration is the next (very large)
milestone. Nothing below is overstated.

| Area | Status | Notes |
|---|---|---|
| Control daemon, lifecycle, `--daemonize`, signals | **Working** | ~few-ms startup, no boot phase |
| Unix-socket protocol, framing, `SCM_RIGHTS` fd passing | **Working** | tested with real memfds |
| `ashmem` via `memfd_create` + seals | **Working** | GROW/SHRINK seals verified to block `ftruncate` |
| Kernel binder probe (`BINDER_VERSION` ioctl) + fallback | **Working** | detects `/dev/binderfs/binder`; falls back to userspace broker on desktop kernels |
| Userspace binder broker: named services, transactions | **Working** | `pengudroid services` lists the table; platform stubs answer transactions |
| Guest↔guest service calls (forwarded over the control socket) | **Working** | a guest registers a service, another connection transacts on its handle; covered by an integration test with a real daemon |
| Android property store (`getprop`/`setprop`, enumeration) | **Working** | Android 16 defaults; `getprop` with no name lists everything |
| APK + binary `AndroidManifest.xml` parsing | **Working** | ZIP (store/deflate) + AXML parser, unit-tested |
| `pengudroid deps`: ELF import / Bionic-coverage analysis | **Working** | parses real arm64 / armv7 / x86-64 APK libraries; predicts `dlopen` outcome |
| Loading **real Android APK libraries** on glibc (`de-bionic` + `libpath` + `nativelib`) | **Working** | real x86-64 APK libraries (`librtb.so`, `libgtb.so`, `libnativeutil.so`) `dlopen` successfully |
| Loading a native `.so` under the shim (`LD_PRELOAD`) | **Working** | `dlopen` and `dlclose` verified |
| Bionic teardown (C++ static destructors) | **Working** | suppressed on request (`PENGUDROID_SUPPRESS_DESTRUCTORS`); `--dlclose` on a faulting destructor goes 139 → 0 |
| `__sF` standard streams | **Partial** | `stdin`/`stdout`/`stderr` via `__sF` map to glibc streams; other `FILE` uses are unsupported |
| **Calling a real Android-style library and getting a correct result** | **Working** | `fixtures/native-demo` returns 500500 after `de-bionic` + load, via the shim |
| **Loading an application's own native libraries (`nativelib --apk`)** | **Working** | extracts an APK's `lib/<abi>/*.so`, strips them, and loads the set; `fixtures/cpp-cascade` proves a consumer resolves the bundled `libc++_shared.so` (= 42) |
| Wayland graphics broker (compositor, `wl_shm`, `xdg_wm_base`, toplevels) | **Working** | connects to the live session; surfaces + `wl_shm` buffers |
| Guest process: owns a window, `ps`-visible, animated | **Working** | the `__guest` runtime; the compositor closing a window stops the guest |
| Guest launched by the **daemon** (`RunApk`, `pengudroid start --wait`) | **Working** | daemon forks the CLI's `__guest`, reaps it, and reports its exit through `QueryRun`; an exited guest's window is destroyed and it leaves `ps` |
| Fallback when the sandbox forbids `bind(2)` | **Working** | the daemon retries on an abstract (`@name`) Unix socket and clients connect to it transparently |
| `pengudroid demo` | **Working** | built-in animated window, no APK and no manifest needed |
| Host GPU via EGL/GLES/GBM (`gpu-info`, `gpu-test`) | **Working** | dlopen'd host EGL/GLES/GBM; offscreen render + pixel readback verified on Intel UHD 630 |
| Zero-copy GPU presentation (`zwp_linux_dmabuf_v1`) | **Working** | the guest renders on the GPU, exports a dma-buf, the daemon imports and commits it |
| Bionic `libEGL`/`libGLESv2` shim (`pengudroid-gleshim`) | **Working** | exports the EGL API; an app's EGL/GL calls render on the host GPU and swap into Wayland as dma-bufs — `fixtures/egl-app` swaps 5 frames |
| Real apps' native libraries load | **Working** | after soname mapping + Bionic stubs: Dolphin `libmain.so` (15 MB) and PPSSPP `libppsspp_jni.so` (27 MB) both `dlopen` |
| OpenSL ES (audio) | **Working** | engine/output-mix/buffer-queue player implemented; `Enqueue` forwards PCM to `pw-cat`; `fixtures/opensl-app` verifies PLAYING + callbacks against the canonical headers |
| `LD_PRELOAD` Bionic→glibc shim (`libashmem`, logging, props, API level) | **Working** | interposes real Bionic symbol names; `__android_log_print` formats with the host `vsnprintf` |
| Service registry + `phone`/`media.camera`/AMS/PMS/SurfaceFlinger stubs | **Stub** | correctly-named interfaces, minimal replies |
| Audio: PipeWire | **Working** | the daemon owns a shared ring and drains it into `pw-cat` (or `aplay`); `audio-test [--via-daemon]` streams PCM |
| Telephony / camera | **Stub** | report "radio off" / "zero cameras" |
| **Running real DEX bytecode (ART)** | **Not started** | the central open problem; see `docs/LIMITATIONS.md` |
| **Real Bionic/EGL in a guest** | **Not started** | needs the Android userspace `.so`s loaded in-process |

The honest framing of "no `system.img`": the *container/VM* image is gone, but
Android apps still need ART and Bionic *binaries* from somewhere. PenguDroid's
design goal is to load those as host libraries under a translation shim rather
than boot them as a guest OS. See `docs/ARCHITECTURE.md`.

---

## Build

Requires a Rust toolchain (stable, 1.75+). No system `-dev` packages are needed
for the default build — the Wayland client backend is pure Rust.

```sh
cargo build --release
```

Binaries land in `target/release/`:

* `pengudroid-daemon` — the resident broker
* `pengudroid` — the CLI
* `libpengudroid_shim.so` — the `LD_PRELOAD` Bionic translation shim

## Usage

```sh
# Start the daemon (or let `run`/`start` start it for you).
pengudroid daemon

# Inspect an APK without running it.
pengudroid info app.apk

# Report how well the shim covers a native library's Bionic imports.
pengudroid deps lib/arm64-v8a/librtb.so

# Try to load a real Android library on glibc (de-bionic + bionic-compat + dlopen).
pengudroid nativelib lib/x86_64/librtb.so

# Load every native library an APK ships for an ABI.
pengudroid nativelib --apk app.apk --abi x86_64

# Print the Bionic-soname compat directory (for LD_LIBRARY_PATH) / strip versioning.
pengudroid libpath
pengudroid de-bionic in.so out.so

# See it work with no APK at all: the built-in demo window.
pengudroid demo

# Launch an app as a native Wayland window; Ctrl-C stops the guest.
pengudroid run app.apk

# Let the daemon fork the guest instead (survives the CLI exiting); `--wait`
# blocks until it exits and prints the exit status.
pengudroid start app.apk --wait
pengudroid start --demo --wait

# Inspect the host GPU and prove the GPU path renders (offscreen + dma-buf).
pengudroid gpu-info
pengudroid gpu-test

# Audio: stream PCM (silence by default; --tone is audible).
pengudroid audio-test --seconds 1
pengudroid audio-test --tone

# Manage guests, services and properties.
pengudroid ps
pengudroid services
pengudroid getprop                           # every property
pengudroid getprop ro.build.version.sdk      # -> 36
pengudroid setprop debug.pengudroid 1
pengudroid status
pengudroid stop
```

Health-check the graphics path without opening a window:

```sh
pengudroid-daemon --wayland-selftest
# OK: connected to the Wayland compositor and bound required globals
```

Using the shim with a guest process:

```sh
LD_PRELOAD=$PWD/target/release/libpengudroid_shim.so your-android-binary
```

## Loading and running native libraries

Android `.so` files are Bionic-linked, and glibc refuses to load them for two
independent reasons, both found empirically against real APK libraries:

1. **Symbol versioning.** Bionic versions every libc symbol against a single
   node per library — `libc.so` → `LIBC`, `libm.so` → `LIBM`, `libdl.so` → `LD`
   — while glibc exports granular `GLIBC_2.x`. So an object both demands
   `version 'LIBC' not found` and requires `free@LIBC` / `__cxa_finalize@LIBC`,
   which glibc's versioned `free` cannot satisfy.
2. **Soname mismatch.** `DT_NEEDED` names like `libc.so`, `libdl.so` do not
   exist on a modern glibc system (`libc.so` is a linker script; `libdl.so` was
   merged into libc).

A few Bionic symbols glibc simply lacks (`__strlen_chk`, `__sF`, ...) are the
third issue, handled by the shim.

```
$ pengudroid deps lib/x86_64/librtb.so        # measure Bionic coverage vs the shim
bionic coverage: 2/2 (100%)

$ pengudroid nativelib fixtures/native-demo/lib/libandroid_demo.so \
      --call pengudroid_demo_run             # strip, load, and call
library: .../libandroid_demo.so (ELF64, x86-64)
OK: dlopen succeeded
CALL pengudroid_demo_run() = 500500
```

The mechanism has two parts:

* **`pengudroid de-bionic` / `pengudroid_elf::strip_android_versions`** rewrites
  every non-zero `.gnu.version` entry to *global*, so symbol references become
  unversioned and resolve normally against glibc (or the shim).
* **`pengudroid libpath`** materialises a compat directory in which the Bionic
  sonames (`libc.so`, `libm.so`, `libdl.so`, `liblog.so`, ...) all resolve to
  the **shim**, which carries the `LIBC`/`LIBM`/`LD` version nodes via a linker
  version script (`crates/pengudroid-shim/build.rs`) and provides the Bionic
  symbols. glibc's own `libc.so.6` is already loaded in the process, so
  unversioned references like `free` resolve from there.

`nativelib` runs both and then actually attempts the load (re-execing as the
hidden `__dlopen` command under `LD_PRELOAD` + `LD_LIBRARY_PATH`), optionally
calling a nullary `int`-returning export with `--call`.

### An application's own libraries (`nativelib --apk`)

`pengudroid nativelib --apk app.apk --abi <abi>` extracts every native library
an APK ships for that ABI, strips them into one scratch directory, and loads
them as a set — so dependencies *between* the APK's libraries (for example a
library that `DT_NEEDED`s the APK's own `libc++_shared.so`) resolve inside that
directory rather than failing. Cross-architecture entries are reported as
`[SKIP]` with the reason instead of a confusing load failure:

```
$ pengudroid nativelib --apk droidfish.apk --abi x86_64
/tmp/droidfish.apk: 3 native libraries for x86_64
  [ OK ] libgtb.so
  [ OK ] libnativeutil.so
  [ OK ] librtb.so
/tmp/droidfish.apk: 3 loadable, 0 skipped, 0 failed
```

**Teardown.** Bionic C++ libraries register static destructors that assume Bionic
runtime state; running them on glibc is what segfaulted `librtb.so` during
`dlclose`. The shim interposes `__cxa_atexit`/`atexit`/`__cxa_finalize` and,
when `PENGUDROID_SUPPRESS_DESTRUCTORS=1` (which `nativelib` sets for guests),
skips registration. Verified: `nativelib --dlclose` on a faulting destructor goes
from SIGSEGV (139) to a clean exit when suppression is enabled.

**`__sF`.** Bionic's `FILE __sF[3]` is mapped onto glibc's
`stdin`/`stdout`/`stderr` by interposing `fprintf`/`vfprintf`/`fputs`/`fwrite`/
`fputc`/`fflush`/`fclose` (the mapping assumes Bionic's LP64 `sizeof(FILE)` =
152). Only the three standard streams are supported, and `fprintf` does not
format varargs (use `vfprintf`, which does).

**An application's own libraries.** `nativelib --apk` stages an APK's whole
`lib/<abi>` set in one directory, so intra-APK dependencies resolve.
`fixtures/cpp-cascade` proves it: a consumer that `DT_NEEDED`s the bundled
`libc++_shared.so` loads, and `consumer_run()` returns `42`.

**`pengudroid-elf`** reads ELF32 and ELF64 (armv7, aarch64, x86, x86-64) with no
external dependencies.

### Reproducible test

`fixtures/native-demo/` builds a small *Android-style versioned* library (a stub
`libc.so` provides version `LIBC`) with an entry point returning `500500`, and
`fixtures/native-demo/run-test.sh` loads and calls it through PenguDroid — no
network or real APK required.

## GPU path (zero-copy)

PenguDroid binds the **host's** EGL/GLES/GBM at runtime (no `-dev` headers —
everything is `dlopen`'d), so guest graphics drive the real GPU instead of a
software rasteriser. `pengudroid gpu-info` reports the driver; `pengudroid
gpu-test` renders offscreen and verifies the pixels, then exports a dma-buf:

```
$ pengudroid gpu-info
GL renderer:      Mesa Intel(R) UHD Graphics 630 (CFL GT2)
GL version:       OpenGL ES 3.2 Mesa 25.0.7
zero-copy ready:  yes

$ pengudroid gpu-test
rendered 128x128 on the GPU; corner pixel RGBA = (26,51,230,255)
pixel verification: OK
dmabuf: 128x128 stride 512 fourcc 0x34324241 modifier 0x100000000000002 offset 0 fd 7
zero-copy hand-off: OK
```

When a compositor advertises `zwp_linux_dmabuf_v1`, `pengudroid run` uses the
guest's GPU path: the guest renders into an FBO on the host GPU, exports it as a
dma-buf, and sends it to the daemon (`PresentDmabuf`, fd via `SCM_RIGHTS`),
which imports it into the existing Wayland surface and commits — no CPU copy and
no `wl_shm`. If the GPU or the protocol is unavailable it falls back to the
`wl_shm` client path.

```
$ pengudroid run app.apk
guest pid 9734: rendering on the host GPU via dma-buf
guest: first dma-buf presented (720x1280, fourcc 0x34324241) — zero-copy
```

Health-check it directly:

```sh
pengudroid-daemon --wayland-selftest   # reports linux-dmabuf availability
pengudroid-daemon --dmabuf-selftest    # GPU render -> dma-buf -> compositor
```

## Audio path

The daemon owns a shared PCM ring (an ashmem region) at 48 kHz / stereo / S16_LE.
Output goes to the session's **PipeWire** through its own `pw-cat` tool, driven
over a pipe (with `aplay` as a fallback) — a real stream in the PipeWire graph
without binding `libpipewire`'s large C ABI.

```
$ pengudroid audio-test --seconds 1
audio: pw-cat @ 48000 Hz, 2 ch, 1.00s of silence
audio: stream completed cleanly
```

Silence is the default so the check cannot disturb anyone; `--tone` plays a quiet
440 Hz sine. Add `--via-daemon` to exercise the full architecture — the client
maps the daemon's ring, fills it, and sends `AudioWrite`:

```
$ pengudroid audio-test --via-daemon --seconds 1
audio: sent 192000 bytes through the daemon ring (48000 Hz, 2 ch)
```

What remains is the broader OpenSL ES surface: the buffer-queue player works, but
recorders, prefetch, effects and the other interfaces are still
`SL_RESULT_FEATURE_UNSUPPORTED`.

## Repository layout

```
crates/
  pengudroid-proto/    wire protocol, ABI constants, no dependencies
  pengudroid-socket/   Unix-socket transport with SCM_RIGHTS fd passing
  pengudroid-elf/      ELF dynamic-symbol reader + Android version stripping
  pengudroid-daemon/   the resident broker (binder/graphics/audio/props)
  pengudroid-cli/      the `pengudroid` command, APK/AXML parser, guest runtime
  pengudroid-shim/     LD_PRELOAD Bionic->glibc translation shim
fixtures/
  native-demo/         a reproducible Android-style versioned test library
docs/
  ARCHITECTURE.md      how each translation layer works
  API.md               CLI surface and the control protocol, in one place
  LIMITATIONS.md       the hard problems, stated plainly
```

## Testing

```sh
cargo test --workspace
```

Unit tests cover the wire protocol round-trips, `SCM_RIGHTS` fd passing, socket
timeouts, ashmem sealing, binder ioctl-number correctness, the property store,
service stubs and guest-service ownership, daemon dispatch (including guest
calls, `RunApk` validation and process bookkeeping), ZIP + AXML parsing, and the
shim's property/ashmem/log-formatting entry points.

The shim's variadic `__android_log_print` / `__android_log_assert` are compiled
from a small C file generated by `crates/pengudroid-shim/build.rs` (Rust cannot
define C-variadic functions on stable). Without a C compiler the crate still
builds, minus those two entry points, and says so with a cargo warning.

## License

Apache-2.0.
