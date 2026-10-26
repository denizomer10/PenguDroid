# API documentation

This document describes PenguDroid's control protocol and CLI. See [LIMITATIONS.md](LIMITATIONS.md) for compatibility boundaries and [ARCHITECTURE.md](ARCHITECTURE.md) for the translation layers.

## Command line

| Command | Description | Status |
|---|---|---|
| `pengudroid daemon [--foreground]` | Start the daemon; exits quietly if it is already running. | Working |
| `pengudroid status` | Ping the daemon and report guest, service, and property counts. | Working |
| `pengudroid stop` | Stop the daemon and wait for its socket to disappear. | Working |
| `pengudroid demo` | Open the built-in demo window; no APK required. | Working; requires a Wayland session |
| `pengudroid native <apk>` / `--lib lib.so` | Run a NativeActivity app without ART; `--isolate` adds seccomp. | Working |
| `pengudroid launch <app.apk>` | Launch with an app-specific prefix under `~/.local/share/pengudroid/prefixes/<pkg>`. Reads overrides from `pengudroid.env` and selects the `native` or `art` backend from the APK. Supports `--backend`, `--prefix`, and `--sysroot`. | Working |
| `pengudroid extract <file> [--dest DIR] [--abi A]` | Extract `.apk`, `.apkm`, `.xapk`, or `.apks` packages, including base APK, ABI-filtered splits, and OBB data under `Android/obb/<pkg>/`. | Working |
| `pengudroid doctor <apk>` | Report whether an APK can run and what it is missing. | Working |
| `pengudroid sysroot <dir>` | Inspect an extracted Android system tree. | Working |
| `pengudroid art-test --sysroot <dir>` | Try `JNI_CreateJavaVM`; also supports `--classpath <apk>`, `--find-class <C>`, `--main <C>`, `--jni-lib <so>`, and `--register-natives <so>`. | Working; returns a live `JavaVM` (`JNI_OK`) |
| `pengudroid run app.apk` | Run the guest under the CLI and stop it with Ctrl-C; reports an error for a missing APK. | Working; requires a Wayland session |
| `pengudroid start app.apk [--wait]` | Run the guest under the daemon; `--wait` waits for its exit code. | Working |
| `pengudroid start --demo [--wait]` | Run the built-in demo under the daemon. | Working |
| `pengudroid key <linux-keycode> [down|up]` | Send a physical key through the daemon. Without a state, sends press and release; separate states support Shift/Ctrl chords. | Working; virtual device, character map, and NewPipe text entry verified |
| `pengudroid ps` | List running guests from the daemon's process table. | Working |
| `pengudroid services` | List broker services with handle, name, and interface. | Working |
| `pengudroid getprop [NAME]` | Read one property, or list all properties if `NAME` is omitted. | Working |
| `pengudroid setprop NAME VALUE` | Set a property; it is saved when the daemon exits. | Working |
| `pengudroid info app.apk` | Print manifest fields. | Working |
| `pengudroid deps lib.so` | Report Bionic import coverage. | Working |
| `pengudroid nativelib …`, `libpath`, `de-bionic` | Load APK native libraries on glibc and prepare the compatibility library path. | Working |
| `pengudroid gpu-info`, `gpu-test` | Inspect host EGL/GLES and test dma-buf export. | Working |
| `pengudroid-daemon --wayland-selftest` | Test the Wayland connection and dma-buf extensions. | Working |
| `pengudroid-daemon --dmabuf-selftest` | Render on the GPU and submit the dma-buf to the compositor. | Working |
| `pengudroid video-decode-test <one-frame.h264> [--device NODE] [--verify-gradient]` | Require VA-API H.264 decode and synchronized multi-plane GPU export; optional fixture readback. | Host-only test; guest Codec2 adapter pending |

## Runtime keymapper preference

ART launches load the package's saved keymap automatically. Set
`PENGUDROID_KEYMAPPER=0` to disable profile loading and retain ordinary key
input. The desktop launcher sends `0` or `1` from the selected app's checkbox;
changes apply on the next launch. `PENGUDROID_KEYMAP_PATH` is the resolved
profile path passed to the guest before its prefix changes HOME.

## Protocol (v2)

Transport uses a non-empty `PENGUDROID_SOCKET` when set; otherwise it uses `$XDG_RUNTIME_DIR/pengudroid/daemon.sock` with mode `0600`. Set `PENGUDROID_SOCKET` to run a private daemon while keeping `XDG_RUNTIME_DIR` pointed at the real Wayland/PipeWire session. The CLI, daemon, and guest must use the same socket value.

Frames use a length prefix: `[u32 body_len][body]`, where `body = [kind u8][fields…]`. File descriptors travel out of band through `SCM_RIGHTS`. Every connection begins with `Hello`/`Welcome`; `Hello.role` is `Cli`, `Shim`, or `Guest`.

| Message | Direction | Description |
|---|---|---|
| `Ping` / `Pong` | Either | Liveness check. |
| `RunApk { path, component?, args }` → `RunStarted { pid, package, surface_id }` | CLI → daemon | Fork a guest process under the daemon. |
| `QueryRun { pid }` → `RunStatus { pid, running, exit_code, signal }` | CLI → daemon | Get the state of a forked guest. |
| `RegisterProc` / `UnregisterProc` / `ListProcs` → `ProcList` | Guest/CLI | Maintain and read the `ps` process table. |
| `AllocShmem { name, size }` → `ShmemAllocd { name, size, seam }` + fd | Guest → daemon | Allocate ashmem backed by a memfd. |
| `RegisterService { name, iface }` → `ServiceRegistered { handle }` | Guest → daemon | Publish a guest service; platform service names are reserved. |
| `ListServices` → `ServiceList { (name, iface, handle)… }` | CLI → daemon | Read the service table. |
| `BinderTransact { handle, code, flags, data }` → `BinderReply { status, data }` | CLI/guest → daemon | Route to a stub or the guest that owns the service. |
| `GuestCall { code, data, token }` / `GuestCallReply { status, data }` | Daemon ↔ guest | Deliver a guest service call and its reply. |
| `CreateSurface { width, height, title }` → `SurfaceCreated { … }` + fd | Guest → daemon | Create a Wayland toplevel and shared buffer. |
| `SetSurfaceOptions { surface_id, fps_meter }` → `Ack` | Guest → daemon | Set per-window FPS title display for a surface owned by this connection's process. |
| `PresentFrame { surface_id }` → `Ack` | Guest → daemon | Commit the shared buffer. |
| `PresentShm { surface_id, width, height, stride, format }` + fd → `Ack` | Guest → daemon | Snapshot a CPU memfd buffer and present it through `wl_shm`. Android formats 1/2/4/5 are supported; stride is in bytes. `Ack` means the source buffer may be reused; it does not prove that the frame appeared on screen. |
| `GetProp` / `SetProp` / `ListProps` | CLI/guest | Access the property store. |
| `Shutdown` → `Goodbye` | CLI → daemon | Shut down cleanly. |

Errors are returned as `Error { code, message }`, where `code` is a negative errno: `-2` file not found, `-3` unknown PID, `-5` spawn or fd error, `-17` name conflict, `-19` graphics unavailable, `-22` unknown handle, `-32` guest disconnected, and `-110` guest service timeout (5 seconds).

## Local verification

Build and run the workspace checks with:

```sh
cargo build --workspace
cargo test --workspace
```

Tests call the daemon dispatch table through a real `Daemon` instance without opening a process or socket, so they also work in restricted sandboxes.

## Host hardware video API

`pengudroid_hostgpu::video::Decoder` loads the optional host-native bridge and accepts complete compressed access units, flush/EOS and microsecond timestamps. `receive` returns a GPU-owned `HardwareFrame`, input/EOS status, or bounded-frame backpressure. `HardwareFrame::export` duplicates DMA-BUF descriptors and preserves all plane metadata. A frame can outlive the decoder.

`Graphics::present_video_dmabuf` imports a synchronized top-down multi-plane frame directly. Its returned release flag becomes true only at `wl_buffer.release`; the caller must retain the source hardware frame until then. `None` means the frame was dropped before import due to compositor backpressure. This direct host API is not a new socket message and is not yet used by NewPipe. See [HARDWARE_VIDEO.md](HARDWARE_VIDEO.md).

## Hosted settings provider

Known scalar GET replies contain the string under Bundle key `value`. Unknown
scalar values return an empty Bundle. Unsupported `LIST_config` returns a null
Bundle, which Android 13 treats as an empty result. Settings writes and complete
IContentProvider AIDL argument parsing are not implemented.

## Platform migration target

The forward platform target is Android 17 / API 37. A separate local API-37 tree
boots and passes core probes; the default app runtime remains API 33 until the
window/media migration gates pass. See [ANDROID17.md](ANDROID17.md). Hosted audio-focus transaction numbers
are resolved from the loaded framework's generated Stub at ART binding, not from
the APK targetSdk. A complete Android 17 Binder/media adapter is still pending.
