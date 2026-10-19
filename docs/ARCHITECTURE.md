# PenguDroid architecture

This document explains how each translation layer works, and where the honest
boundaries of the MVP are.

## The shape of the system

```mermaid
flowchart TB
    subgraph Host["Linux host session"]
      WL[Wayland compositor]
      PW[PipeWire]
      KB["/dev/binderfs/binder (if present)"]
      DMABUF[dmabuf / host GPU drivers]
    end

    subgraph Daemon["pengudroid-daemon (resident)"]
      IPC[control socket + SCM_RIGHTS]
      GFX[graphics broker]
      BND[binder broker]
      AUD[audio ring]
      PRP[property store]
      ASH[ashmem / memfd]
    end

    subgraph Guest["pengudroid __guest (ordinary process)"]
      SHIM[LD_PRELOAD Bionic shim]
    end

    CLI[pengudroid CLI] -->|Hello / RegisterProc| IPC
    Guest -->|CreateSurface / PresentFrame| IPC
    SHIM -.->|ashmem + props over socket| IPC
    CLI -->|fork + exec| Guest

    IPC --> GFX
    IPC --> BND
    IPC --> AUD
    IPC --> PRP
    IPC --> ASH
    GFX -->|wl_surface / xdg_toplevel| WL
    GFX -.->|future: wl_shm to dmabuf| DMABUF
    AUD -.->|stub: ring to sink| PW
    BND -.->|ioctl when available| KB
```

## 1. The daemon (`pengudroid-daemon`)

A single resident process, in the spirit of `wineserver`. It owns everything
that is expensive or shared, so guests start with no boot phase:

* **control socket** — a Unix socket (`$XDG_RUNTIME_DIR/pengudroid/daemon.sock`,
  mode `0600`) speaking the length-prefixed protocol in `pengudroid-proto`.
  Where a sandbox refuses `bind(2)` with `EPERM`, the daemon falls back to an
  abstract socket derived from the uid and file name; clients connect to either
  form through `Connection::connect_endpoint`.
* **fd passing** — ashmem regions are handed to guests as real file descriptors
  over `SCM_RIGHTS` (`pengudroid-socket`), so pixels are never copied through
  the control channel.
* **lifecycle** — accepts connections on a non-blocking listener, one thread per
  client, and exits cleanly on `SIGTERM`/`SIGINT` or a `Shutdown` message.

It is deliberately dependency-light (`libc` plus the Wayland client crates) so it
stays small and cheap to keep resident.

## 2. Binder

Android's IPC is binder. PenguDroid has two backends behind one interface:

* **Kernel binder** — if a binder device answers the `BINDER_VERSION` ioctl
  (`/dev/binderfs/binder`, `/dev/binder`, ...), the daemon can drive real
  `BINDER_WRITE_READ` transactions. The ioctl numbers and the
  `struct binder_write_read` layout are asserted at compile time (48 bytes) so
  the ABI cannot silently drift.
* **Userspace broker** — on desktop Linux the kernel module is frequently absent
  (`CONFIG_ANDROID_BINDER_IPC` off, or not loadable without root), so the daemon
  runs a broker that preserves the *shape* of binder — named services get
  integer handles, and transactions carry `(handle, code, flags, data)` — routed
  over the control socket.

Guests address platform services by name (`activity`, `package`,
`SurfaceFlinger`, `media.camera`, `phone`, ...). Each has a stub implementation
in `pengudroid-daemon::stubs` that returns a well-formed, honest answer (for
example "zero cameras" or "radio off") so probes do not hang.

A **guest may also publish its own service**. `RegisterService` binds the name
to the connection that registered it; a transaction on that handle is forwarded
to the owner as a `GuestCall`, the owner answers with `GuestCallReply`, and the
daemon relays it to the caller as a normal `BinderReply`. Both directions run on
each connection's own serving thread, which selects between its socket and an
inbox channel, so nested calls (A calling B while B waits on C) make progress
instead of deadlocking. When the owning process disconnects, its services are
dropped and later calls fall through to the broker's `-EINVAL`.

## 3. Graphics: replacing SurfaceFlinger with Wayland

On real Android, pixels flow
`EGL/GLES -> gralloc GraphicBuffer -> BufferQueue -> SurfaceFlinger -> display`.
**PenguDroid does not run SurfaceFlinger.** It collapses the chain:

1. A guest asks the daemon to `CreateSurface { width, height, title }`.
2. The daemon allocates an ashmem region (memfd), creates `wl_surface` plus
   `xdg_toplevel` on the user's compositor, wraps the region in a `wl_shm`
   buffer, attaches it, and commits.
3. The daemon returns the surface id **and the region fd** to the guest.
4. The guest `mmap`s the fd and writes pixels; `PresentFrame` asks the daemon to
   commit the same buffer to Wayland.

Because the guest and the compositor share one buffer, there is no copy through
a display server. `wl_shm` is the zero-GPU path; the upgrade to
`zwp_linux_dmabuf_v1` with host-Vulkan-allocated buffers (a Lepton-style driver
pass-through, but without a container) is a localized change in
`graphics.rs::finalize_surface`, since the daemon already owns the allocation and
its fd.

`xdg_wm_base` pings are answered (or the compositor drops us), `configure` is
acked before the first attach, and window-close events are tracked.

## 4. Audio

`AudioBroker` probes for `libpipewire-0.3.so.0` at runtime with `dlopen` (no
build-time headers) and allocates a PCM ring buffer as an ashmem region. The
transport and the availability probe are real; the thread that drains the ring
into a PipeWire virtual sink is the next milestone.

## 5. ashmem

Modern Android implements ashmem on top of memfd internally, and so does
PenguDroid: `memfd_create(MFD_CLOEXEC | MFD_ALLOW_SEALING)` plus `ftruncate`.
Pinning is emulated with `F_ADD_SEALS` of `F_SEAL_SHRINK | F_SEAL_GROW`, which —
as the tests confirm — makes a later `ftruncate` fail with `EPERM`. This is what
lets a guest safely map a region `PROT_EXEC` and rely on it not moving.

## 6. The Bionic to glibc shim

`pengudroid-shim` is an `LD_PRELOAD` shared object that exports Android's Bionic
symbol names so they interpose over the real ones in a guest process:

* logging — `__android_log_print`, `__android_log_write`, `__android_log_assert`
  (the variadic entry points are C shims generated by `build.rs`; they format
  with the host `vsnprintf`, so `"x=%d"` logs `x=42`)
* `ashmem_create_region` and friends, mapped to memfd
* `android_get_device_api_level`, reporting 36
* the `__system_property_*` family, backed by an in-process store seeded with
  Android 16 defaults

It carries a `.init_array` constructor so its load is visible.

## 7. The guest and the CLI

`pengudroid run app.apk`:

1. parses the APK's binary manifest (ZIP + AXML) for the package, SDK levels, and
   the MAIN/LAUNCHER activity;
2. ensures the daemon is running (starting it if needed);
3. `fork + exec`s itself as `pengudroid __guest ...` — a normal Linux process;
4. the guest asks for a surface, maps it, registers with the daemon for `ps`,
   and animates until interrupted.

Ctrl-C on the CLI propagates to the guest. `pengudroid ps` lists guests by
querying the daemon's process table, which is checked against `/proc` for
liveness.

`pengudroid start app.apk` takes the other route: the CLI sends `RunApk` and the
*daemon* forks `pengudroid __guest --apk ...`, so the guest is owned by the
daemon rather than the terminal. The daemon reaps the child, remembers its exit
status, and `pengudroid start --wait` (or any client via `QueryRun`) polls it.
Closing a guest's window on the compositor side sends `SIGTERM` to the guest's
pid, so the process tree stays clean in both modes.

## 8. Native libraries: measure, then load

An Android `.so` cannot be `dlopen`ed on glibc unless three independent
obstacles are cleared. All three were found empirically on real APK libraries.

### 8.1 Measure coverage (`pengudroid deps`)

`pengudroid-elf` reads a library's **undefined dynamic symbols** (imports) and
its **defined exports** from `.dynsym` (ELF32 and ELF64). `pengudroid deps`
compares those imports against the shim's *actual* exported symbols — read from
the built `libpengudroid_shim.so`, so the report cannot drift — and prints the
Bionic coverage and any gaps. On DroidFish's libraries the only Bionic imports
are `__errno` and `android_set_abort_message`, both covered.

### 8.2 Make symbol references unversioned (`de-bionic`)

Bionic versions every libc symbol against a single node per library — `libc.so`
→ `LIBC`, `libm.so` → `LIBM`, `libdl.so` → `LD` — while glibc exports granular
`GLIBC_2.x`. A Bionic object therefore both demands `version 'LIBC' not found`
and requires `free@LIBC`, `__cxa_finalize@LIBC`, ... which glibc's versioned
`free` cannot satisfy.

`pengudroid_elf::strip_android_versions` fixes the *per-symbol* half: it rewrites
every non-zero `.gnu.version` entry to *global* (`1`), so references become
unversioned and resolve normally against glibc (or the shim if it defines them).

Two plausible-looking alternatives were tried and rejected:

* **Zeroing the `VERNEED` record** does nothing useful — glibc walks the
  `vn_aux` chain unconditionally (ignoring `vn_cnt`), so it still finds `LIBC`.
* **Retagging `DT_VERNEED`** to an “unused” value is *dangerous*: some values are
  real tags (`0x7fffffff` is `DT_FILTER`, which made the loader try to load the
  library's own entry-point name as an auxiliary object; other values crashed in
  relocation). The requirement is instead satisfied honestly (see 8.3).

### 8.3 Satisfy the version requirement and map sonames (`libpath`)

The *file-level* requirement remains: the object's `VERNEED` says its `libc.so`
must provide `LIBC`. So the provider must actually carry that node. Rather than
ship a separate stub, the **shim itself** is given the nodes: its build script
(`crates/pengudroid-shim/build.rs`) links a version script assigning three
anchor symbols to `LIBC`, `LIBM` and `LD`, leaving the rest of its exports
global and unversioned.

`pengudroid libpath` then materialises a cache directory where every Bionic
soname — `libc.so`, `libm.so`, `libdl.so`, `liblog.so`, `libandroid.so`, ... —
is a symlink to the shim. That satisfies the requirement *and* supplies the
Bionic symbols; glibc's `libc.so.6` is already loaded in the process, so
unversioned references like `free` resolve from there. `libz`/`libstdc++` map to
the host libraries. The directory is used as `LD_LIBRARY_PATH`.

### 8.4 Put it together (`nativelib`)

`pengudroid nativelib <lib.so> [--call SYM]` runs both and then **actually
attempts the load**: it patches a copy, sets up the compat directory, and
re-execs itself as the hidden `__dlopen` command under `LD_PRELOAD=<shim>` +
`LD_LIBRARY_PATH=<compat>`. With `--call` it resolves the named export and
invokes it as a nullary `int`-returning function, printing the result. Verified
on real APK libraries (`librtb.so`, `libgtb.so`, `libnativeutil.so`, x86-64),
all of which `dlopen`, and on `fixtures/native-demo`, whose entry point returns
`500500` through the shim.

`nativelib --apk <app.apk> [--abi <abi>]` extends this from one file to a whole
application: it extracts an APK's `lib/<abi>/*.so` entries, strips each into a
shared scratch directory, and loads them as a set, so dependencies *between* an
app's own libraries (`DT_NEEDED libc++_shared.so`) resolve inside that directory.
Entries for other ABIs are reported as skipped, with the reason.

**What this does not mean:** loading is not running. Teardown
(`dlclose`/`atexit`) can crash because C++ static destructors assume Bionic
runtime state (observed on `librtb.so`, hence `__dlopen` exits via `_exit`
without running destructors). The `__sF` stdio array satisfies linkage but is
not ABI-compatible with glibc's `FILE`.

### 8.5 Running, not just loading

Three things separate *loading* a Bionic library from *running* it.

**Teardown.** C++ libraries register static destructors via `__cxa_atexit`; those
destructors assume Bionic runtime state and fault on glibc (this is what
segfaulted `librtb.so` on `dlclose`). The shim interposes `__cxa_atexit`,
`atexit`, `__cxa_thread_atexit_impl` and `__cxa_finalize`. When
`PENGUDROID_SUPPRESS_DESTRUCTORS` is set, registration is skipped and finalisation
is a no-op; otherwise the calls are forwarded to glibc via `dlsym(RTLD_NEXT)`.
`pengudroid nativelib --dlclose` sets it for guests and exercises the teardown
path, so the difference is observable (faulting destructor: 139 → 0).

**`__sF`.** Bionic exposes `FILE __sF[3]` with `stdout`/`stderr` as
`&__sF[1..2]`. glibc has no such symbol and a different `FILE` layout, so the
shim interposes the common stdio calls and rewrites an `__sF`-resident `FILE*`
to glibc's real stream before forwarding (the index comes from
`offset / 152`, Bionic's LP64 `sizeof(FILE)`). `fprintf` cannot build a
`va_list` in stable Rust, so it writes its format string literally; `vfprintf`
receives a real `va_list` and is fully forwarded. `fclose` on a standard stream
is a no-op so guest code cannot close the host's streams.

**Library sets.** An app that ships `libc++_shared.so` needs it loaded *with* its
dependents. `nativelib --apk` extracts and strips the whole `lib/<abi>` set into
one directory, so `DT_NEEDED` entries resolve inside it rather than against the
host. `fixtures/cpp-cascade` exercises exactly this: a consumer library depends
on the bundled runtime and returns `42` through it.

A build detail worth recording: the variadic log entry points are written in C
(stable Rust cannot define `...`), and Rust's cdylib export list hides
C-defined symbols while a static-archive member is dropped if unreferenced. The
build therefore links them with `--undefined` + `--export-dynamic-symbol` and
assigns them to the `LIBC` version node; without that, `__android_log_print` was
silently absent from the `.so`.

## 9. GPU path: host EGL/GLES and dma-buf presentation

`pengudroid-hostgpu` binds the **host's** graphics stack with `dlopen` — no
build-time EGL/GLES/GBM headers or link libraries. It resolves EGL and GLES
entry points (`eglGetProcAddress` first, then `dlsym`) and provides:

* [`info`](crate::GpuInfo) — EGL/GL vendor and version, GL renderer/version,
  and whether `libgbm`, dma-buf export/import, and the GBM/surfaceless
  platforms are available.
* `Offscreen` — an EGL context created on the **surfaceless** platform (Mesa's
  `EGL_MESA_platform_surfaceless`), with an RGBA8 FBO. `render` clears and
draws a triangle with a tiny GLES2 program; `read_pixels` reads the FBO back
  (used for verification); `export_dmabuf` turns the FBO's texture into an
  `EGLImage` and exports it with `EGL_MESA_image_dma_buf_export`, yielding an fd,
  stride, fourcc and DRM modifier.

Presentation is the zero-copy counterpart of the `wl_shm` path. The daemon
binds `zwp_linux_dmabuf_v1` (v3+, for modifiers); a guest renders on the GPU,
exports a dma-buf, and sends `PresentDmabuf` with the fd over `SCM_RIGHTS`. The
daemon builds a `zwp_linux_buffer_params_v1`, calls `create_immed`, attaches the
resulting `wl_buffer` to the **existing** surface, commits, and keeps the fd
alive. Nothing is copied through the CPU and no shared-memory pool is involved.

The guest runtime prefers this path when `Offscreen::new` succeeds and the
compositor supports the protocol, falling back to `wl_shm` otherwise. The
`--dmabuf-selftest` command exercises the whole chain and includes a negative
check (a plain memfd must be rejected) so a silent pass cannot mask a broken
acceptance verdict.

### 9.1 An app's own EGL calls (`pengudroid-gleshim`)

`crates/pengudroid-gleshim` is a Bionic-compatible **`libEGL`** (plus a small
`libGLESv2` surface). It resolves the **host's** EGL/GLES with `dlopen` and
implements EGL on a surfaceless host context:

* `eglGetDisplay` uses `EGL_MESA_platform_surfaceless`; `eglInitialize`,
  `eglChooseConfig`, `eglCreateContext`, `eglBindAPI` and the query/attribute
  calls forward to the host.
* `eglCreateWindowSurface` ignores the opaque `ANativeWindow` and allocates a
  virtual surface (`PENGUDROID_SURFACE_W`/`_H`, default 720×1280).
* `eglMakeCurrent` makes the host context current and binds an RGBA8 FBO for that
  surface, so the app's GL draws land there.
* `eglSwapBuffers` exports the FBO texture as a dma-buf
  (`EGL_MESA_image_dma_buf_export`, passing the **current context** to
  `eglCreateImageKHR` — `EGL_NO_CONTEXT` segfaults on iris) and sends
  `PresentDmabuf` to the daemon. The shim creates a surface on demand (or uses
  `PENGUDROID_SURFACE_ID`) and expects an `Ack`; if the daemon is absent it
  returns `EGL_FALSE` instead of hanging.

`EGL_GL_TEXTURE_2D_KHR` → dma-buf → `zwp_linux_dmabuf_v1` is the app-facing form
of the zero-copy path. `fixtures/egl-app` is a native program that loads the
shim, creates a context and window surface, clears 5 frames and swaps: it reports
`5 frames swapped (GPU -> dma-buf -> Wayland)`.

Still missing on this front: `gralloc`/`AHardwareBuffer` compatibility, hardware
composition, and Vulkan (the host Vulkan ICDs can be loaded, but no translation
layer drives them yet).

One deliberate shortcut: the compat directory maps `libEGL.so` to our shim but
`libGLESv2.so` to the **host driver**. Our shim makes the host context current
and binds its FBO, so direct `gl*` calls land in the right place — and the app
gets the *full* GLES entry-point surface rather than the handful the shim
exports. (Apps that resolve GL through `eglGetProcAddress` get host pointers
anyway.)
`libOpenSLES.so` resolves to the Bionic shim, which exports `slCreateEngine` and
the `SL_IID_*` identifiers so OpenSL ES users load; audio is not produced.

With those mappings and the fortify/NDK stubs, real application libraries load:
Dolphin's `libmain.so` (15 MB) and PPSSPP's `libppsspp_jni.so` (27 MB) both
`dlopen` under `pengudroid nativelib --apk`.

## Why this is faster than a container

Waydroid/Anbox boot an Android userspace inside an LXC container (or a VM):
`init`, zygote, `system_server`, SurfaceFlinger, and namespace setup, all before
the first app frame. PenguDroid has none of that. The daemon is already resident,
so launching an app is a `fork + exec` plus a socket round-trip: no Android boot,
no zygote, no namespace overhead, and no second display server.
