# Project status and handoff

This is the current handoff note. It records verified behavior, known limits, and the next concrete task. See [ARCHITECTURE.md](ARCHITECTURE.md) for implementation details and [LIMITATIONS.md](LIMITATIONS.md) for broader compatibility gaps.

## Where we left off — the last null-reply NPE classes are shimmed (2026-11-13)

Pick up here. A 280 s BBR2 smoke (`/tmp/bbr2-smoke3.log`) showed three remaining
NPE classes, all caused by status-only fallthrough replies decoding as null:
all are now shimmed in `pengudroid-shim` and installed from the real framework
`TRANSACTION_*` fields at apphost startup:

| Gap | Codes | Shim reply |
|---|---|---|
| `shortcut` `getShortcuts`/`getShareTargets` (NPE in `ShortcutManager.getDynamicShortcuts` on `ParceledListSlice.getList()`) | 23 / 20 (read from `IShortcutService$Stub`) | status + `[presence=1][count=0]` — empty `ParceledListSlice` |
| `jobscheduler` `getAllPendingJobsInNamespace`/`getAllJobSnapshots` (NPE in `JobSchedulerImpl.getAllPendingJobs`) | 8 / 17 (read from `IJobScheduler$Stub`) | same empty-slice shape |
| `graphicsstats` `requestBufferForProcess` (NPE in `HardwareRenderer` on `ParcelFileDescriptor.getFd()`) | 1 (read from `IGraphicsStats$Stub`) | status + `[presence=1][hasComm=0][flat BINDER_TYPE_FD]` wrapping a fresh zeroed ashmem memfd of `0x248` bytes — `nGetAshmemSize()` in the shipped `libhwui.so` returns exactly `0x248`, and the client rejects smaller regions ("Ashmem region is too small") |

Wire formats were verified against the API 37 dex/libhwui: Proxy calls
`readException()` + `readTypedObject(PFD.CREATOR)`; `ParcelFileDescriptor.writeToParcel`
writes `[hasComm][writeFileDescriptor]`; `ParceledListSlice.writeToParcel` is
`writeTypedList` = `[count]`.

Validation on this checkout: `cargo test --offline -p pengudroid-shim` **157
passed**, `cargo test --offline -p pengudroid-cli` green (94+7+2+3+2), and the
280 s BBR2 smoke with **zero `Fatal signal`/`SIGBUS`/`SIGSEGV`, zero
`UNKNOWN_TRANSACTION`, zero `ParceledListSlice.getList()` NPEs and zero
"Could not acquire gfx stats buffer" NPEs**; apphost logs
`empty-list shortcut=[23,20]`, `empty-list jobscheduler=[8,17]`,
`fd-reply graphicsstats=[1]`. Remaining noise is benign (aconfig mmap WARNs,
`classes.dex` entry misses, GMS/Tenjin ads SDK complaints, 2x
`libbinder.Parcel: Expecting binder but got null!` after app-thread capture on
handle 17, "Failed to load FMOD libs").

### Next concrete task

Hunt the next gap from fresh BBR2 smoke logs (candidates: the two
"Expecting binder but got null!" warnings around handle 17 — likely a service
whose reply must carry a binder object; `Settings$SettingNotFoundException:
limit_ad_tracking`; FMOD native lib loading). Then re-run the smoke set
(BBR2 launch, resize via `SetWindowState`, tablet classification). The big
blocker is unchanged and lives outside this repo: image mode needs the ARM64
translator to resume after a delivered guest signal (see the uffd section
below). `Constructor.getReturnType()` stays a logged upstream guest bug — do
not shim it.

## Where we left off — the last three unimplemented binder transactions are now shimmed (2026-11-13)

Pick up here. The three remaining `UNKNOWN_TRANSACTION` sources seen in BBR2
logs are implemented in `pengudroid-shim` and installed from the real framework
`TRANSACTION_*` fields at apphost startup:

| Gap | Code | Shim reply |
|---|---|---|
| `IAudioService.setAllowedCapturePolicy` / `getAllowedCapturePolicy` | 171 / 172 (read from `IAudioService$Stub`, non-contiguous in API 37) | state held in `audiofocus::CapturePolicy` (default `ALLOW_CAPTURE_BY_ALL`=1); set validates 1..=3 → `SUCCESS`=0, else `ERROR_BAD_VALUE`=-2 and keeps the old value; get returns the current policy. The policy int is read after the `android.media.IAudioService` interface token (`after_interface_token`) |
| `IInputManager.getVelocityTrackerStrategy` | 1 | parcelled String16 `"default"` — `VelocityTracker.toStrategyId` maps any unrecognized name to DEFAULT, so this is always accepted (input table widened to 9 ops, op 8) |
| `IDisplayEventConnection.getLatestVsyncEventData` | 4 (native-only AIDL, no Java Stub; declaration order) | `VsyncEventData` wire layout measured in the shipped x86-64 `libgui.so` (`ParcelableVsyncEventData::readFromParcel`): status + presence 1 + preferred index 0 + count 1 + one `{vsyncId=1, deadline, expected}` timeline on `CLOCK_MONOTONIC` at the next 60 Hz pulse. Does **not** push a vsync event into the stolen channel |

Validation on this checkout: `cargo test --offline -p pengudroid-shim` **155
passed** (+ new contract tests for all three replies), `cargo test --offline
-p pengudroid-cli` green (94+7+2+3+2), and a 170 s BBR2 smoke run
(`pengudroid launch <cobalt apkm> --backend auto`) with **zero
`Fatal signal`/`SIGBUS`/`SIGSEGV` and zero `UNKNOWN_TRANSACTION`**; the apphost
log shows the real installed codes `audio=[82,83,84,85,171,172]`,
`input=[2,3,0,4,5,6,7,39,1]`.

### Next concrete task

Re-run the Genshin-adjacent smoke set (BBR2 launch, resize via
`SetWindowState`, tablet classification) and keep closing whatever the next
`UNKNOWN_TRANSACTION` turns out to be. The big blocker is unchanged and lives
outside this repo: image mode needs the ARM64 translator to resume after a
delivered guest signal (see the uffd section below). `Constructor.getReturnType()`
stays a logged upstream guest bug — do not shim it.

## Where we left off — boot image loads, but uffd GC cannot run under the bridge (2026-11-13)

Pick up here. The x86_64 boot image from the Android 17 GSI **validates and
loads**, which retires the "impossible to load" verdict in
[ANDROID17.md](ANDROID17.md). Getting there needed three fixes, all shipped:

| Fix | Evidence |
|---|---|
| `gUseReadBarrier = !gUseUserfaultfd` is derived, not a flag | `objdump` of the sole store (`0x5873be`); `KernelSupportsUffd()` probes `syscall(323)` with `UFFD_USER_MODE_ONLY`, which is allowed with `vm.unprivileged_userfaultfd=0` |
| Toggling `ro.dalvik.vm.enable_uffd_gc` clears the read-barrier rejection | `PENGUDROID_PROP_ro.dalvik.vm.enable_uffd_gc=true`: the `ValidateOatFile found read barrier state mismatch` error disappears |
| The boot classpath must match the image's OAT header exactly | Parsed the ELF-wrapped OAT headers: 14 entries, `core-icu4j.jar` **last**. `Layout::detect` and its unit test now encode that order |
| With all three, the image is adopted | `-verbose:image` decompresses all 14 components, and `ICU4J` registers (`libicu_jni.so`, `I18n APEX ICU file found`) |

### Why image mode still cannot ship

The image forces `gUseReadBarrier=false`, which forces userfaultfd GC, and this
`libart.so` is a **uffd-CMC-only build**: `-Xgc:MS` and `-Xgc:SS` abort with
`Check failed: foreground_collector_type_ == kCollectorTypeCMC`, and clearing
`UFFD_FEATURE_SIGBUS` from the `UFFDIO_API` reply only brings the read-barrier
rejection back (`oat file: false, runtime: true`). There is no image mode without
uffd.

The first *compacting* collection then dies:

```
post_compact_end:0x2092000     <- compacted space ends here
Fatal signal 7 (SIGBUS), code 2 (BUS_ADRERR) fault addr 0x2092378
FATAL art: runtime_common.cc:466] HandleUnexpectedSignal reenter
```

Traced with `strace -f -e trace=madvise,mremap,userfaultfd,ioctl`:

* the GC thread calls `UFFDIO_API` (features `UFFD_FEATURE_SIGBUS`) on a fresh
  uffd, then `UFFDIO_REGISTER [0x2000000, 0x209a000)` — note the end is
  `post_compact_end` rounded *past*, into reserved-but-unused space;
* a *mutator* thread faults at an address inside that registered range but
  **outside the copying plan** (it is filler/free space above the compacted
  end), *before* the GC thread issues the `UFFDIO_COPY` calls;
* `BUS_ADRERR` is exactly what `UFFD_FEATURE_SIGBUS` produces by design — the
  kernel sends `SIGBUS` so ART's handler can populate the page. ART's handler
  cannot map that address to a plan entry, so it declares the fault fatal, and a
  second fault during the fatal report produces `reenter`.

Two things were ruled out along the way:

| Ruled out | How |
|---|---|
| A kernel/`mremap(DONTUNMAP)` bug | A standalone C repro replays ART's exact sequence (mmap 512 MB → `MREMAP_DONTUNMAP|FIXED` → register MISSING → `madvise(DONTNEED)` → `UFFDIO_COPY` back → unregister → write past the copy-back end): every write succeeds |
| The shim stealing ART's `SIGBUS` | Our last-resort handler is now **chained** rather than replacing (`chain_to_saved_handler`), and with `PENGUDROID_CRASH_BT=1 PENGUDROID_DEBUG_FAULTS=1` it verifies `installed=true` on all five signals — the fault then goes through ART's own saved handler and is still fatal |

**Root cause.** With `UFFD_FEATURE_SIGBUS`, resolving a missing page requires the
kernel to deliver `SIGBUS` *into guest code* and then re-execute the faulting
instruction. Here that guest code is ARM64 running under the prebuilt
translation bridge (`libberis_arm64.so`, no source in this repository), which
cannot resume translated execution after a delivered signal. So the uffd
resolution path cannot work, no matter how the shim behaves.

**Workaround that proves the diagnosis.** Sizing the heap so no compaction
happens during startup avoids the uffd dance entirely, and the image boot then
runs far past the old crash point — `ACRA` initialising for
`org.schabi.newpipe`, `WorkManager`/`SystemJobService` created, `DisplayManager`
returning the 720x1280 PenguDroid display, binder lookups for netstats/user/
appops/SurfaceFlingerAIDL/jobscheduler/connectivity/mount:

```sh
env 'PENGUDROID_PROP_ro.dalvik.vm.enable_uffd_gc=true' \
PENGUDROID_LAUNCH_ACTIVITY=org.schabi.newpipe.MainActivity \
./target/debug/pengudroid art-test --sysroot .toolchain/sysroot-android17 \
  --classpath ~/Downloads/NewPipe_v0.29.1.apk \
  --register-natives libandroid_runtime.so,libhwui.so \
  --bind-app --main android.app.ActivityThread \
  --vm-option -Xms512m --vm-option -Xmx1g --vm-option -verbose:image
```

The first collection that compacts still kills it, so this is a *diagnostic*,
not a shipping configuration: `launcher.rs` keeps the imageless `-Xint` path as
the default.

### Kernel 6.8+ `UFFDIO_MOVE` masking (kept, with an opt-out)

Host kernel 7.0.0 advertises the new `UFFD_FEATURE_MOVE`, which makes ART take
its newest `UFFDIO_MOVE`-based compaction path. The shim's `ioctl` interposer
clears `UFFD_FEATURE_MOVE` and the `_UFFDIO_MOVE` capability bit from the
`UFFDIO_API` reply, so ART uses the older `UFFDIO_COPY` path that this note
describes. `strace` confirms zero `UFFDIO_MOVE` calls. It does not by itself
avoid the fault above; opt out with `PENGUDROID_UFFD_ALLOW_MOVE=1` (see
[API.md](API.md)).

Unchanged and re-verified: `cargo test --offline -p pengudroid-cli` green
(94+7+2+3+2), and BBR2 still launches via `pengudroid launch <apkm> --backend
auto` (100 s smoke run, guest profile installer active, no fatal signal).

### Next concrete task

Make the translator resume after a delivered guest signal, or find an image
whose read-barrier state matches a non-uffd collector. Nothing in the shim can
substitute for either: the classpath order fix is correct and stays, but image
mode is gated on signal-resume support in `libberis_arm64.so`.

Reproduce the fatal in ~40 s with NewPipe (dotted property names need `env`):

```sh
PENGUDROID_LAUNCH_ACTIVITY=org.schabi.newpipe.MainActivity \
env 'PENGUDROID_PROP_ro.dalvik.vm.enable_uffd_gc=true' \
./target/debug/pengudroid art-test --sysroot .toolchain/sysroot-android17 \
  --classpath ~/Downloads/NewPipe_v0.29.1.apk \
  --register-natives libandroid_runtime.so,libhwui.so \
  --bind-app --main android.app.ActivityThread --vm-option -verbose:image
```

Drop the `env '...'` and the image is rejected at the read-barrier check (today's
default); keep it and the image loads, then the SIGBUS arrives. Adding
`-Xms512m -Xmx1g` moves the crash past app startup instead.

## Where we left off — ARM64 BBR2 runs without `--sysroot` (2026-11-12)

Pick up here. The headline change since 2026-11-11 is that an **ARM64-only game
launches from a plain `pengudroid launch`** with no `--sysroot` argument and no
manual runtime step, reaches its own `main`, renders, and presents frames.

Verified this session:

| Claim | Evidence |
|---|---|
| BBR2 (arm64-v8a only) launches with no `--sysroot` | `pengudroid launch <apkm> --backend auto`, 300 s run: `app_exit: null`, daemon alive, clean teardown |
| The guest main loop really runs | SIGQUIT dump: `SDLThread` in `state=R`, 23.7 s user CPU, 6816 switches |
| Frames reach the host compositor | `PENGUDROID_TRACE_GLESHIM=1`: `eglCreateWindowSurface -> 720x1280`, `presented surface 1 720x1280` |
| It is fast once warm, too | swap counter 240 at t=100 s → 2,640 at t=140 s (60 fps), 5,280 presents per 170 s |
| The game's own backend is reachable | HTTP 200 from `free2play.cobalt.vectorunit.com` inside the guest |
| Audio, billing, SQLite, GMS paths run | FMOD `stream`/`mixer`/`Studio` threads, `BillingClient` and `SQLiteCursor` activity |
| Resize policy is inspectable | `pengudroid geometry [--width W] [--height H] [--density DPI] [--resize WxH]` |
| The host can ask a live surface its size | `SurfaceGeometry` (protocol v4); roundtrip tested, and a live BBR2 run read `668x1189` back and relaid out the guest |
| A resize is applied to the guest | `PENGUDROID_FOLLOW_RESIZE=1`: `window is now 668x1189 … 1 root(s) poked`, then `2560x1373 … tablet` after a maximize, 246 presents afterwards |
| A window can be resized without a seat | `pengudroid window [<id>] [--maximize|--unmaximize|--fullscreen]` (`SetWindowState`, protocol v5): a real Mutter `configure` takes a 334x594 dp phone window to 1280x686 dp **tablet** and back, guest buffer unchanged |
| Tests | `cargo test --workspace --offline` green; `python3 -m unittest discover -s tools -p 'test_*.py'` 256 passing |
| New regressions | `geometry` resize/dp policy (`crates/pengudroid-cli/src/geometry.rs`), guest-bundle assembler (`tools/test_assemble_arm64_bridge_guest.py`), `SurfaceGeometry` unknown-surface error |

### A window can be resized without a seat (2026-11-12, later still)

The resize path had one untested link: a compositor `configure`. The daemon is a
Wayland client, so only the compositor can send one — and the smoke harness has
no pointer to drag a window with. `Message::SetWindowState` closes that gap for
testing *and* for users: the daemon calls the same `xdg_toplevel` requests a
title-bar button issues, Mutter answers with a real `configure`, and the
ordinary path runs from there.

```
$ pengudroid window                       # list live windows
surface 1: 720x1280 px internal, 668x1189 px window (334x594 dp)
$ pengudroid window 1 --maximize
surface 1: 720x1280 px internal, 2560x1373 px window (1280x686 dp, tablet)
$ pengudroid window 1 --unmaximize
surface 1: 720x1280 px internal, 668x1189 px window (334x594 dp)
```

| Piece | Where |
|---|---|
| `SetWindowState { surface_id, state }`, `WINDOW_MAXIMIZED`/`WINDOW_FULLSCREEN`, `PROTO_VERSION` 5 | `crates/pengudroid-proto/src/lib.rs` |
| `set_window_state` (toplevel request + flush) | `crates/pengudroid-daemon/src/graphics.rs` |
| Message handler, unknown-bit rejection | `crates/pengudroid-daemon/src/server.rs` |
| `pengudroid window [<id>] [--maximize|--unmaximize|--fullscreen|--windowed]` | `crates/pengudroid-cli/src/main.rs` |

Notes worth keeping:

* **The version had to move.** Adding a message kind changes nothing about the
  wire format of existing ones, but peers are checked for an exact match, so
  `PROTO_VERSION` went to 5. A stale `libpengudroid_shim.so` then shows up as
  `client protocol v4 != daemon v5; rebuild/restart matching peers` — rebuild the
  whole workspace, not just the CLI.
* **The state applies to one window.** After a relayout BBR2 replaces its window,
  so un-maximizing the previous id changes nothing on screen. `pengudroid window`
  with no id lists what is live; `window <newest> --unmaximize` moves it.
* **Fullscreen is the honest "fill the output".** Maximize is bounded by the
  compositor's work area (`2560x1373`), fullscreen is not (`2560x1440`); both keep
  the guest buffer and switch to tablet layouts.

### The audio-port poll loop is answered (2026-11-12, later)

`AudioManager.updateAudioPortCache` polls the audio **policy** service — not the
flinger — about seven times a second for the life of the process. Nothing was
modelled there, so each poll fell through to an empty success reply, the
framework read a count out of nothing, and every poll logged
`AudioSystem: Function: listAudioPorts Line: 1914 Failed`: measured at 1,123
occurrences per 150 s BBR2 run, the largest single source of log noise. Once
ports were answered the same loop moved on to `listAudioPatches` (Line 1985).

| Claim | Evidence |
|---|---|
| `media.audio_policy` code 51 is `listAudioPorts` | `PENGUDROID_TRACE_BINDER=1`: 1,123 dispatches of code 51 on that handle in 150 s |
| The codes come from the shipped framework, never hardcoded | `IAudioPolicyService$Stub` fields read at bind time: `[51, 56]` reported in the run |
| Both polls are answered | 90 s run: `listAudioPorts: 0`, `listAudioPatches: 0`, no `shim symbol` failure (was 1,123 + a trailing loop of patches) |
| Nothing else about audio changed | Same run still reaches the same stage as the pre-fix run (4 surfaces handed out, `ActivityThread` attached, main Looper entered, `surfaceCreated`), and only one unrelated `Function: getAttributesForStreamType` remains |
| Regression test | `cargo test -p pengudroid-shim --offline audio_policy` |

Implementation: `__pengudroid_binder_set_audio_policy_transactions` installs up
to four distinct codes; the dispatch answers only those, with
`status=0, count=0, generation=0`. An empty list is the truthful answer (audio
reaches the host through the forwarder without a framework port), and a zero
count means the framework never reads an entry, so the reply parses the same way
whichever order the revision declares the out parameters in. Every other
audio-policy method keeps the old fallthrough — inventing a reply for
`getOutputForAttr` would hand the framework a handle to a device we do not have.
If a future framework renames the fields, the install fails, the launch prints
`audio policy transaction codes unavailable`, and behaviour degrades to what it
was before this table existed.

### How the two launch blockers were fixed

1. **`JNI DETECTED ERROR: fid == null`** aborted ART before the game's code ran.
   Fixed by reporting the manifest's real field layout rather than a synthetic
   one, with a `GetFieldID`/`GetLongField` tracer left behind under `TRACE`.
2. **An EGL loader failure** followed it. Guest `libandroid.so` and the guest
   GL/EGL family are now **NativeBridgeSupport stubs** that resolve through the
   guest vDSO hook into `libberberis_proxy_<name>.so`, and from there into the
   staged host forwarder (gleshim first, then versioned Mesa). The guest runtime
   that carries those stubs is assembled by
   `tools/assemble-arm64-bridge-guest.py --guest-stubs`. See
   [NATIVE_BRIDGE.md](NATIVE_BRIDGE.md).

### What is still wrong, in priority order

1. **Time to first frame, not frame rate.** Once warm, BBR2 holds a
   vsync-locked 60 fps: the swap counter went 240 → 2,640 between t=100 s and
   t=140 s of a traced run, and 170 s produced 5,280 presents. The whole cost is
   front-loaded — roughly 100 s of loading and shader work first — because the
   guest VM runs `-Xint` and every guest instruction is translated. Cheaper
   warm-up is a performance problem, not a compatibility one.
2. **Resize is verified end to end; only a pointer drag is untested.** The
   compositor preserves the guest's internal resolution and scales the frame
   (correct, verified). `PENGUDROID_FOLLOW_RESIZE=1` closes the loop: it polls
   `SurfaceGeometry` (protocol v4), republishes the geometry with
   `Geometry::for_resize` and pokes each root so `hwui` measures again at the new
   dp size. `SetWindowState` (protocol v5) can now drive the configure without a
   seat, and live runs walked the whole path: `668x1189` (334x594 dp, phone) →
   `--maximize` → a real Mutter `configure` → `2560x1373` (1280x686 dp,
   **tablet**) → `2176x1167` (1088x583 dp, phone again), with the guest buffer
   never reallocated (`720x1280` throughout). What is still untested is a human
   pointer drag; Mutter sent these configures. See [LIMITATIONS.md](LIMITATIONS.md) §4.1–4.2; `pengudroid
   geometry --resize WxH` prints the classification a session would report.
3. **Service gaps that show up as log noise.** `listAudioPorts`/`listAudioPatches`,
   audio code 171/172 (`set/getAllowedCapturePolicy`), input code 1
   (`getVelocityTrackerStrategy`), `IDisplayEventConnection` code 4
   (`getLatestVsyncEventData`), the `shortcut`/`jobscheduler` empty
   `ParceledListSlice` replies, and `graphicsstats.requestBufferForProcess`'s
   ashmem PFD are all answered now (see the top section);
   `berberis: trans#`/`dispatch#` DEBUG lines still dominate the log.
4. **A guest JNI bug that is not ours.** BBR2 issues a `GetMethodID` for
   `Constructor.getReturnType()`, which does not exist (verified by parsing
   `core-oj.jar`). It is logged and does not kill the main loop.

### Next concrete task

The warm-up: ~100 s of loading and shader compilation under `-Xint` is the
remaining cost, and it is a performance problem — [LIMITATIONS.md](LIMITATIONS.md)
§2. Root cause work has since moved to the top of this file: the image itself
now loads, and ART's uffd GC is what still blocks using it. The resize path needs
no more work to be correct; a human pointer drag on a second compositor would
only widen coverage.

## Desktop manager verified (2026-11-11)

Pick up here. The project currently does these things end to end:

| Capability | Evidence |
|---|---|
| BBR2 (2026.10.05) boots and plays | 22,911 presents / 120 s; splash → update → title → race ([screenshots](screenshots/)) |
| NewPipe v0.29.1 renders its live feed | 107 presents / 45 s, network-fetched thumbnails |
| One-click AppImage manager | `target/PenguDroid-x86_64.AppImage` (13 MB, bridge embedded) |
| `+` install → library → launch → stop → uninstall | verified on a real desktop: NewPipe and GTLCS installed through the shell |
| Tests | 390 Rust (`cargo test --workspace --offline`), 242 Python |

Verified this session on the shipped AppImage, not on the dev tree:

* `AppImage <apk>` installs through the same worker the `+` button uses and the
  package appears in **Your library** with **Launch / Stop / Uninstall**.
* `apps.txt` was persisted (the shell's registry caught up with the window).
* The manager's install list, replace-on-reinstall, and uninstall paths now have
  regression tests (`registry::tests::installed_app_can_be_listed_and_removed`).
  `merge_installed` persists the merged list, so a batch install cannot leave the
  file and the window disagreeing.

### Next concrete task

1. **Hardware video decode** (`MediaCodec` with a real `AHardwareBuffer`
   backing) — the last piece NewPipe playback needs.
2. **Icons.** The shell logs `Error loading image …/icons/<pkg>.png` when icon
   extraction fails; fall back silently to the placeholder instead.
3. `AHardwareBuffer`/gralloc, camera, and broader aarch64 translation coverage —
   tracked in [LIMITATIONS.md](LIMITATIONS.md).

### Done since the last handoff (2026-11-11)

* **Frame pacing.** `eglSwapBuffers` now paces the guest to
  `PENGUDROID_MAX_FPS` (default 60, `0` = off): the daemon acks a present as
  soon as it is queued, so games free-ran at ~190 presents/s and burned a core
  on frames the compositor then dropped. Measured on BBR2, same APK, 60 s runs:
  **190/s before → 47/s after** (the cap is 60; the rest is real render time).
  Guest-side on purpose — blocking the daemon on a `wl_callback` would stall
  input for every client.

## Current handoff — Beach Buggy Racing 2 boots into gameplay (2026-11-05)

**Beach Buggy Racing 2 runs.** The splash, the loading path and live gameplay all
render: a 120 s run presents 22,911 dma-bufs and the captured frames walk through
the Vector Unit logo, the profile/update screens, the title screen and then a
race in progress (car, track, HUD, lap counters).

The blocker that had been recorded for weeks — "no GL at all" — **was a
measurement error, not a rendering failure.** The game had been presenting
frames the whole time. See "Why 'no GL at all' was wrong" below.

### The actual last blocker: a hardcoded `versionName`

`install_package_manager_replies` (`crates/pengudroid-cli/src/apphost.rs`)
answered every `PackageInfo` with `versionName="1.0"`, `versionCode=1`. BBR2
asks its update server for a minimum version and compares:

    Important Update
    Please update to the latest version of Beach Buggy Racing 2.
    The minimum version is 2026.10.05. Your current version: 1.0.
                        [ Update! Now ]

Nothing is broken in the graphics stack — the game renders an ordinary dialog
and then, correctly, refuses to continue. The manifest already carried the real
values (`ApkInfo::version_name` / `version_code`); they were parsed and thrown
away. `AppSpec` now carries them through to `PackageInfo`.

Measured before/after on the same APK, from BBR2's own telemetry line:

| build | `HttpConnection: Tenjin::connect params` | outcome |
|-------|------------------------------------------|---------|
| before | `app_version=1.0` | stuck on the update dialog for 110 s |
| after | `app_version=2026.10.05, app_version_code=500` | straight into gameplay |

The update dialog still flashes for ~0.5 s (the server reply arrives after the
first frame), then dismisses itself.

### Why "no GL at all" was wrong

Two independent masking bugs made a working renderer look dead:

1. **Success is silent.** gleshim logs *failures* (`log_once`) and traces only
   when `PENGUDROID_TRACE_GLESHIM` is set. The old harness exported
   `PENGUDROID_GL_TRACE`, which **does not exist in the Rust code** — so no GL
   line ever appeared, no matter how well GL worked.
2. **The daemon's success path is `debug!`.** `handle_present_dmabuf` logs
   `presented dmabuf on surface N (WxH)` at debug level, which requires
   `pengudroid-daemon -v`. `tools/smoke_bundle.py` spawned the daemon with a
   hardcoded path and no arguments, so `-v` could not be passed at all.

`smoke_bundle.py` now honours `PENGUDROID_DAEMON` and `PENGUDROID_DAEMON_ARGS`.
With `PENGUDROID_DAEMON_ARGS=-v` the same 110 s run logged **83,736** successful
presents.

The `pixel format wanted SDL_PIXELFORMAT_UNKNOWN (0), got SDL_PIXELFORMAT_RGB565 (4)`
line is also not an error: `Android_SetFormat()` is called *from*
`SDL_EGL_CreateWindowSurface()` (`src/video/SDL_egl.c`) **after**
`eglCreateWindowSurface()` has already succeeded; it only records what the
window is backing. `format_wanted == 0` means the GL config's surface format was
not recorded.

### Looking at frames

`PENGUDROID_DUMP_FRAME=<path>` (with `_AT`, `_EVERY`, `_NOFLIP`) writes
`<path>.s<surface>frame<n>.ppm` from `glReadPixels` straight off the app's FBO.
`frame 20` is legitimately black (pre-splash); frames from ~300 on show real
content. This is the cheapest way to answer "does the game actually draw?"
without guessing from logs.

## Previous handoff — `pthread_attr_t` ABI mismatch fixed; BBR2 runs `SDL_main` (2026-11-04)

Beach Buggy Racing 2 used to die ~13 s after launch inside berberis'
`ScopedVirtualGuestCallFrame` constructor with `cpu->sp == 0x1001`. That was not
a berberis bug and not a `libfmod.so` bug: it was a **guest/host
`pthread_attr_t` layout mismatch at the libc proxy boundary**.

### Root cause (measured, not guessed)

The guest's `pthread_attr_*` calls are proxied to host glibc, so the 56 bytes a
guest thread passes to `pthread_create` are written by **glibc**, while the
bridge memcpy'd them and read them as **bionic** fields. Every field was
shifted by one slot:

| offset | glibc meaning | bionic meaning (what the bridge read) |
|--------|---------------|---------------------------------------|
| 8 | detachstate bit | `stack_base` |
| 16 | `guardsize` (0x1000) | `stack_size` |
| 24 | `stackaddr_set` | `guard_size` |
| 32 | `stacksize` (0x20000) | `sched_policy` |

So FMOD's real request — a **128 KiB detached stack** — reached the guest as a
4096-byte stack with `stack_base == 0x1`, and the first translated call used
`sp == 0x1001`.

Evidence: a throwaway host program dumped `pthread_attr_t` bytes after
`init` / `setstacksize(0x40000)` / `setdetachstate(DETACHED)` and matched the
guest's observed words **byte for byte** (`0 0 0x1000 0 0 0 0` →
`0 1 0x1000 0 0x20000 0 0`). Two further traps confirmed:

* `pthread_attr_getstack` on glibc returns a garbage base (`-stacksize`,
  `0xfffffffffffe0000`) when no explicit address was set — and the garbage is
  page-aligned, so alignment is not a valid sanity check;
* glibc `stacksize == 0` means "libc default" (8 MiB), not zero, so trusting it
  handed the guest a single 4 KiB page.

### Fix

The translation now happens with the *host's own accessors* in both directions,
and is owned by the build driver (`tools/build-nativebridge.py`,
`apply_source_compatibility_patches`) rather than hand-edited:

* `DoCustomTrampoline_pthread_create` reads the guest attr with
  `pthread_attr_getstack`/`pthread_attr_getguardsize`, rejects implausible bases
  (`< 0x10000`, `> 0x7fffffffffff`, `size < 0x1000`), floors an unset guest stack
  to `kDefaultGuestStackSize` (1 MiB), and requests the host stack with
  `pthread_attr_setstacksize`;
* `DoCustomTrampoline_pthread_getattr_np` reports guest stack bounds with
  `pthread_attr_setstack`/`setstacksize`/`setguardsize` instead of writing bionic
  struct fields directly.

The driver's compatibility-patch list became `(root, relative_path, old, new,
change)` so it can patch both `sources/digitalis` and
`sources/native_bridge_support`; `is_known_compatibility_overlay` accepts the
latter when the host-accessor read is present. Re-running the driver is
idempotent (all 9 patches report `changed_this_run: False`). As
defence-in-depth the `AllocStack` guard in `tools/nativebridge-glibc-tls.patch.json`
and `target/guest-pc-ring/rebuild.py` still rejects implausible guest stacks.

### Measured state after the fix

A 90-second run (`target/bbr2-patch-run.sh shot1 /tmp/px/libmain.so.orig 90`)
ends with `elapsed 90.17`, `app_exit null`, `daemon_alive true`, and **zero**
`SIGSEGV`/`SIGABRT`/fatal-signal lines. Reached, in order:

* `SDL: Running main function SDL_main from library .../libmain.so`;
* `[blast] handed out a Surface` and `[blast] update 720x1280`;
* `[pd-attach] tid=… host stack_size=0x104000` (ART thread attach succeeds);
* `fmod: AudioDevice::init : Actual buffer size: 8192 bytes` + `AudioTrack`;
* the apphost probe finds the window: `views: root 0 mWidth=720 mHeight=1280
  … mAdded=true mAppVisible=true` (11 times);
* 52 guest threads, ~1.1 GB resident, alive for the whole window.

Everything from `SDL: onPause()` to `onDestroy()` in the log happens in the
final ~400 ms and is the harness SIGTERM, not a crash.

### Remaining blocker: no GL at all

> **SUPERSEDED (2026-11-05).** This was a measurement error — the game had been
> presenting frames all along; the real blocker was the hardcoded
> `versionName="1.0"`. See the current handoff at the top of this file. Kept for
> the record.

The engine never starts rendering — the log contains **no** `eglChooseConfig`,
`eglCreateContext`, `eglMakeCurrent` or `vkCreateInstance` line, and the only
`eglSwapBuffers` is the teardown one. Live sampling during a run shows the
main thread parked in `ep_poll` (Looper idle, which is normal) while a single
`SDLThread` burns ~90 % of one core, i.e. an engine/audio thread free-running
instead of presenting frames.

Two suspects, both cheap to test next:

1. **AudioTrack is not back-pressuring.** `AudioSystem.listAudioPorts` fails
   784 times in 90 s (~9/s) and our `AudioTrack.write` accepts far faster than
   real time, which is exactly how an FMOD/SDL mixer thread ends up spinning at
   full speed. Making `write` block on the daemon's PipeWire ring would fix the
   spin and probably unblock the first frame.
2. **The SurfaceView never gets a real consumer.** The BLAST buffer queue is
   created `1x1` and only later updated to `720x1280`, four host surfaces
   (`surface 1..4`) are created for one activity, and the daemon never logs a
   single present. The `apphost` SurfaceView-refresh path still needs the
   non-blocking probe / `ViewRootImpl` descent work.

Also benign and already explained: `NoSuchMethodError:
Constructor.getReturnType()` is our own `reflected_shorty` probe
(`crates/pengudroid-cli/src/sysroot.rs:1606`) logging and clearing itself.

## Previous handoff — Intel BSP compatibility pass (2026-11-02)

The [Intel Edge AOSP BSP](https://github.com/edge-aosp-bsp) was analysed against
this runtime across its Binder/HAL, Mesa, Vulkan, EGL, gralloc and memory
components. The full report, reuse matrix and roadmap are in
[INTEL_BSP_COMPATIBILITY.md](INTEL_BSP_COMPATIBILITY.md).

Short version: the BSP is a bare-metal Android 16 product, while PenguDroid is a
Linux-hosted, unprivileged Android 17/API 37 runtime with no general HAL service
manager. Its gralloc/HWC/media/boot implementations are not portable; the
reusable part is the contract layer. Measured API-37 facts used in the report:

* the sysroot still ships HIDL gralloc mapper 2.0/3.0/4.0 **and** the AIDL
  `allocator-V2-ndk` / `graphics.common-V7` libraries, so HIDL is deprecated but
  not removed;
* memtrack and power clients are AIDL (`memtrack-V1-ndk`, `power-V7-ndk`), while
  the BSP's MemTrack is a legacy `hw_module` and Intel's power AIDL is frozen at
  v5;
* API-37 `libui` probes Gralloc5, then Gralloc4, and only falls back to
  Gralloc3/2 when `android_get_device_api_level() <= 35`;
* ANGLE (`libEGL_angle.so`) is present, so GLES routing must not assume Mesa.

**Implemented and tested from that analysis:** `pengudroid-shim` now derives a
gralloc-style plane layout for `AHardwareBuffer` (RGBA/RGBX/RGB565/F16 plus
NV12/NV21/YV12), so allocation size, `lockPlanes`, `lockYCbCr` and
`bytesPerPixel` agree on plane offsets and row/pixel strides; the old code
always reported one plane at `stride * 4`. `AHardwareBuffer_isSupported` now
advertises only the formats with real geometry.

Validation: `cargo test -p pengudroid-shim --offline --lib` **148 passed**;
`cargo test -p pengudroid-cli --offline` **94 passed** (80 + 7 + 2 + 3 + 2).

### BBR2 native loading (same branch)

The merged TLS/proxy work plus this branch's changes now get past "library not
found": `libfmod.so` and `libfmodstudio.so` load through
`NativeBridgeLoadLibraryExt`, and `libSDL3.so` enters the guest linker and runs
its constructors. The remaining gap is speed, not discovery: in
`BERBERIS_MODE=interpret-only` the SDL3 load did not return within 45 s, so no
game frame has been observed. `docs/STATUS.md`'s older "libc proxy still blocks"
heading below is historical; the proxy and TLS blockers are fixed.

## Current handoff — TLS fault fixed; libc proxy still blocks BBR2 (2026-11-01)

**Android 17 / API 37 / x86_64 only on the host.** Focus is Beach Buggy Racing 2
(`com.vectorunit.cobalt.googleplay`, SDL + FMOD, **arm64-v8a only**). Its Java
side boots on ART and the only blocker is the ARM64 natives, so this pass
rebuilt the ARM64 build inputs that had been deleted from `.toolchain`.

### What now exists again (no root used)

| Input | State |
| --- | --- |
| ARM64 GSI `aosp_arm64-exp-CP2A.260605.012-15430684-8e545e1e.zip` | downloaded, SHA-256 `8e545e1e…48f75` verified |
| LLVM 22.1.2 (the pinned compiler) | unpacked from distro `.deb`s into `.toolchain/llvm-22-root`, wrappers in `.toolchain/llvm-bin` |
| NDK r30 platform sysroot | extracted to `.toolchain/ndk-r30/sysroot`, linked as `.toolchain/ndk-refs_tags_ndk-r30-platform-sysroot` |
| `rustup` target `aarch64-linux-android` | installed (was missing; E0463 `can't find crate for core`) |
| AOSP source closure (bionic + 13 external trees) | **new** `tools/fetch-guest-sources.py`, pins read from the driver |

`tools/fetch-guest-sources.py` is the tool that was missing: `nativebridge-deps.py`
fetches header *slices* by design, so the full trees the guest build compiles had
been assembled by hand and were unreproducible. The new tool fetches
`<project>/+archive/<revision>.tar.gz` at the driver's own pins, hashes every
archive, bounds extraction, and materialises the exact destinations the driver
hard-codes. Two layout facts it encodes (both measured): `EXT_PINS` is keyed by
AOSP **path** but archives are **project** snapshots, so each input carries a
subpath; and the driver indexes crates as `<dest>/<crate>/Android.bp`, so
`android-crates-io` must land as its `crates/` subtree.

### Measured build state

The guest linker/libc bundle and translator are built and wired into the real
BBR2 launch path. The smoke run reaches `JNI_CreateJavaVM`,
`LoadNativeBridge=true`, Berberis initialization, and ARM64 guest execution.
This proves that the staged-only guest gate, linker bundle, bridge loading, and
basic ARM64 execution path are all crossed.

The first optimizer crash was caused by uninitialized `postloop_edge` and
`loop_edge` locals in the pinned Digitalis `read_flags_optimizer.cc`. The
nativebridge build driver now applies and records a narrow compatibility overlay
for those locals and keeps the gear-switch threshold at `UINT32_MAX` while the
heavy optimizer remains unvalidated. Rebuilding succeeds, but the first BBR2 blocker was a repeatable low-address
fault at `si_addr=0xfa9` in guest bootstrap code. The faulting instruction was
`ldrb w8, [x8, #0xfa9]` in the guest linker's `bionic_trace_begin`; `x8` came
from Bionic `__get_tls()`. The original translator did not bind a synthetic
Bionic TLS object to the guest thread, so the linker read an invalid TLS base.

The bridge TLS adapter now allocates and binds the required guest pthread,
Bionic TLS object, and guest TCB. With the actual TLS-adapted artifact
(`run-74nov5bs`) and `PENGUDROID_BRIDGE_TLS=1`, the `0xfa9` fault is gone.
This was independently reproduced in interpreter-only mode with
`BERBERIS_FLAGS=glibc-host-thread-id-handoff`.

The next blocker is native libc proxy loading. `libberberis_proxy_libc.so` now
exports Bionic's missing `__pthread_cleanup_push`/`__pthread_cleanup_pop`
compatibility ABI from `crates/pengudroid-shim`, but the translator's proxy
loader still needs to be rebuilt with the compatibility overlay changing
`dlopen(library_name, RTLD_GLOBAL)` to
`dlopen(library_name, RTLD_NOW | RTLD_GLOBAL)`. The latest smoke log reached
`ProxyLibraryBuilder::Build()` and then failed with glibc's
`dlopen failed: libc.so: invalid mode for dlopen(): Invalid argument`.

The required next build must be a fresh translator build so the proxy-loader
overlay and TLS adapter are present in the same artifact. Rebuild the proxy
against that report, stage `libberberis_arm64.so` and
`libberberis_proxy_libc.so` together, then rerun the short `/tmp` BBR2 smoke
with `BERBERIS_MODE=interpret-only`, `PENGUDROID_BRIDGE_TLS=1`, and
`BERBERIS_FLAGS=glibc-host-thread-id-handoff`. BBR2 has not yet loaded
`libSDL3.so`/`libfmod.so` or reached the game Activity.

`build-guest-linker.py` resolves the whole Blueprint closure and compiles **85
translation units** (Rust `icu4x` crates and `arm-optimized-routines` assembly
included). Two driver defects fixed to get there:

* clang injects the NDK libc++ *after* module includes when `--sysroot=<NDK>`, so
  libc++'s `<string.h>` wrapper lost to `bionic/libc/include` and `<cstring>`
  failed. libc++ is now declared explicitly and first, as AOSP does.
* `libdl`, `libutils`, `libsystem` and `libpropertyinfoparser` are now
  materialised from the pinned archives instead of hand-copied.

The six pinned clang archives are now fetched reproducibly by
`tools/fetch-guest-sources.py`, with manifest hashes and a report in
`sources/GUEST_SOURCES.md`. The guest linker/libc build now completes with
`PENGUDROID_LLVM_DIR=/usr/lib/llvm-22/bin`; the report and
`.toolchain/nativebridge-guest-linker-build/out/ld-android.so` are present.
The build driver generates compatibility headers for the pinned libc++/Bionic
atomic boundary and the API-37 `std::nothrow_t` initialization mismatch.

The guest bundle includes `system/bin/arm64/{linker64,app_process64}`,
`system/lib64/arm64/{libc.so,libdl.so,libm.so,libnative_bridge_vdso.so}`, and
`system/etc/ld.config.arm64.txt`. BBR2 is not open yet: the current run reaches
native libc proxy construction but stops before `libSDL3.so` or `libfmod.so` is
loaded.

### Render-path contracts fixed last pass (unchanged, still verified)

`INTERFACE_TRANSACTION` now answers with the descriptor of every hosted
interface (measured `code=1598968902` refusals 1 → 0), and the API-37 C++
`android.ui.ISurfaceComposer` sharing the SurfaceFlinger handle accepts
`BOOT_FINISHED`/`SET_TRANSACTION_STATE` (tag 8, from the pinned
`ISurfaceComposer.h`) with an empty body (measured `code=8` refusals 9 → 0).

Validation: `cargo test --workspace --offline` **373 passed / 0 failed**;
`tools` unittest **214 passed**; `git diff --check` clean.

## Previous handoff — BBR2 target: ARM64 chain must be rebuilt (2026-10-31)

**Android 17 / API 37 / x86_64 only on the host.** The focus is now Beach Buggy
Racing 2 (`com.vectorunit.cobalt.googleplay`, SDL + FMOD, **arm64-v8a only**).

### BBR2: re-measured on this checkout

A bounded 30 s run (`target/bbr2-1031-remeasure/`) reproduces the documented
state exactly: the XAPK materialises (35 splits incl. `split_config.arm64_v8a`),
ART boots the Java side, `handleLaunchActivity` returns the Activity, and the
only hard blocker is the ARM64 natives:

```
nativeloader: Load libSDL3.so ... cannot open shared object file
nativeloader: Load libfmod.so ... cannot open shared object file
ERROR MainActivity: Failed to load FMOD libs
apphost: no x86_64 native libraries in /tmp/pengudroid-libs/...
```

**No ARM64 execution work is wired right now.** The translator
(`libberberis_arm64.so`), the guest linker/libc artifacts, the NDK sysroot and
the guest source expansion had all been removed from `.toolchain` since the last
checkpoint. This pass rebuilt the *inputs* and measured what is still missing:

| Input | State now |
| --- | --- |
| ARM64 GSI `aosp_arm64-exp-CP2A.260605.012-15430684-8e545e1e.zip` | downloaded, SHA-256 verified `8e545e1e…48f75` (`.toolchain/downloads/aosp_arm64-gsi.zip`) |
| LLVM 22 (pinned driver) | installed **without root** from Ubuntu `.deb`s into `.toolchain/llvm-22-root`, wrapped by `.toolchain/llvm-bin` |
| NDK r30 platform sysroot | extracted to `.toolchain/ndk-r30/sysroot`, linked as `.toolchain/ndk-refs_tags_ndk-r30-platform-sysroot` (`usr/include/jni.h`, `usr/lib/aarch64-linux-android/37`) |
| `tools/nativebridge-deps.py` inputs | fetched (28.7 MB, manifest written; preflight still `NOT RUN`) |
| `sources/bionic-expansion/bionic` + `sources/ext/*` | **still missing — see below** |

Both build drivers now honour `PENGUDROID_LLVM_DIR` (default
`/usr/lib/llvm-22/bin`), so a relocated compiler tree can be used without
patching the pins.

**The chain is not reproducible from this repository yet.**
`tools/build-guest-linker.py` compiles from
`sources/bionic-expansion/bionic` (full pinned Bionic tree) and ~15
`sources/ext/*` trees (`arm-optimized-routines`, `google-highway`, `llvm-libc`,
`android-crates-io`, `librustutils`, `libcutils`, `libpropertyinfoparser`,
`soong`, `clang-prebuilt`, `bionic-libdl`, `scudo`, `gwp_asan`).
`tools/nativebridge-deps.py` deliberately fetches only header **slices**
(its own manifest says so), so those trees were created by hand in the previous
session and are gone. The next concrete task is a pinned fetcher for that
closure (googlesource `+archive/<revision>.tar.gz` per project, same
budget/lock/manifest style as `nativebridge-deps.py`), then:

```sh
PENGUDROID_LLVM_DIR=$PWD/.toolchain/llvm-bin python3 -B tools/build-nativebridge.py --towards-bridge
python3 -B tools/prepare-arm64-guest.py --archive .toolchain/downloads/aosp_arm64-gsi.zip   # staging only
PENGUDROID_LLVM_DIR=$PWD/.toolchain/llvm-bin python3 -B tools/build-guest-linker.py --module app_process --bundle
```

and finally BBR2 with `PENGUDROID_NATIVE_BRIDGE_LIB` +
`PENGUDROID_ARM64_GUEST_ROOT=<bundle>`. `bundle/` must be manifest-free: the
host gate in `sysroot.rs` rejects staged trees by contract.

### Two render-path contracts fixed (measured, not guessed)

BBR2's Java UI reaches `ThreadedRenderer.create`, and both failures below were
in that path. Both are **API-37 contracts**, not older-release fallbacks:

1. **`INTERFACE_TRANSACTION` is answered everywhere.** A caller that probes a
   hosted interface must receive its descriptor; the broker rejected it
   (`UNKNOWN_TRANSACTION target=26 (SurfaceFlingerAIDL) code=1598968902`).
   `hosted_interface_descriptor()` now names the interfaces this broker really
   implements (SurfaceComposer, SurfaceComposerClient, DisplayEventConnection,
   the hosted activity host) and the descriptor is the whole reply, with no
   status word — the same shape `Binder.onTransact` uses.
2. **The C++ `android.ui.ISurfaceComposer` shares the SurfaceFlinger handle.**
   Nine `code=8` calls were refused with "token required". The interface token
   was measured from the parcel (`first16=Some("android.ui.ISurfaceComposer")`,
   UTF-16 stored in API-37 `libgui.so`), and code 8 is
   `ISurfaceComposerTag::SET_TRANSACTION_STATE` in the pinned API-37
   `libs/gui/include/gui/ISurfaceComposer.h`. `BOOT_FINISHED` (1) and
   `SET_TRANSACTION_STATE` (8) are now accepted with an empty body (the
   `status_t` travels in the reply header), counted and traced; every other tag
   is still refused loudly. The host owns no layers, so this is "applied, nothing
   to compose", not a claim that a frame was composed.

Measured effect on BBR2 (`target/bbr2-1031-cppsf/`): `code=8` refusals **9 → 0**
and `code=1598968902` refusals **1 → 0**.

Validation on this checkout: `cargo test --workspace --offline` **373 passed, 0
failed**; `tools` unittest **214 passed**; `git diff --check` clean.

Still open and unchanged: the two `libbinder.Parcel: Expecting binder but got
null!` warnings (a service reply that must carry a binder object), the
`Settings$SettingNotFoundException: limit_ad_tracking`, and BBR2's FMOD native
lib load. Closed since this note: the `HardwareRenderer` graphics-stats buffer
(`graphicsstats.requestBufferForProcess` now returns a real `0x248`-byte ashmem
PFD), the `ParceledListSlice` NPEs on `shortcut`/`jobscheduler` (empty-slice
replies), `IAudioService` code 171/172, and the `listAudioPorts` loop — see the
top section.

## Previous handoff — activity roundtrip and video crop verified; ARM64 guest runtime still blocked (2026-10-31)

**Android 17 / API 37 / x86_64 only on the host.** This checkpoint verifies the
hosted Activity lifecycle on a real NewPipe run (Main → Settings → finish →
resume), routes input to the focused visible window only, and fixes the decoded
video's padding by propagating the per-frame BufferQueue crop all the way to the
compositor. The ARM64 translator still cannot execute guest code: the guest
runtime. The ARM64 translator and the guest linker/libc now build, but guest
execution has not been wired or run. It
does **not** claim Beach Buggy Racing 2 runs.

### Beach Buggy Racing 2: measured debug chain (2026-10-31)

`tools/smoke_bundle.py <bundle>` (new, reusable) runs one bundle against a
private daemon. Measured on `Beach+Buggy+Racing+2_2026.10.05_APKPure.xapk`
(`com.vectorunit.cobalt.googleplay`, `com.vectorunit.cobalt.MainActivity`):

1. The XAPK materialises (5 members, 4 splits + assets) and the **Java side
   boots on ART**: `SDLActivity.onCreate` (this build is **SDL + FMOD**, not
   Unity), `handleLaunchActivity` returns the Activity, WindowInsets 720x1280,
   the WorkManager DB is created.
2. First hard blocker: `nativeloader: Load libfmod.so ... cannot open shared
   object file` → `MainActivity: Failed to load FMOD libs`. The bundle is
   arm64-v8a-only (`apphost: no x86_64 native libraries ...`), so the FMOD/SDL
   natives need the translator.
3. With `PENGUDROID_NATIVE_BRIDGE_LIB=<de-bionic'd libberberis_arm64.so>` the
   chain reaches the fail-closed gate in 4 s, **before** `JNI_CreateJavaVM`:
   `art: ARM64 guest ... is staged-only ... stock GSI linker64/libc are not
   native-bridge variants`. No fake linker exports, no patched game.
4. Java-side gaps to fix alongside the bridge work: the
   `IAudioPolicyService.listAudioPorts` failure loop (audio port cache spam),
   `IAudioService` code 171 unimplemented, the graphics-stats `requestBuffer`
   NPE, and the `ParceledListSlice` NPE.

Guest bootstrap artifact built and verified:
`target/arm64-guest-bootstrap/app_process64` — the real tiny
`native_bridge_post_init()` executable (AArch64 PIE, interpreter
`/system/bin/arm64/linker64`, `DT_NEEDED linux-vdso.so.1`).

### NewPipe: click → detail → decoded video frames

Two implementation pieces landed:

* **Hosted `startActivity`.** The broker resolves
  `IActivityTaskManager.startActivity`, `IActivityManager.startActivity` and
  `IActivityManager.finishActivity` from the loaded API-37 Stubs, parses the
  API-37 transaction forward (header, interface token, caller binder, calling
  package, optional feature id, then `Intent`), validates the explicit
  component and the hosted package, and queues the raw transaction. The main
  thread now runs API-37 `Looper.loopOnce(Looper,long,int)` between queued
  transitions instead of blocking in `Looper.loop()`; it replays
  `handleLaunchActivity` / `handleResumeActivity` / `handlePauseActivity` /
  `handleStopActivity` / `handleDestroyActivity` with a distinct local `Binder`
  token per activity. Unknown or unhostable starts return a transport
  `UNKNOWN_TRANSACTION`; no fake success.
* **A real NewPipe navigation run, with the round trip.**
  `target/newpipe-activity-roundtrip/`: `PENGUDROID_PROBE_START_ACTIVITY` opens
  `SettingsActivity` from the resumed `MainActivity`, the synthetic tap at
  `(48,56)` hits its toolbar Up button and the broker records
  `finish token=2` for the **correct** activity; `handleResumeActivity` brings
  the previous one back (`activity finished; the previous one is resumed`),
  with zero `JNI DETECTED ERROR` lines. Input is delivered to the focused
  visible root only (`IsSameObject`, stopped roots excluded), so a tap no
  longer clicks the Activity underneath.
* **Per-frame video crop.** The decoder intentionally allocates `ALIGN128(width)`
  (512×240 for a 426×240 stream); the visible rectangle arrives as the
  `BufferItem.mCrop` (offset 32 in the pinned API-37 layout, frameworks/native
  `ae266dcb706d`). `blast.rs` now reads it on **every** acquired frame and sends
  it in `PresentShm`; the protocol bumped **v2 → v3** with an explicit
  `BufferCrop` (exclusive right/bottom), and the daemon converts only that
  rectangle while still addressing YUV planes by allocation stride/height.
  Measured: `first software frame submitted (512x240 format=842094169
  crop=BufferCrop { left: 0, top: 0, right: 426, bottom: 240 })`, 690+ decoded
  frames, and the compositor dumps are `present-4-426x240.ppm` (previously
  `512x240` with a green margin) plus `present-7-1280x720.ppm` after the quality
  switch. Right-edge pixel sampling shows real content, not padding.

Reproduction (bounded, no daemon reuse; `PENGUDROID_DUMP_PRESENT` needs an
**existing** directory):

```sh
mkdir -p target/newpipe-activity-roundtrip/frames-crop
# dismiss NewPipe's first-run "Keep Android Open" dialog (OK ≈ 600,579 on the
# 720x1280 window), then open the first feed item 12 s later:
PENGUDROID_DUMP_PRESENT=$PWD/target/newpipe-activity-roundtrip/frames-crop \
PENGUDROID_SYNTH_CLICK='600,579@8000;150,300@12000' SMOKE_SECONDS=45 \
  python3 -B target/newpipe-activity-roundtrip/smoke.py
```

Synthetic-click delays are **cumulative** (each `@ms` waits before that tap),
and `PENGUDROID_CLICK_TEXT`/probes run after all taps.

Remaining NewPipe gaps: the `HardwareRenderer` graphics-stats buffer is still
refused (`requestBuffer` NPE, non-fatal WARN), `ParceledListSlice` NPEs remain,
audio reaches the PipeWire sink (`pw-cat` is alive during playback) but the
audio content itself is unverified, and leak-free long playback is untested (a
45 s sample ends at ~429 MB RSS, stable over its final seconds — not a leak
measurement).

### Digitalis ARM64 translator: built, loads, guest-runtime blocker measured

`tools/build-nativebridge.py --towards-bridge --seconds 1800` now reports
`Translator shared built: YES`:

* `libberberis_arm64.so`, 37,719,928 bytes, SHA-256
  `11fec1897d6ec4cb5faa19eb14753db980e7d4ea43d5dcf2383d36581e976f35`,
  global dynamic `NativeBridgeItf` v8.
* `tools/prepare-arm64-guest.py` verifies and extracts the matching
  `aosp_arm64` GSI (`CP2A.260605.012`, SHA-256
  `8e545e1e1e977c3b43b4e54a3af463f3e7d1c3177b120e772138334e15f48f75`), stages
  832 guest files under `system/bin/arm64` and `system/lib64/arm64`, and builds
  `libnative_bridge_vdso.so`.
* Real run: `LoadNativeBridge = true`, `PreInitializeNativeBridge = true`,
  `Initialized Berberis (aarch64)`, then
  `FATAL ... couldn't find "__native_bridge_config_static_tls" symbol in linker`.
  The stock GSI `linker64` is not the native-bridge product's modified linker.
  The loader is now **fail-closed** about this state: tool staging (schema 1 and
  schema 2 `staged-only`) is rejected before `JNI_CreateJavaVM`, and a
  manifest-free bundle must pass the real ELF contract
  (`__native_bridge_config_static_tls`, `__native_bridge_get_host_pthread`, the
  VDSO hooks). The selected translator is preloaded by exact path and proven
  identical to its basename before `LoadNativeBridge`, and
  `NativeBridgeGetSignalHandler`'s callback is chained over ART's handler
  instead of being misread as a `struct sigaction`.
* **Guest runtime artifacts now build.** `tools/build-guest-linker.py` produces,
  from the pinned sources plus the fetched external closure:
  * `native_bridge_guest_linker.native_bridge` (6,906,368 B, SHA-256
    `2fd29a0c…f6304d`): ELF64 AArch64 DYN, SONAME `ld-android.so`, no
    `DT_NEEDED`, all 12 required `.dynsym` exports (`__loader_*` from
    `dlfcn.cpp`, `__native_bridge_config_static_tls`,
    `__native_bridge_get_host_pthread`), `.symtab` retained with `__dl_`
    prefixes, `ANDROID_RELR`.
  * `libnative_bridge_guest_libc.native_bridge` (3,605,768 B, SHA-256
    `9b403bce…77bf05`): SONAME `libc.so`, version nodes `LIBC`…`LIBC_37`,
    `DT_NEEDED` = `linux-vdso.so.1`, `ld-android.so`, `libdl.so`; exports the
    native-bridge replacements and **imports `native_bridge_intercept_symbol`**
    (`stubs_arm64.cpp`), satisfying the runtime gate.
  * `bundle/` assembles `system/bin/arm64/{linker64,app_process64}`,
    `system/lib64/arm64/{libc.so,libdl.so,libnative_bridge_vdso.so}` and
    `system/etc/ld.config.arm64.txt` (needed for the guest `default` namespace).
* **Next blocker is wiring, not building:** Digitalis resolves guest runtime
  paths as **absolute host paths** (`GetSystemPath`: `/system/bin/arm64/...`,
  preferring `/system/bin/berberis/...` when that directory exists;
  `kernel_api/open_emulation.cc` special-cases `/system/bin/arm64/linker64`).
  PenguDroid has no host-side `/system` redirection yet, so the next step is to
  interpose `open`/`openat`/`stat`/`access` (and the exec path) in the shim and
  rewrite `/system/...` to a configured root (e.g. the built `bundle/` or the
  sysroot), then rerun BBR2 and capture the next real failure. After that:
  guest `libm.so`/`libc++.so` and real ARM64 `app_process64`/framework
  libraries as the failures demand; nothing is runtime-tested yet.

See [NATIVE_BRIDGE.md](NATIVE_BRIDGE.md) for the build flags, source pins, and
the exact driver changes.

### Validation on this checkout

* `cargo test --workspace --offline -j 2` with
  `PENGUDROID_TEST_ANDROID17_SYSROOT=$PWD/.toolchain/sysroot-android17`:
  **365 tests, 0 failures** (all seven serialized real API-37 CheckJNI gates
  ran, including the activity-request roundtrip gate).
* `python3 -B -m unittest discover -s tools -p 'test_*.py'`: **167 tests, 0
  failures** (44 cover ARM64 guest preparation).
* `cargo build --workspace --offline -j 2`: passed. `git diff --check`: clean.
* The ARM64 guest extraction and translator build are local `.toolchain`
  artifacts; neither is committed or redistributed.

## Previous handoff — main-thread startup and Java process identity fixed (2026-10-29)

**Android 17 / API 37 / x86_64 only.** This checkpoint supersedes the older
Roblox network-only diagnosis below. TLS/client-settings fetching now succeeds;
two measured application-hosting defects were fixed. **Roblox still does not
reach its menu, and NewPipe playback is not stable.** No capability, GPU-vendor
coverage, leak-free playback, or ARM translation is inferred from passing tests.

### The actual splash failure, not a guessed Activity transition

The previous run ended after `fastflag_load_success` / `native_lib_wait_fetch_end`.
Two bounded SIGQUIT captures (`target/roblox-settings-diag/`) and a real
`-Xcheck:jni -Xjnitrace:NativeGLInterface` run (`target/roblox-jni-diag/`) separated
JSON processing from JNI: `readLocalFlags` resolves its class and methods and
`NewObjectV` returns a non-null `ClientLocalFlags`. No sustained wait inside that
native method was demonstrated.

An opt-in JVMTI exception capture then exposed what the app's ordinary log hid
(`target/roblox-exceptions-diag/app.log:1147`):

```
SharedPreferencesImpl$EditorImpl.notifyListeners:
  NullPointerException: Handler.post(...) on a null object reference
  caught by th.f.h, in the settings-completion path
```

`prepare_main_thread` recreated `Looper.prepareMainLooper`, `new ActivityThread`
and `attach`, but omitted **`ActivityThread.sMainThreadHandler = thread.getHandler()`**,
which the real API-37 `ActivityThread.main` performs immediately after attach.
`apphost.rs` now publishes that exact Handler, before application binding, and
preserves an already-published one as the framework does. Missing contracts or a
failed JNI write fail startup. `jni.rs` adds standard `SetStaticObjectField`.

The new `main_thread_probe.rs` uses real framework objects, an attached background
thread, `SharedPreferencesImpl$EditorImpl.notifyListeners`, and
`Looper.loopOnce(Looper,long,int)`. It checks Handler/Looper identity and executes
a posted `FutureTask` marker on the main thread. Empty but non-null standard
collections deliberately exercise the notification's posting path; this is not a
test of application listener callbacks. Globals, local frames, worker attachment,
timeout callbacks and temporary preferences storage have explicit cleanup.

**Red/green evidence:** `android17_main_dispatch_under_checkjni` failed before the
fix with `sMainThreadHandler is null` (~2.9 s), then passed after it. Retained logs:
`target/main-handler-regression-before.log`, `target/main-handler-regression-after.log`.
Roblox then advanced to `fetch_flag_begin/end`, native engine/crashpad setup, and
cookie initialization (`target/roblox-main-handler/`). No `startActivity` attempt
was observed; the absent activity-transition host remains a possible later
blocker, not the demonstrated cause of this splash stop.

### Java application identity must not equal Android's SYSTEM_UID

The next exception was `WebViewFactory.getProvider`: **"WebView is not allowed in
privileged processes"** (`target/roblox-handler-exceptions/app.log`). This machine's
ordinary Linux user is UID 1000, which Android interprets as `SYSTEM_UID`, while
the hosted `ApplicationInfo.uid` was already 10000.

`process_identity.rs` now installs the app UID at the **Java/libcore JNI boundary**
before app initialization. The pinned API-37 `core-libart.jar` has instance Java
`Linux.getuid/geteuid/getgid/getegid` wrappers backed by private static
**`@CriticalNative nativeGet*()I`** methods. The four registered callbacks therefore
have the exact zero-argument ABI, not `(JNIEnv*, jobject)`. Wrapper/native modifiers,
method signatures, API level and application-UID range are checked before
registration. This artifact's SHA-256 is
`981354781d1bd379cebc6036f8ebe404f852da36d9484a2b1672a8f5e857bf3b`.

The mapping is immutable within an app process, uses the same `spec.uid` as
`ApplicationInfo` (currently 10000), and supports user-0 normal app UIDs only.
Real/effective host root is rejected, including a root check in every callback.
**Host libc/native identity queries and credentials remain untouched.** This is
Java compatibility metadata, not a security sandbox, privilege drop, root
concealment, hardware identity, or attestation. Native libraries still observe
host identities. See [DEVICE_SHIM.md](DEVICE_SHIM.md).

`android17_process_identity_under_checkjni` verifies all four Java `Linux` and
`Os` queries plus `Process.myUid`, repeats them, and independently verifies all
four host libc IDs are unchanged. Seven synthetic policy tests cover the range,
immutability, rejected root credentials and transparent uninstalled state.
Failed `bind_application` results now propagate through `bind_application_now`:
an unbound guest exits unsuccessfully instead of silently entering the Looper.
The negative no-classpath launch returned status 1 and never entered it
(`target/bootstrap-bind-failure.log`).

### Current Roblox blocker: a real WebView provider is not hosted

After both fixes, a bounded 55 s API-37/CheckJNI exception capture
(`target/roblox-process-identity/`) stays alive and captures splash frames, but
cookie setup throws a **bare** `UnsupportedOperationException`. The privileged
UID message is gone. The actual API-37 DEX (`framework.jar!/classes4.dex`,
`WebViewFactory.getProvider`, code item `0x3c9a24`) checks `isWebViewSupported()`
at code-unit PC 45 and throws the no-argument exception at PCs 147–152 when
`hasSystemFeature("android.software.webview")` is false. PC 163 in the captured
stack is the lock-cleanup **rethrow**, not the original check.

PenguDroid does not advertise WebView, correctly: there is no implemented
`webviewupdate` service or provider package/loading contract. **Do not flip the
feature flag just to move the error.** A genuine provider candidate is already
in the API-37 tree:

- `system/product/app/webview/webview.apk`, package `com.android.webview`, version
  `145.0.7632.218` / `763221807`, 333,998,538 bytes.
- Real ELF64 x86_64 `libwebviewchromium.so`, 185,797,040 bytes; the expected
  `WebViewChromiumFactoryProviderForT.create(WebViewDelegate)` exists.
- APK signing blocks were observed, **not cryptographically verified** here.

Next, in order: validate the actual provider package/signers and install
package-name-keyed metadata (the current one-caller cached PackageInfo and
placeholder signatures cannot stand in for it); host the loaded Stub's
`IWebViewUpdateService.waitForAndGetProvider` with a framework-marshalled
`WebViewProviderResponse`; handle provider package dependency/context/resource
loading; perform actual x86_64 native/RELRO/factory initialization. Only then
publish WebView support and gate real `CookieManager` construction. Keep version,
signature and privileged-process checks intact. Factory/cookie initialization
would still not establish page rendering or renderer-service support.

### NewPipe regression attempt: rendered UI, then a measured media crash

The bounded `target/newpipe-main-handler/` launch was **not a passing smoke**.
It rendered a 720x1280 video-detail/comments frame with a loading spinner, bound
`PlayerService`, and reached real AAC/AVC codec activity. The known surface-pool
failure remains (`get generationNumber failed`, `fetchGraphicBlock ... status 22`).
It then exited with status 1 after ~22 s: `MediaCodec_loop` entered
`JMediaCodec::handleCallback` → JNI `CallVoidMethodV` → ART interpreter stack-overflow
handling, followed by `Check failed: top_shadow_frame_ != nullptr` / SIGSEGV.
Its causal relationship to these bootstrap changes has **not** been isolated;
do not call this run stable playback or a clean regression pass. The native
callback/thread stack contract needs a deterministic CheckJNI reproducer in
addition to the two pending surface-block-pool measurements recorded below.

Native SIGQUIT visibility has a separate identified limit: this API-37
`libunwindstack` requests signal **33**, which glibc/NPTL reserves and rejects
with EINVAL. Do not bypass glibc with raw `rt_sigaction(33)` or remap only the
installation while leaving `tgkill(33)` unchanged. No signal workaround was
shipped in this pass.

### Validation of this checkpoint

- `PENGUDROID_TEST_ANDROID17_SYSROOT=/home/pc/PenguDroid/.toolchain/sysroot-android17
  cargo test --workspace --offline -j 2`: **336 passed, 0 failed**, including all
  **six** serialized real API-37 CheckJNI gates (core, BLAST, media discovery,
  AudioTrack, main dispatch, process identity). Runtime gates were not skipped.
- `python3 -B -m unittest discover -s tools -p 'test_*.py'`: **123 passed**.
- Workspace build passes with existing warnings; `git diff --check` is clean.
- Logs: `target/bootstrap-identity-workspace-tests.log`,
  `target/bootstrap-identity-tool-tests.log`, `target/process-identity-gate.log`.
- ART/application probes ran serially; app/daemon process groups were reaped.
  Short samples are **not** evidence of leak-free sustained operation.

BBR2 remains ARM64-only with no working translator; Intel/AMD/NVIDIA hardware
playback coverage is unchanged. No old runtime fallback, container, VM, mounted
image, TLS weakening, game-binary patch, or repository visibility change was used.

## Prior handoff — API-37 AudioTrack adapter hosted and verified (2026-10-28)

Android 17 / API 37 / x86_64 remains the only runtime. The previous handoff's
audio task — a descriptor-checked AudioSystem/AudioTrack JNI adapter with a
CheckJNI gate — is implemented, integrated and now **verified on this
checkout**. Playback (video or audible audio) is still **not** established.

### What the adapter is

`crates/pengudroid-shim/src/audiotrack/` hosts AudioTrack and AudioSystem at
the JNI boundary; it never invokes a real `AudioTrack` C++ native with a hosted
integer handle. The pieces:

- `contract.rs` reflects the loaded classes' own `getDeclaredMethods`, derives
  each descriptor, and validates a **closed** typed table before any pointer is
  registered, so a stale `native_setup` signature or a new/renamed native fails
  loudly. `runtime_contract.rs` is the independent DEX oracle (framework.jar
SHA-256 `55f235f3…`); 57 AudioTrack + 2 AudioSystem entries, 4 of them static.
- `api37.rs` registers exactly those pointers; `engine.rs` owns the bounded
  streaming queue into the existing `pw-cat`/`aplay` sink; `jni.rs` is only the
  standard JNI C table (no private C++ layouts).
- Registration is a **closed boundary**: `sysroot.rs` calls
  `__pengudroid_register_audio_api37` only *after* the original
  `libandroid_runtime` groups run. Audio is never name-patched through the
  exported `AndroidRuntime::registerNativeMethods` interposer, which keeps the
  original `internalRegisterMethodsOrDie` path authoritative for audio.

### Verified evidence (2026-10-28)

- `android17_audio_track_under_checkjni` ran (not skipped) and **passed** via
  `PENGUDROID_TEST_ANDROID17_SYSROOT=.toolchain/sysroot-android17`. A direct
  probe (`-Xint -Xcheck:jni`) reproduces in **~3 s**:
  `audio: hosted API37 AudioSystem=2 AudioTrack=57 output_rate=48000` and
  `audio: API-37 AudioTrack initialized session=3 min=3528 rate=48000 wrote=44100/44100 head 9844 -> 11025`.
- `cargo test --workspace --offline`: **326 passed, 0 failed**. All four API-37
  CheckJNI gates (`core`, `blast`, `media` discovery, `audio`) ran and passed.
- `python3 -B -m unittest discover -s tools -p 'test_*.py'`: **123 passed**, `OK`.
- `git diff --check`: clean.
- `cargo build --workspace --offline`: clean (warnings only).

A previously retained log (`target/workspace-tests-audio-device.log`) showed all
four gates failing with `ART probe timed out`; that was a transient
loaded/contended run. A serialized rerun completes each probe in seconds. The
new logs are `target/workspace-tests-audio-verified.log` and
`target/audio-api37-gate-rerun.log`.

### Sink-stall test made deterministic (2026-10-28)

The engine test
`blocked_writer_is_released_and_sink_pipe_cannot_trap_the_finalizer` was racy,
not just red once: its 256 KiB payload is smaller than the sink chain's total
absorption, so whether `occupied() > 0` held after 50 ms depended on whether the
pump thread lost the scheduling race (it failed 3/3 solo, and could pass under a
loaded parallel suite). Measured on this host (`/bin/cat` is uutils coreutils,
64 KiB pipes, stdout never read) the chain absorbs **835,584 bytes** before
stalling. The engine was never at fault — it already writes through a
non-blocking pipe with a 3 ms retry and `release()` never waits on I/O — so the
fix is the payload only: 8 MiB, several times any plausible pipe+buffer
absorption, making the stall premise deterministic. 3/3 solo reruns pass and
the suite number above is now stable.

### A real NewPipe launch on this machine (2026-10-28)

A bounded 75 s launch (`pengudroid launch NewPipe_v0.29.1.apk --backend art
--component org.schabi.newpipe.MainActivity`, private daemon socket, host
Wayland/PipeWire session preserved) confirmed the adapter is live in a real app
and that the app draws on the host GPU:

- `audio: API37 RegisterNatives android/media/AudioTrack descriptors=57 rc=0`
  and `audio: hosted API37 AudioSystem=2 AudioTrack=57` appear in the app's own
  log, i.e. the adapter installs on the real registration path.
- The guest attached, installed the window session, built a `SurfaceControl`,
scheduled traversals, bound `PlayerService`, loaded real YouTube content over
  HTTP/2 and ran its SQLite database. No fatal signal; the run ended only at the
  timeout.
- `PENGUDROID_DUMP_FRAME` captured **four 720×1280 frames** written by
  `gleshim`, so hwui really draws on the host GPU through the dma-buf path. One
  frame is the rendered NewPipe *Live* feed; an earlier one is the first-run
  dialog.

**Not established by that run:** no MediaCodec/Codec2 activity occurred and no
`AudioTrack` was constructed (`audio: API37 setup` never printed), because no
stream was played. This is UI rendering evidence, not video or audio playback.

### Host hardware video, measured here (2026-10-28)

On this machine (single **Intel CometLake-U UHD**, iHD VA-API 1.22):

- `pengudroid video-decode-test` decoded a 128×128 H.264 fixture and exported a
  synchronized NV12 dma-buf (`objects=1, planes=2`, luma readback 64 = fixture).
- `pengudroid-hostgpu --test hardware_video` ran **2,000** decode/flush/export
  cycles: fd count 5 → 5, RSS 31 424 → 31 424 KiB (bounded).
- `pengudroid-daemon --lib hardware_video_import` passed the two-plane
  compositor import against the live Wayland session.

So the **host** half of hardware video is working here. "Every GPU" is not
demonstrable here either: only one Intel iGPU is present, AMD is unvalidated,
and no NVIDIA/NVDEC backend exists.

### Native Codec2 service unblocked: 60 codecs, `video/avc` present (2026-10-28)

The in-process codec2 service (`crates/pengudroid-shim/src/codec2.rs`) had never
come up on API 37, so `MediaCodecList` saw only the APEX AAC codec and **no
video decoder could be created**. Two defects were fixed:

- **`Parcel::setData` does not exist on API 37.** `libhidlbase.so` exports
  `setDataSize`/`setDataCapacity`/`setDataPosition` but no `setData`, so
  `build()` returned at the first `dlsym` and the store was never constructed
  (`[codec2] dlsym …setDataEPKhm failed`). The request parcel is now filled
  with the exported `Parcel::write(const void*, size_t)` (with `setData` kept as
  an optional alternative for runtimes that export it), then rewound with
  `setDataPosition(0)`.
- **The parcel backing store was misaligned.** A `[u8; PARCEL_BYTES]` on the
  stack is byte-aligned, so writing the objects array at `0x48`/`0x50` faulted
  with `misaligned pointer dereference` once the service actually ran. It is now
  `#[repr(C, align(16))]`.

Verified with `PENGUDROID_PROBE_MEDIA_LIST=1` (`target/api37-codec2-service-up.log`):

```
[codec2] service up: platform=0x… store=0x… bn=0x…
Codec2Client: Available Codec2 services: "default" "__ApexCodecs__"
[codec2] built traits reply for 36 codecs
media: API-37 codec enumeration repeated 16 times: regular=60, all=60
```

60 codecs instead of one, and `video/avc` is in the list (`C2SoftAvcDec`). All
four API-37 CheckJNI gates and **326** Rust tests still pass.

### The gralloc mapper is now provided (2026-10-28)
The earlier stopping point (kept here because it is what the fix addresses):
with the service up, a bounded NewPipe run reaches the real decoder and aborts:

```
CCodecBufferChannel: [c2.android.avc.decoder#231] Created input block pool … OK (0)
… Configured output block pool ids 21 => OK
Codec2Client: setOutputSurface -- generation=… consumer usage=0xb00
FATAL GraphicBufferMapper: gralloc-mapper is missing
```

The stack is `CCodec::start` → `CCodecBufferChannel::start` →
`Codec2Client::Component::setOutputSurface` →
`C2BufferQueueBlockPool::Impl::configureProducer` →
`BufferQueueProducer::dequeueBuffer` → `GraphicBuffer::GraphicBuffer`
(`target/newpipe-api37-avc-surface.log`).

The mechanism is **measured, not guessed** (`system/apex/com.android.media.swcodec/lib64/libui.so`,
BuildID `4fdd6c3b…`): `GraphicBuffer::GraphicBuffer()` has the entire
mapper-selection chain **inlined** into it (`0x2fc60`–`0x2fd0f`). It constructs
`Gralloc5Mapper`, checks `isLoaded()` (a virtual call through `*0x10`), then
`Gralloc4Mapper` (PLT), then — only when
`android_get_device_api_level() <= 35` — `Gralloc3Mapper`/`Gralloc2Mapper`, and
finishes with `__android_log_assert("gralloc-mapper is missing")` when every one
is unloaded. All four are unloaded because the gralloc HAL is absent here
(`Failed to get isDeclared for android.hardware.graphics.allocator.IAllocator/default`).

Because the chain is inlined into libui's own constructor, the shim's
interposed `GraphicBufferMapper`/`GraphicBufferAllocator` constructors were
never on this path — symbol interposition only covers cross-library PLT calls.
`Gralloc5Mapper` is built by a *direct* call and cannot be hooked, but
`Gralloc4Mapper` **is** a PLT call, so `ahardwarebuffer.rs` now interposes
`_ZN7android14Gralloc4MapperC1Ev`/`C2Ev` and installs a mapper table whose
`isLoaded()` is true and whose buffer operations reuse the shim's existing
memfd-backed `GraphicBufferMapper` implementations. The three slot offsets this
depends on were read from the **real call sites** (`isLoaded` at `*0x10`,
`importBuffer` at `*0x20`, `freeBuffer` at `*0x28`), not copied from a static
image: a naive read of `_ZTVN7android14Gralloc4MapperE` comes back mostly zero
because RELRO relocations are not materialised on disk.

Verified on device (`target/newpipe-api37-gralloc-mapper.log`):

```
[ahb] Gralloc4Mapper constructed: hosted mapper installed
[ahb] allocated 640x480 fmt=35 stride=640 (1228800 bytes) fd=181
[ahb] allocated 384x240 fmt=842094169 stride=384 (368640 bytes) fd=221
```

The `gralloc-mapper is missing` abort is **gone** (zero occurrences), the
bounded run stays alive for its whole window, and `c2.android.aac.decoder`
**and** `c2.android.avc.decoder` are created and passed `setOutputSurface`.
All four API-37 CheckJNI gates and **326** Rust tests still pass.

### New stopping point: the surface-backed output block pool

The decoder now fails one layer deeper, inside codec2's block pool for a
`Surface` output (`target/newpipe-api37-gralloc-mapper.log`):

```
CCodecBufferChannel: [c2.android.avc.decoder#561] Query output surface allocator returned 0 params => BAD_INDEX (6)
CCodecBufferChannel: [c2.android.avc.decoder#561] Created output block pool with allocatorID 18 => poolID 21 - OK
SimpleC2Component: Using output block pool with poolID 21 => got 21 - 0
ERROR C2SoftAvcDec: fetchGraphicBlock for Output failed with status 22
CCodecBufferChannel: [c2.android.avc.decoder#561] work failed to complete: 14
```

So the remaining work is that surface/block-pool contract: `fetchGraphicBlock`
must hand out a graphic block the software decoder accepts. Note the decoder the
app chose is the software `C2SoftAvcDec`; wiring in the host VA-API decoder is a
separate later step.

The `BAD_INDEX (6)` allocator *query* is **not** the bug: `CCodecBufferChannel`
tolerates `C2_BAD_INDEX` there and only logs because `params.size() != 1`; the
store legitimately has no `C2PortSurfaceAllocatorTuning`. The real contract is
below.

### Surface block pool: measured root cause (2026-10-28)

**Update: the decoder crash is fixed; ByteBuffer output now decodes.** The
fault was not a lifetime bug at all. API-37 `libcodec2_vndk.so` imports two
symbols this shim was not defining (`llvm-nm -D`, verified):

```
_ZN7android19GraphicBufferMapper6unlockEPK13native_handlePNS_4base14unique_fd_implINS4_13DefaultCloserEEE
_ZN7android19GraphicBufferMapper4lockEPK13native_handlejRKNS_4RectEPPv
```

The shim only provided the single-argument `unlock(handle)` and a seven-argument
`lock(...)` overload, so both guest calls resolved to `libui`'s own
implementations. `libui`'s `unlock` is `this->mMapper`, then `*(mMapper)`,
then `callq *0x50(%rax)` (disassembled at `libui+0x2a41a`), and `mMapper` is
unusable without a gralloc HAL — that is the measured SIGSEGV. Three symbols are
now hosted with their exact API-37 signatures: the two above plus
`importBufferNoValidate(native_handle const*, native_handle const**)`. The
hosted vtable slots were aligned to the same ABI (`importBuffer` 2-arg,
`lock` 4-arg, `unlock` with a fence out-parameter).

Verified with `PENGUDROID_PROBE_MEDIA=1` (ByteBuffer mode, real H.264 fixture):

```
[ahb] import 0x7f9754136690 -> 0x7f9754637110 fds=1
[ahb] lockYCbCr ... [ahb] unlock 0x7f9754637110
media: MediaCodec.start = true
media: queueInputBuffer(24732 bytes) = true
media: dequeueOutputBuffer[1] = Some(0) size=24576 flags=0
media: output format width=128 height=128
media: dequeueOutputBuffer[2] = Some(1) size=0 flags=4   # EOS
```

24576 bytes is exactly one 128x128 YV12 frame, the fatal signal count is zero,
and `GraphicBufferMapper::unlock` no longer appears in any backtrace. So the
software AVC decoder and the shim's CPU mapper work end to end.

**Still open: surface output.** With `PENGUDROID_PROBE_SURFACE=1` the pool is
created and configured, but the HIDL producer handshake still fails:

```
[ahb] allocated 640x480 fmt=35 stride=640 (1228800 bytes)
[ahb] free 0x7f20ac039020 owned=true
WARN C2BqBuffer: get generationNumber failed <producerId>
ERROR C2SoftAvcDec: fetchGraphicBlock for Output failed with status 22
```

The buffer is allocated by the real `BufferQueueProducer::dequeueBuffer` and
freed immediately, so `requestBuffer` returned nothing;
`getGenerationNumberAndUsage()` (`C2BqBuffer.cpp:264`) therefore reports no
buffer queue information and the component cannot take a graphic block. That
HIDL `IGraphicBufferProducer` callback path is the next thing to host.

Measured further on this checkout:

- `PENGUDROID_TRACE_BINDER=1` shows **no** binder transaction for the producer,
  so `dequeueBuffer`/`requestBuffer` are in-process calls on real `libgui`.
- Adding a trace to `GraphicBufferMapper::importBuffer` proves the conversion
  never reaches it (`[ahb] importBuffer` never prints), so `requestBuffer` fails
  before `hgbp_h2b` produces a `GraphicBuffer`. The slot buffer is then detached,
  which is the `[ahb] free … owned=true` line.
- The block pool falls back to its own allocator (`mProducerId == 0`): it logs
  `[ahb] allocated 384x240 fmt=842094169 stride=384`, and the next observable
  event is `fetchGraphicBlock for Output failed with status 22`
  (`C2_BAD_VALUE`), i.e. `C2AllocationGralloc` rejects the block this shim's
  allocator produced.

Next two measurements, in order: (1) why `BufferQueueProducer::requestBuffer`
returns a non-OK status or an unconvertible `HBuffer` for a slot this shim's
allocator filled, and (2) which check inside `C2AllocationGralloc` returns
`C2_BAD_VALUE` for a memfd-backed handle. Both are local calls; no HAL can be
blamed and no binder work is involved.

The earlier stopping point is kept below for context.

A deterministic reproducer now exists that needs no Wayland window and no app UI:
`PENGUDROID_PROBE_MEDIA=1 PENGUDROID_PROBE_SURFACE=1` inside an `art-test` app
bind configures `c2.android.avc.decoder` with an `ImageReader` surface, starts
it, queues a real H.264 fixture and reads frames
(`target/newpipe-api37-surf/app.log`). It reproduces the exact NewPipe failure.

Two distinct defects are now separated:

1. **Producer generation/usage query fails.** `C2BqBuffer: get generationNumber
   failed <producerId>` comes from `getGenerationNumberAndUsage()`
   (`frameworks/av/media/codec2/vndk/platform/C2BqBuffer.cpp:264`), which does
   `dequeueBuffer(640x480, YCBCR_420_888)` and then `requestBuffer(slot)` on the
   HIDL `IGraphicBufferProducer`. The dequeue succeeds — the shim's allocator
   logs `[ahb] allocated 640x480 fmt=35 stride=640` — but `requestBuffer` does not
   return a buffer, so `bqInformation` stays false and the pool is configured
   with generation `0`.
2. **ByteBuffer output reaches real decoding, then the mapper's unmap crashes.**
   With the surface probe disabled, the same fixture is genuinely decoded: the
   stack reaches `C2SoftAvcDec::process` and builds/destroys a `C2GraphicView`.
   Destruction faults in `GraphicBufferMapper::unlock`:

```
Fatal signal 11 (SIGSEGV) SEGV_MAPERR fault addr 0x7f89c8e743ec
#00 libui.so  android::GraphicBufferMapper::unlock+61
#01 libcodec2_vndk.so  android::C2AllocationGralloc::unmap+107
#02 libcodec2_vndk.so  _C2MappingBlock2DImpl::Mapped::~Mapped
#06 libcodec2_soft_avcdec.so  android::C2SoftAvcDec::process+1679
```

The faulting address is the `native_handle_t*` itself, so `unlock` is
dereferencing a handle whose storage is no longer mapped. The shim's own
`lock`/`unlock` interposition is therefore not on this path, or the handle it
gets back has already been released by the extra `importBuffer` copy this shim
returns (a `Box<NativeHandle>` the caller owns).

**A bounded retire-on-free pool was tried and rejected:** keeping released
handle records alive did not stop the fault, so the faulting pointer is not one
of the records this shim allocated.

Measured identity of the faulting handle (`PENGUDROID_TRACE_AHB=1`, ByteBuffer
probe): `importBuffer` received `0x7f8618136690` and returned the shim's copy
`0x7f8618637110`; `lockYCbCr` then reached the shim successfully; the faulting
`rdi` was `0x7f81e065c3ec` — neither address. There is **no** `[ahb] unlock`
line, so the fault happens inside `libui`'s own `unlock` on a handle this shim
never imported and never allocated a map for. The remaining work is therefore
the `importBuffer` ownership contract: on a device the mapper hands back the
handle the buffer keeps, and `freeBuffer` releases that same handle exactly
once. This shim currently returns an unrelated `Box<NativeHandle>` copy, so
`GraphicBuffer` and `C2AllocationGralloc` end up observing different handles.
The next change must make those agree (single owner, single release) and be
validated with the ByteBuffer probe before touching the surface path again.

`GraphicBufferMapper::unlock`/`lock` are **not exported** by this API-37
`libui.so` (`llvm-nm -D` lists `importBuffer`, `freeBuffer`, `getTransportSize`,
`getPlaneLayouts`, … but no `lock`/`unlock`/`mMapper`), so they are inlined into
`libcodec2_vndk` and cannot be interposed; the fix has to be correct handle
lifetime plus a mapper the inlined path accepts, not another symbol hook.

### Roblox (x86_64): launches, renders the splash, blocked by the network (2026-10-28)

This is the first real third-party app with large native code to run on this
checkout, and it is an x86_64 path — **no ARM translation involved**.

`pengudroid launch …apkm --backend art --component
com.roblox.client.startup.ActivitySplash` (bounded 90 s, private daemon socket,
`target/roblox-api37-run/`):

- APKM extracted to `base.apk` + `split_config.x86_64.apk`; the split is passed
  as `ApplicationInfo.splitSourceDirs`, and the APK's 88 MB of assets were
  unpacked.
- `RobloxApplication.onCreate` ran, `handleBindApplication` returned normally,
  and `ActivitySplash.onCreate` / `InitHelper.setView` executed.
- **`libroblox.so` loaded** — 122,979,704 bytes of real x86_64 code — plus
  `libzstd-jni-1.5.7-6.so`, `libbacktrace-native.so` and the framework
  connectivity JNI libraries. No `UnsatisfiedLinkError`, no fatal signal, zero
  `SIGSEGV` in the whole log.
- **55 frames were captured and all 55 are distinct**, so the splash is drawn
  continuously on the host GPU. The image is the real Roblox splash: the black
  ROBLOX wordmark on `#f5f5f5` with an animated "Loading ..." line.
- `pengudroid ps`-visible ordinary process; the guest stayed alive for the whole
  90 s window and only the harness ended it.

**Why it stops at the splash — an external network block, not a PenguDroid
bug.** The app waits for `clientsettingscdn.roblox.com` and TLS fails:

```
javax.net.ssl.SSLPeerUnverifiedException: Hostname clientsettingscdn.roblox.com not verified:
  DN: CN=*.btk.gov.tr, O=Bilgi Teknolojileri ve İletişim Kurumu, ST=Ankara, C=TR
  issuer: Sectigo Public Server Authentication CA OV R36
ClientSettingsRequest: can't parse flag json.
```

The host itself is intercepted identically, so this is not the guest's CA store
(the sysroot has 145 CA certificates and TLS works for other hosts):

```
curl https://www.youtube.com/            -> 200
curl https://api.github.com/            -> 200
curl https://clientsettingscdn.roblox.com/ -> 000 (hostname mismatch)
openssl s_client … clientsettingscdn.roblox.com -> CN=*.btk.gov.tr
```

So Roblox reaches its own splash and then cannot fetch client settings from this
network. Reaching the main menu requires a network where the Roblox domains are
not intercepted; nothing in PenguDroid can honestly change that, and adding the
intercepting CA to the guest trust store would deliberately weaken TLS for every
app.

### Beach Buggy Racing 2: measured blocker (unchanged by this pass)

`pengudroid doctor 'Beach+Buggy+Racing+2_2026.10.05_APKPure.xapk'` reports 18
native libraries, `ABIs: arm64-v8a`, `host ABI (x86_64): no`, `loadable (host):
0/18`, verdict *not runnable natively: ARM-only code (needs the ARM translator)*.
No `libberberis_arm64.so` exists in this checkout and the native-bridge build
stops at a real dependency closure (`libandroid` → `libandroid_common` →
`android.hardware.power-ndk_shared`); the upstream route is a full AOSP
`m berberis_all` with `BUILD_BERBERIS_ARM64_TO_X86_64`, whose build system is not
present. **No guest instruction has ever been translated**, so BBR2 cannot be
opened from this checkout. Staging its ARM64 libraries alone would not change
that: the shim does not yet route ARM paths to a translator either.

### Device compatibility profile integrated

`crates/pengudroid-device` (Pixel 11 metadata, see [DEVICE_SHIM.md](DEVICE_SHIM.md))
is now wired into the daemon property store: `props.rs::defaults()` populates it
*after* the base/image defaults, and both `Props::set` and persisted ingestion
(`load_into`) plus `PENGUDROID_PROP_*` env overrides run through
`validate_platform_property_override`, which now also rejects
`is_protected_profile_key` writes (including nested
`ro.appcompat_override.` wrappers). Runtime API/CPU identity is unaffected.

### Still open in the audio path (measured this run)

The gate proves the JNI contract, the session allocator, the minimum-buffer
helper and PCM consumption by the queue — **not** audible output:

1. Audio focus in this path is incomplete: the audio `AudioSystem` path emits
   `UNKNOWN_TRANSACTION target=8 (audio) code=2/4/5: API-37 audio-focus Stub
   transactions not installed`. Core focus (the separate AIDL audio-focus gate)
   passes; these AudioSystem codes are not yet installed and are non-fatal here.
2. The probe (and the `art_framework` harness) points `XDG_RUNTIME_DIR` at a
   temporary directory, so the sink logs `pw_context_connect() failed: Host is
   down`. That is the isolated harness, not a host without audio: this desktop
   runs PipeWire + WirePlumber with a live `pipewire-0` socket. The gate proves
   PCM consumption by the queue (`9844 -> 11025`), not audible output.
3. NewPipe audio initialization is still unverified; the earlier ExoPlayer
   `AudioSink$InitializationException` is not claimed fixed by this pass.
4. Previously recorded blockers are unchanged: video Codec2/gralloc
   integration, the boot-image read-barrier mismatch, real hardware media
   integration, ARM64 translation, and resource lifetime auditing.

## Prior handoff — API-37 media discovery no longer aborts (2026-10-27)

Android 17 / API 37 / x86_64 remains the only runtime. The next real NewPipe
playback blocker was a fatal HIDL manager cast, not graphics-stats telemetry.
It is now fixed and covered by a new headless CheckJNI gate. Playback itself
is **not** complete.

### Corrected discovery, without inventing codecs

`crates/pengudroid-shim/src/binder.rs` now implements the pinned HIDL manager
contracts rather than treating them as AIDL:

- `get(manager-version, "default")` returns one canonical manager handle, and
  that handle routes to the same dispatcher as the context object. Previously
  it became an unrelated handle whose `interfaceChain` returned only zeros.
- `interfaceChain`, `interfaceDescriptor` and `ping` use the actual reserved
  HIDL codes (`0x0f43484e`, `0x0f445343`, `0x0f504e47`). The chain is
  `manager@1.2`, `@1.1`, `@1.0`, `IBase`; these are HIDL interface versions
  present on Android 17, not older Android runtime selections.
- Strings/vectors use real LP64 pointer-buffer records, including parents,
  offsets, NULs and the nullable empty-array record. The finite immutable
  manager payloads are cached once, not leaked on every query. Tests decode
  the pointees and repeat each query 1,024 times with identical backing pointers.
- Missing HALs return successful typed absence: `Transport::EMPTY`, null HIDL
  Binder, or an empty vector. They do not receive arbitrary handles or fake
  `[default]` declarations. Unsupported methods/tokens still fail explicitly.
  `uint8_t Transport` is four-byte padded in Parcel; HIDL binders have no AIDL
  stability word. Code 5 (`listByInterface`) and 13 (`listManifestByInterface`)
  are distinct methods, not alternate platform layouts.

Pinned sources and reproduction are in [ANDROID17.md](ANDROID17.md). The native
Codec2 backend is not made available by this discovery fix.

### Attach order and real-runtime validation

`ServiceManager` Stub codes are now installed **before** `ActivityThread.attach`.
The CLI stops if main-thread preparation fails instead of continuing with a
half-attached thread. The core CheckJNI gate now exercises this ordering.

- Workspace build and **312 Rust tests passed**, zero failures.
- **All three** API-37 CheckJNI gates ran and passed: core services/attach,
  BLAST lifecycle, and `android17_media_discovery_without_codec2_under_checkjni`.
  The latter enumerates the real codec list 16 times with the hosted HIDL
  Codec2 backend disabled and asserts successful empty HIDL discovery, no
  incompatible-manager error and no discovery retry loop. Independently
  available APEX codecs are not suppressed or fabricated.
- **111 Python tool tests passed**; `git diff --check` passed.

Logs: `target/hidl-discovery-{workspace-build,workspace-tests,checkjni,shim-tests,tools-tests}.log`.
The standalone default-backend probe is
`target/api37-media-discovery-native-backend.log`.

### NewPipe reached AAC decoding; audio initialization is next

A new bounded run, `target/newpipe-api37-hidl-discovery/`, injected a measured
feed tap at `360,300` after 8 s. The guest stayed alive for **35.18 s** until
cleanup, with five sampled UI frames. Logs contain **zero** fatal messages,
incompatible-service errors or Codec2 discovery retries. `ActivityThread.attach`
succeeds and the earlier `rawGetService` NPE is absent.

The real framework discovers one APEX AAC codec through `__ApexCodecs__` (the
old HIDL service loader stops at missing `Parcel::setData` symbol). AAC work
reports **14 positive output events / 57,344 output bytes**, then ExoPlayer
reports `AudioSink$InitializationException` / `Invalid audio session ID -1`.
This is decoder progress, **not successful audio playback or video decoding**.
No AVC/HW decode, pause/seek or leak-free soak is claimed.

**Next concrete work:** a descriptor-checked API-37 AudioSystem/AudioTrack JNI
adapter. The exported registration hook is bypassed by this runtime's internal
registration path; there are no `audio: hosted` lines. Do **not** simply revive
its name-only table: the existing `native_setup` function has an incompatible
signature (separate mask integers instead of `ChannelMasks`, missing attribution
Parcel and provenance string), and its output-rate helper interprets stream
number 3 as 3 Hz. It also misses seven current natives, which could send an
integer hosted handle to C++ as an AudioTrack pointer. The pinned-source/runtime
contract audit is retained in `target/audio-contract-*-20261027.log`; use a
closed, correctly typed registration boundary and a new CheckJNI audio gate.

Other blockers remain: API-37 video Codec2/gralloc integration, the boot-image
read-barrier mismatch, real hardware media integration, ARM64 translation, and
resource lifetime auditing. In particular the Binder transfer area's 16-slot
wraparound does not track `BC_FREE_BUFFER`; caching these HIDL pointees does
not prove transfer-buffer ownership or whole-app leak freedom.

## Prior handoff — API-37 runtime restored, CheckJNI gates pass (2026-10-27)

The pinned API-37 runtime is now present and exercised in this checkout. This
pass restored it, made the two real API-37 CheckJNI gates pass, and fixed a
frame-path NPE that aborted every fling traversal. It does **not** establish
video playback, hardware decode, leak freedom, or a working BBR2 run.

### Runtime restored from the pinned GSI

`tools/prepare-android17.py` verified the official archive before extracting a
single byte:

* `aosp_x86_64-exp-CP2A.260605.012-15430684-3da2ca55.zip`, 1 022 217 967 bytes,
  SHA-256 `3da2ca5599d304c4d92fe8c72f31d035a59db57c93ce073740ed4015ee034a8e`.
* ext4 `system.img` expanded without mounts, 38 APEX payloads unpacked, 61
  absolute symlinks relativised, output `.toolchain/sysroot-android17`.
* The GSI keeps `/apex` as an **empty mount-point directory**. `sysroot-relink`
  previously left it in place, so `/apex/...` links resolved outside the
  extracted tree and `system/lib64/libc.so` looked missing. It now replaces only
  that empty directory with `system/apex`, and refuses to touch a populated one.
* Runtime identity enforced end to end: SDK 37 / release 17 / build
  `CP2A.260605.012`, real ELF64 x86_64 `libart.so`, and per-file SHA-256 in
  `pengudroid-runtime.json`. `prepare` also pins `system/lib64/libandroid.so` and
  `system/lib64/libdl_android.so`, which the native-bridge build now imports.

### Real API-37 CheckJNI gates: 2 passed, 0 skipped

`PENGUDROID_TEST_ANDROID17_SYSROOT=… cargo test -p pengudroid-cli --test art_framework`
now runs both gates instead of skipping:

* `android17_core_services_under_checkjni` — window, input, settings, audio
  focus, media session/metrics, user and thermal Stub codes, native
  `KeyCharacterMap`, WindowLayout and the API-37 resized parcel.
* `android17_blast_lifecycle_under_checkjni` — real framework
  create/destroy/recreate with native display discovery.

Two runtime gaps had to be closed first, both found by the gates:

* **`memset_explicit` was an undefined symbol.** Android's `libbinder.so`
  imports this Bionic/C23 entry point; this glibc has no `memset_explicit`.
  The shim now exports one whose stores cannot be dead-code-eliminated
  (`write_volatile` + return of `dst`), with unit tests for fill, partial-fill,
  zero length and the `c as u8` truncation rule.
* **`DisplayInfo.frameRateVelocityMapping` was null.** Android 17 added this
  `List` field and `Display.getFrameRateVelocityMapping()` calls
  `List.isEmpty()` on it unconditionally, so every `View.updateDisplayListIfDirty`
  during a fling threw and aborted the traversal. The observed
  `invoke-interface List.isEmpty()` fixes the declared type as `java.util.List`;
  an empty `ArrayList` is the truthful value for this synthetic display
  (`hasArrSupport = false`, one fixed 60 Hz mode). A missing contract now fails
  loudly instead of being guessed.

### NewPipe UI verified on the restored runtime

Bounded 35 s Wayland run with a private daemon socket
(`target/newpipe-api37-final/`, logs and `result.json` retained):

* The guest survived the **whole** window (`app_exit_before_cleanup: null`)
  where the pre-fix run died at 25.2 s with only 2 frames. 3 frames were dumped
  across 2 surfaces.
* The captured 720x1280 frame is the real NewPipe UI: red header, tab strip and
  feed spinner. This is UI rendering evidence, **not** video decode or a
  frame-rate claim.
* Both test broker and guest process groups were reaped (`cleanup_complete`).

### Measured remaining runtime gaps

1. **Boot image does not load; ART runs imageless.** Exact ART error: `ValidateOatFile
   found read barrier state mismatch (oat file: false, runtime: true)`. The
   GSI's `framework/x86_64/boot.art` (image v119) and `boot.oat` are present and
   reachable; the mismatch is not configurable — all 154 `-X`/`-XX` options
   exported by this `libart.so` were enumerated and **none** mentions read
   barriers. `-Ximage:` semantics were also measured: it is a location whose
   directory gains `<arch>/<basename>`, so `framework/boot.art` resolves to the
   real `framework/x86_64/boot.art`, while passing the arch dir directly makes
   ART look for `framework/x86_64/x86_64/boot.art`. Imageless running works but
   costs startup time and removes boot-image sharing.
2. **Three non-fatal NPEs remain** (the guest keeps running through all of them):
   `ServiceManager.rawGetService` from `ActivityManager$4.create`,
   `JobSchedulerImpl.getAllPendingJobs` on a null `ParceledListSlice`, and
   `HardwareRenderer$ProcessInitializer.requestBuffer` on a null
   `ParcelFileDescriptor` in `initGraphicsStats`.
3. **No real translator `.so`.** See [NATIVE_BRIDGE.md](NATIVE_BRIDGE.md).
4. Playback, MediaCodec/Codec2, gralloc allocation, hardware decode and the
   memory-lifetime audit are unchanged and still open.

### Validation on this checkout

* `cargo build --workspace --offline`: passed.
* `cargo test --workspace --offline`: **307 tests, 0 failures**, including the
  `memset_explicit` ABI tests, the C++ GraphicBuffer fixture and the bridge ABI
  tests.
* `python3 -B -m unittest discover -s tools -p 'test_*.py'`: **111 tests, 0
  failures** — runtime preparation, sparse-image expansion, APEX mount-point
  relinking, and the native-bridge dependency/build drivers.
* Negative check unchanged: `.toolchain/sysroot` (API 33) is rejected before ART
  loads; it is not a fixture.
* Native-bridge dependency preflight: `PASS (compile/link only)`.

Logs: `target/android17-restored-checkjni.log`, `target/workspace-tests-final.log`,
`target/tools-tests-latest.log`, `target/newpipe-api37-final/`.

## Prior handoff — API-37-only selection enforced, native bridge modularised (2026-10-27)

**Only Android 17 / API 37 x86_64 is a runtime target.** This pass removed
remaining selectable old runtime contracts and isolated ARM64 translation behind
one crate. It did not establish stable NewPipe playback, leak freedom, hardware
decoding on every GPU, or a working BBR2 run.

### Native bridge module

All ARM64-to-x86_64 bridge knowledge now lives in `crates/pengudroid-nativebridge`:
the `NativeBridgeItf` layout (21 members, version 8), `NativeBridgeRuntimeCallbacks`,
the `arm64` / `/lib/arm64` parameters, library loading, trampoline lookup and the
canonical staging layout. `sysroot.rs` only orchestrates the self-test and
`launcher.rs` only forwards the opt-in environment. See [NATIVE_BRIDGE.md](NATIVE_BRIDGE.md).

Verified without an Android runtime: the Rust table matches the real AOSP
`native_bridge.h` **offset by offset and in total size**; a stub translator is
driven through `initialize` → `loadLibrary` → `getTrampoline` → call and returns
42; wrong argument order, an x86_64 path, a descriptor shorty, interface version
99 and a non-bridge library are all rejected. An API-37 AArch64 guest fixture
(`tools/build-arm64-bridge-fixture.sh`) cross-builds and links with Bionic
`@LIBC` versions; it is not executed on the host.

Correction recorded from the source: the ARM64 path rule is a `strstr` test, so
`lib/arm64-v8a` **is** accepted. The module stages into `lib/arm64` as the
canonical layout but does not reject the APK's own directory.

**Not verified:** no real bridge binary exists here, so no guest instruction has
been translated. Building one needs an AOSP checkout and `m berberis_all` with
`BUILD_BERBERIS_ARM64_TO_X86_64`; the sources are supplied, the Android build
system is not.

### Implemented in the previous pass

- Shared SDK/release/CPU identity is 37/17/x86_64. Shim/daemon property writes,
  environment overrides and stale persistence cannot downgrade it or enable
  32-bit ABI aliases. APK targetSdk remains application metadata, not platform
  identity or Binder ABI selection.
- Runtime selection requires SDK-37 metadata and an actual x86_64 ART ELF.
  Required framework bootclasspath metadata replaces the hardcoded jar fallback.
- Removed old keyboard parcel/InputDevice, activity-launch, WindowLayout and
  audio-focus signature retries and window-frame field fallback. Runtime Stub
  tables now cover service manager, application, input, audio focus, media
  session/metrics, user and thermal operations; absent tables do not guess.
- SurfaceComposer has only the covered API-37 AIDL path and 224-byte VSYNC.
  Corrected the third ContentProviderHolder flag and the WindowRelayoutResult
  resized-callback shape. Added framework-CREATOR decoding to the window gate.
- Removed the GraphicBuffer private handle offset. The BLAST consumer uses the
  source-confirmed native accessor and the pinned Android-17 C buffer layout.
  A C++ fixture tests independent object layout, handle provenance, reference
  counts, imports and CPU mapping cleanup.

### Validation on this checkout

- `cargo build --workspace --offline`: passed.
- `cargo test --workspace --offline`: passed, zero failures. This includes the
  native C++ buffer fixture, the bridge header/stub ABI tests and mapping/lifetime
  regressions, not a live app soak.
- No-Wayland daemon check: passed. Explicit Binder UAPI compilation/test against
  the provided `common-30c0eaeef78d5c19f687ad8418c27b76353ab261` kernel tree: passed.
- Negative runtime-selection check: `.toolchain/sysroot` reports SDK 33 and is
  rejected before ART loads. It was not run as an Android runtime.
- Both real API-37 CheckJNI gates **skipped**: `.toolchain/sysroot-android17` is
  absent here. Configuring a missing fixture explicitly now fails instead of
  skipping. These tests were not newly passed on a real Android-17 runtime.
- Host capability query: Intel UHD 630, Mesa GLES 3.2, Intel iHD VA-API 1.22 and
  reported VLD profiles. This is discovery, not NewPipe hardware decode evidence.

Logs: `target/android17-hardlock-{build,tests,framework,cli,kernel-uapi,no-wayland,host-gpu,explicit-runtime}.log`.
The bridge ABI tests are `cargo test -p pengudroid-nativebridge`; the guest
fixture is built into `target/arm64-fixture/` by
`tools/build-arm64-bridge-fixture.sh`.

### Next concrete work / blockers

1. Supply the verified API-37 x86_64 runtime and rerun CheckJNI plus a bounded
   NewPipe launch/search/VOD/pause/seek/resize/exit/reopen flow. No fallback to
   the old tree. `.toolchain/references/binary_translation-android17` is now
   populated (AOSP `android17-release`, revision `1290e88c`); the other source
   trees indexed in [ANDROID17.md](ANDROID17.md) are what this checkout has.
2. Audit/replace Codec2's fabricated native C++ callback/Parcel/object layouts.
   Its fabricated decoder-list/handle fallback is removed; unavailable native
   service discovery and dispatch now fail explicitly. BLAST still has RefBase/refcount and opaque
   storage/libc++ assumptions to validate. **The entire native ABI migration is
   not complete.** Do not hide these gaps behind an API-37 version property.
3. Integrate the actual hardware media adapter and measure bounded lifetimes
   with RSS/PSS/fd/maps/JNI ownership. Intel host decode fixtures do not establish
   NewPipe decoding; AMD/NVIDIA and Vulkan WSI require independent implementation
   and hardware validation.
4. The present BBR2 XAPK has **18 AArch64 native libraries and no x86_64 payload**
   (offline ZIP/ELF inventory, no app execution). Its ARM64 execution requirement
   is independent of the API-37 platform target and is now isolated in
   `pengudroid-nativebridge`; see [NATIVE_BRIDGE.md](NATIVE_BRIDGE.md) for the
   bridge build blocker and the ordered list of what BBR2 still needs.

## Previous recorded checkpoint (2026-10-27)

The checkpoint below describes the prior machine/session, not new runtime
validation in this checkout.

### Non-negotiable hard lock: Android 17 / API 37 x86_64 only

Android 17 / API 37 on x86_64 is the **only** runtime target. When this work
resumes:

- No API-33 code path, wire default, layout retry, symbol fallback or test gate
  may remain anywhere. "Try both layouts" is forbidden; a missing contract must
  **fail loudly** instead of guessing.
- Wire contracts are derived from the loaded framework's own `Stub` constants,
  measured binaries, or AOSP sources under `.toolchain/references/`
  (`native-refs_heads_android17-release` = `frameworks/native`,
  `libhybris/compat/ui/ui_compatibility_layer.cpp` = gralloc/GraphicBuffer
  contract). Never fabricate vtables or guess transaction numbers.
- The only runtime sysroot is `.toolchain/sysroot-android17`
  (GSI `CP2A.260605.012`, SHA-256 `3da2ca55…`). `.toolchain/sysroot` (API 33)
  is not a target and not a fixture.

### Verified state at handoff

- Workspace build clean; **255 tests, 0 failures** (the API-33 full-framework
  gate `real_art_keyboard_and_local_window_layout` was deleted with the hard
  lock; its assertions are covered by the two API-37 CheckJNI gates).
- PlayerService binds end to end on API 37 (see the next section): service
  transaction codes installed from the framework Stub
  (`[16, 35, 40, 42, 43]`), 7-field Intent walk (`mExtendedFlags`),
  4-arg `ServiceDispatcher.connected` with null `IBinderSession`.
- Hard-lock edits applied this session (uncommitted at the time of writing):
  `binder.rs` application codes empty until the framework installs them,
  `apphost.rs` intent walk + `connected` signature, `art_framework.rs` API-33
  gate deleted, `pengudroid-gui/src/runner.rs` default sysroot →
  `sysroot-android17`.

### Superseded checklist

The old selection/signature/default checklist was addressed in the current pass
above; the remaining native/media contracts are explicitly still open. The
loaded platform alone chooses wire ABI. Image-defined application compat policy
continues to use the APK's targetSdk, as Android 17 does; that does not retain
an older Android runtime.

### Product targets (after the hard lock is complete), in order

1. **NewPipe flawless, no leaks**: launch → search → deterministic VOD playback
   → pause/seek → resize → exit → relaunch. The synthetic-tap harness
   (`PENGUDROID_SYNTH_CLICK="x,y@delayMs[;…]"`, `PENGUDROID_CLICK_TEXT`) must
   reach a *playing* stream — `target/newpipe-api37-player-final/` shows the
   last tap ended on the feed/drawer. Then the API-37 `MediaCodec`/Codec2 path
   with retained logs, gralloc/GraphicBuffer allocator+mapper contracts
   (contract source: `libhybris/compat/ui/ui_compatibility_layer.cpp`), and a
   memory lifetime audit (BLAST native queue + Java global refs) with RSS+PSS
   over sustained playback, seek/reconfigure, minimize/restore, reopen.
2. **HW acceleration (decode + 3D) on Intel, AMD and NVIDIA** (Intel VA-API
   works; AMD needs validation, NVIDIA needs NVDEC; Vulkan WSI stays separate).
3. **Beach Buggy Racing 2** eventually. The available package is arm64-only;
   compatibility is not established. Required service coverage must be measured,
   not treated as proof that a booted `system_server` is necessary.
   See [LIMITATIONS.md](LIMITATIONS.md) §2.1 for the ARM64 execution gap
   (`libndk_translation` dies in `ndk_translation_HandleNoExec`; the
   `.toolchain/gbos-x86-main/isa-emu` trap-and-emulate pattern is x86-on-x86
   SIMD, not ARM64).

### Verification commands

```sh
cargo build --workspace --offline   # refresh runtime shared libraries first
cargo test --workspace --offline
cargo test -p pengudroid-cli --test art_framework --offline -- --nocapture
git --no-pager diff --check
```

Live runs are always bounded: private socket via
`PENGUDROID_SOCKET=<dir>/daemon.sock`, daemon started first
(`target/debug/pengudroid-daemon --socket <dir>/daemon.sock`), launch with
`--sysroot .toolchain/sysroot-android17`, then kill both process groups
(SIGTERM → SIGKILL) and retain `<dir>/app.log` + `daemon.log`.

## Android 17 PlayerService binds end to end (2026-10-27)

The API-37 service path now delivers NewPipe's `PlayerService` binding. A bounded
Wayland run (`target/newpipe-api37-player-final/`) captured the bind request,
unmarshalled the real `Intent`, created the service, ran `onBind`, and delivered
`LoadedApk$ServiceDispatcher.connected`. Three measured API-37 contracts were
wrong and are now derived from the loaded framework and AOSP sources:

1. **Service transaction codes are dynamic.** `IActivityManager$Stub` no longer
   uses the API-33 numbers: the loaded framework reports
   `[16, 35, 40, 42, 43]` for `getActivityClientController`,
   `getContentProvider`, `startService`, `bindService` and `bindServiceInstance`.
   `bindIsolatedService` has no Stub constant in Android 17; NewPipe's
   `ContextImpl.bindService` arrives as code 43, i.e. `bindServiceInstance`.
   The broker now installs all five from the generated Stub (source:
   `IActivityManager.aidl`, android17-release) instead of fixed 27/30.
2. **`Intent` gained `mExtendedFlags`.** `Intent.writeToParcel` writes
   `action`, `data`, `type`, `identifier`, `flags`, **`extendedFlags`**,
   `package`, then `ComponentName`. The broker's back-walk over six fields
   therefore landed four bytes late (`Uri` type `-1`, "Unknown URI type").
   It now walks seven fields; the old `offset+4` API-33 retry is removed with
   the hard lock. The result is
   `act=bind_player_holder_action cmp=org.schabi.newpipe/.player.PlayerService`
   rather than the bare fallback intent that made `onStartCommand` report a
   "useless intent" and stop the service.
3. **`ServiceDispatcher.connected` gained an `IBinderSession`.** The signature
   is `(ComponentName, IBinder, android.app.IBinderSession, boolean)` — note
   `android.app`, not `android.os`. The session is nullable and
   `ServiceConnection` forwards to the two-argument `onServiceConnected` when it
   is null, so ordinary binds pass null. The API-33 three-argument form was
   removed with the hard lock.

Verified in `target/newpipe-api37-player-final/app.log`:
`binder: framework application transaction codes = [16, 35, 40, 42, 43]`,
`[binder] service request: bind .../PlayerService`, the unmarshalled Intent,
`service: creating .../PlayerService`, `created`, `onBind returned a binder`,
`service: connected 1 pending binding(s) for .../PlayerService`. A
`MediaSessionCompat` media-button warning is expected with no system UI.

**Playback and `MediaCodec` remain unverified on API 37.** No ExoPlayer,
`MediaCodec` or Codec2 activity appeared in this bounded run (the synthetic tap
ended on the feed/drawer, not in a playing stream), so no decode, frame or A/V
claim is made. A regression in `crates/pengudroid-shim/src/binder.rs`
(`android17_intent_location_includes_extended_flags`) pins the new Intent field
order; 16 Binder tests and the workspace build pass.

## BLAST loader is Android-17-only; the API-33 gate is deleted (2026-10-27)

`blast.rs`'s `libgui` loader resolves the Android 17 symbols only: the
`std::optional<std::function<…>>` trailing forms of
`BufferItemConsumer::acquireBuffer`/`releaseBuffer`, and the Android-17-only
`BLASTBufferQueue` constructor and `update` (both **required** — a missing
entry point fails the load loudly with
`[blast] libgui/libutils is missing a required entry point`). A session
experiment that added an API-33 `guest_sym_first` fallback to the short
`acquireBuffer`/`releaseBuffer` forms and made `bbq_ctor`/`bbq_update`
optional was **reverted**: it fixed the API-33 fixture but violates the hard
lock. The API-33 full-framework gate
(`real_art_keyboard_and_local_window_layout`) that demanded it is deleted
instead; its assertions are covered against the real API-37 runtime by
`android17_core_services_under_checkjni` and
`android17_blast_lifecycle_under_checkjni`.

Validation: both API-37 CheckJNI gates and 3 BLAST unit tests pass; the
workspace run is **255 tests, zero failures**.

## Android 17 NewPipe UI renders and continues drawing (2026-10-27)

The immediate mapper abort was triggered by `ANativeWindow_tryAllocateBuffers`,
not by an obligatory dequeue. This NDK API is a best-effort preallocation hint:
implementations are explicitly allowed to allocate nothing. The shim now ignores
that hint, consistent with the host EGL FBO rendering path, rather than eagerly
entering Android's unavailable gralloc HAL. Actual allocation/dequeue operations
are unchanged. Reference: [NDK native-window contract](https://developer.android.com/ndk/reference/group/a-native-window#anativewindow_tryallocatebuffers).

Two bounded real Wayland runs verified progress:

- `target/newpipe-api37-preallocate/`: the first 720x1280 UI frame contains the
  NewPipe header/tabs. Pointer interaction reached a video-detail view. A
  compositor close retired the guest's desktop windows; no gralloc abort occurred.
- `target/newpipe-api37-frame-progress/`: the guest was still running after
  20.01 seconds, before test cleanup. The EGL dump counter reached 550 across
  UI surfaces. 55 sampled images had 52 distinct SHA-256 hashes. This is UI
  frame progress, **not decoded video frames or a stable frame-rate claim**.
  The final sample is retained as `docs/screenshots/newpipe-android17.png`.

The test broker/guest process groups were stopped afterward. Logs, sampled
frames and `result.json` are in those target directories. The shim build,
5 native-window unit tests and 2 API-37 ART/CheckJNI tests passed;
`git diff --check` was clean.

**Mandatory API-37 GraphicBuffer allocation is still incomplete.** Binary
inspection shows libui LTO inlines mapper construction and backend calls inside
GraphicBuffer constructors. The existing no-op constructor hooks cannot catch
those operations. Only the singleton pointer slot is zeroed `.bss`; its heap
object is not guaranteed zero-initialized. Bootstrapping that singleton alone
would merely replace the HAL abort with an invalid backend access. A matching
C++ backend or source-built explicit shim boundary is required for that path;
no fabricated mapper vtable or singleton pointer was introduced here.

Next: verify PlayerService creation/binding and the API-37 MediaCodec path with
retained logs, then implement required allocator/mapper contracts for real video
buffers. Playback, pause/seek, hardware decode and broad app stability remain
unverified. Existing service/resource warnings and multiple app windows remain.

## Android 17 native display discovery passes; gralloc is next (2026-10-27)

A bounded NewPipe run reached the real native BLAST `update()` and then exposed
hwui's `Failed to acquire physical displays for WCG support!` abort. The API-37
SurfaceComposer replies were missing static/dynamic display information and the
composition-preference reply lacked its parcelable framing.

Measured from the shipped x86_64 `libgui.so`:

- `getStaticDisplayInfo` is transaction 12; `getDynamicDisplayInfoFromId` is 13.
- Both return AIDL Status, presence=1, and a self-sized structured parcelable.
- Supported display modes require per-element presence/size and a nested sized
  `gui::Size`. The hosted display is fixed at 720x1280, 320 dpi, 60 Hz, mode ID 1.
- Transaction 34's composition preference is also present/self-sized, not four
  raw words. It reports UNKNOWN dataspace and RGBA_8888, not wide-gamut support.
- Dynamic info intentionally ends at activeColorMode, which the generated reader
  accepts; later capabilities retain defaults. This is a supported prefix, not
  a complete implementation of HDR/refresh-rate capability metadata.

A new native-display probe uses the real `libnativedisplay.so` API to acquire the
single internal display, query its current config/size/FPS and SDR preferred
format, then release the returned allocation. Both API-37 CheckJNI framework
regressions pass with this probe enabled. A standalone unit regression checks
nested parcelable sizes. Final focused verification passed 15 Binder tests,
3 BLAST tests and 2 API-37 ART/CheckJNI tests; `git diff --check` was clean.
CLI/shim/daemon/gleshim builds succeeded.

A separate real Wayland broker run then passed display discovery and native
BLAST update, but aborted in `GraphicBuffer::GraphicBuffer()` at
`libui.so+0x2fd0f`: `GraphicBufferMapper: gralloc-mapper is missing`. The stack
runs through BufferQueueProducer::allocateBuffers and ANativeWindow_tryAllocateBuffers.
No frame dump was produced; there is no NewPipe rendering/playback claim yet.
The next concrete task is the API-37 GraphicBuffer/mapper constructor and allocator
boundary, followed by continued hwui frame progress and transaction/release work.
The private broker and guest process groups were stopped after the bounded run.

Evidence: `target/newpipe-api37-check/{app,daemon}.log` (sandbox diagnostic),
`target/newpipe-api37-render/{app,daemon}.log` (real Wayland),
`target/art37-display-{build,tests,unit}.log`, `target/art37-render-build.log`.

## Android 17 real BLAST lifecycle checkpoint (2026-10-27)

The in-progress native bridge now returns a real `BLASTBufferQueue*` to Java
and hwui, while retaining the separate hosted producer/consumer for presentation.
The native queue handle must be mapped to the Rust `Blast` state: casting the
C++ address directly to `Blast*` is invalid. The registry and teardown regression
now exercise these distinct addresses. The native queue/storage and Java global
reference still have process-lifetime retention; ownership cleanup is not solved.

The standalone ART lifecycle probe also needs the loaded framework's
ServiceManager transaction table **before** constructing the native queue.
Without it, `onFirstRef()` → `initialize()` fails to look up SurfaceFlingerAIDL
and faults at `libgui.so+0xec5c2`. The application binding path already installed
this table; the lifecycle probe now does so independently of other probes.

Validation: CLI/shim offline build succeeded; all three BLAST unit tests passed;
the full lifecycle/framework probe passed on the real API-37 tree with CheckJNI.
A dedicated `android17_blast_lifecycle_under_checkjni` regression now covers this
runtime explicitly. Both API-37 regressions passed together (2 tests, no failures).
Each framework probe now uses a private temporary XDG cache, avoiding concurrent
compat-symlink replacement and writes to the user's existing cache.
Logs: `target/art37-blast-{build,probe}.log`, `target/art37-regressions.log`.
No live NewPipe render/playback result is claimed. The next gate is hwui's real
`nSetBLASTBufferQueue` path and continued frame progress, followed by transaction
callbacks/release semantics and the existing playback/interaction checklist.
The broader shim run previously failed in the unrelated DNS negative-name test;
`rustfmt` is not installed in the selected stable toolchain.

## Android 17 hard lock: window, input and surface gates pass (2026-10-26)

The runtime is now **Android 17 only**. The API-33 reply shapes and the legacy
`IWindowSession` transaction table were deleted, not kept as a fallback:
`WINDOW_SESSION_CODES` starts empty and is filled from the loaded framework's
own `IWindowSession$Stub.TRANSACTION_*` constants. There is no "old Android"
branch left in the window/input path.

Two measured API-37 wire contracts were wrong and are now derived from the
shipped libraries rather than guessed:

1. **AIDL parcelables carry a self-size prefix.** `android::os::InputChannelCore`
   is written as
   `[int32 isInitialized=1][int32 coreSize][String16 name][int32 kNonNullParcelableFlag=1][int32 hasComm=0][flat fd][null binder + stability]`.
   Source of truth: disassembly of `InputChannelCore::writeToParcel` /
   `readFromParcel` in `libinput.so`, plus `Parcel::writeData(const Parcelable&)`
   in `libs/binder/include/binder/Parcel.h`. The missing `coreSize` and the
   missing parcelable flag were exactly the two reasons the client logged
   `channel '' ~ Could not make socket (-1) non-blocking`.
2. **`ISurfaceComposerClient` is AIDL on Android 17.** `createSurface` returns
   `gui::CreateSurfaceResult` (`[Status][flag][size][handle+stability][layerId]`
   `[String16 layerName][transformHint]`), not the old
   `[handle][gbp][layerId][transformHint][status]`. The old reply made the client
   fail with `SurfaceComposerClient::createSurface error Operation not permitted`
   / `PERMISSION_DENIED`.

Verified in `/tmp/a17-run8.log` / `/tmp/a17-rc6.log`: NewPipe creates **two** real input
channels (`Input channel constructed: name='pengudroid', fd=107` / `fd=109`), both
windows receive a `SurfaceControl`, the frame is 720×1280, the app reaches
`Looper.loop()`, and then:

- vsync is delivered and `NativeDisplayEventReceiver::dispatchVsync` runs,
- `Choreographer` dispatches the first frame callback,
- `ViewRootImpl.performTraversals` → `relayoutWindow` → `updateBlastSurfaceIfNeeded`
  creates the BLAST buffer queue (`[blast] update 848x1408`) and hands out a
  `Surface`,
- and the process then faults in `libhwui.so+0x3275d6` while wrapping the surface
  pointer the Java side passed down (`sp<T>` construction on an object whose
  vptr is 0; `si_addr = -0x18`, i.e. `vptr[-3]`). That is the current blocker.

The faulting function was identified from the library itself (scanning the
`JNINativeMethod` table for the function address):

```
libhwui.so+0x3275b0  nSetBLASTBufferQueue  (JJ)V
```

So Java hands `RootRenderNode::setBLASTBufferQueue(sp<BLASTBufferQueue>)` the
value that `BLASTBufferQueue.nativeCreate` returned. `blast.rs` returns a
`Box<Blast>` — a Rust struct whose first word is not a C++ vptr — while the
framework expects a real `android::BLASTBufferQueue`. That is the whole crash.

`libgui.so` does export the matching constructor
`_ZN7android16BLASTBufferQueueC1ERKNSt3__112basic_stringIcNS1_11char_traitsIcEENS1_9allocatorIcEEEEb`,
whose signature is exactly `nativeCreate(String name, boolean
updateDestinationFrame)`. The next step is therefore to host a **real**
`BLASTBufferQueue` instead of the hand-rolled `Blast` object. That in turn means
the producer's buffers have to be released the way SurfaceFlinger releases them
(`setTransactionState` + the consumer side of the queue), otherwise hwui stalls
once the maximum acquired-buffer count is reached. That is the BLAST lifecycle
work already listed in [ANDROID17.md](ANDROID17.md), now with a concrete entry
point.

Three more real contracts were needed to get this far:

3. **`AndroidRuntime::mJavaVM` must be published.** Android's build uses LTO, so
   framework translation units inline `AndroidRuntime::getJNIEnv()` and read the
   static member directly through the GOT. `dispatchVsync` is one of them and
   faulted on a null `mJavaVM` at the first vsync. The shim now writes the VM
   into the exported `_ZN7android14AndroidRuntime7mJavaVME` slot
   (`__pengudroid_publish_java_vm`).
4. **BLAST `acquireBuffer`/`releaseBuffer` gained a trailing
   `std::optional<std::function<void(const sp<GraphicBuffer>&)>>`.** The old
   three-argument symbols no longer exist in `libgui.so`, which made the whole
   BLAST bridge bail out with "libgui/libutils is missing a required entry
   point". The shim now looks up the new symbols and passes a zeroed 32-byte
   disengaged `libc++ optional` by invisible reference (the Itanium ABI for a
   non-trivially-copyable by-value parameter).

A diagnostic fault handler was also added: `PENGUDROID_CRASH_BT=1` makes the
host install a `SA_SIGINFO` handler after `JNI_CreateJavaVM` and again right
before `Looper.loop()`, printing `rip`, `rsp`, `si_addr` and a `lib+offset`
backtrace. It goes through `dlsym(RTLD_NEXT)` because the shim itself interposes
`sigaction`/`sigaltstack` for Bionic compatibility.

Removed with the hard lock: `ViewRootImpl.mForceDisableBLAST` (the field does not
exist in Android 17; BLAST is the only render path), `InsetsState.SIZE`, the
`RELAYOUT_FRAMES`/`ClientWindowFrames` reply prefix, and the
`surface_composer_client` "status last" special case.

## Earlier Android 17 runtime checkpoint (2026-10-26)

A real, checksum-verified API-37 x86_64 GSI is now extracted separately at
`.toolchain/sysroot-android17`. Its ART boots (JNI_OK, imageless fallback), and
174 framework + 2 HWUI groups register. Core CheckJNI probes pass for modern
ServiceManager unions, settings, audio-focus token signatures, WindowLayout,
native keyboard maps and native ApplicationSharedMemory. The legacy full
framework gate still passes. See [ANDROID17.md](ANDROID17.md) for source
provenance, commands and exact coverage.

NewPipe's Application and Activity.onCreate are reached under API 37. Its real
targetSdk is preserved; default-disabled and target-gated compat changes now
come from the image XML instead of an empty disabled list. Modern EGL telemetry
hint handling fixes the measured EGL_BAD_ATTRIBUTE failure.

The latest API-37 app run is `/tmp/pd-android17/newpipe-egl17.log`: graphics
context creation succeeds, then window/input-channel setup fails with socket -1.
The full API-37 BLAST lifecycle gate still fails because new libgui acquire/release
signatures add libc++ optional/function arguments. These are the next concrete
integration boundaries; do not report completed migration, a rendered API-37
NewPipe window, or API-37 media playback. Default app runtime remains API 33
until these gates pass. The API-37 tree is selected explicitly with --sysroot.

Checkpoint verification: **251 workspace tests passed**, including the local
API-33 full framework test and API-37 core/shared-memory test. Build and
`git diff --check` passed. The separately requested full API-37 lifecycle gate
still fails at the libgui boundary; it is not included in that green claim.
Logs: `/tmp/pd-android17/workspace-checkpoint.log`, `shared-framework-tests.log`,
`full-framework17-current.log`, and `egl-tests.log`.

Additional open work: Aconfig storage provisioning, readonly/shared app bootstrap
state across processes, display/insets/session parcels, Binder callbacks/death,
boot-image GC compatibility, Codec2 AIDL, ARM64 and the broader old/new APK matrix.

## Platform target: Android 17 / API 37

The user requested migration of Binder, media and the platform layer to Android
17 while retaining backward app compatibility. This extends the existing broad
Linux Android compatibility goal; the NewPipe/desktop milestone remains work
within that goal. See [ANDROID17.md](ANDROID17.md) for pinned source findings,
implemented code and migration gates. **The runtime is hard-locked to API 37**:
`.toolchain/sysroot-android17` is the only sysroot, and the API-33 sysroot is
neither a target nor a fixture. See the HANDOFF section above for the remaining
hard-lock checklist.

The first code change resolves audio-focus transaction numbers from the loaded
framework's generated Stub, installs all four as one validated table and uses
semantic dispatch in the Rust broker. This removes one legacy numeric dependency;
it does not port the complete service or establish Android 17 support.
The migration build and **248 workspace tests passed**; logs are in
`/tmp/pd-android17/`. Previous milestone counts below are historical.

## Active goal: stable NewPipe and a reliable desktop shell (2026-10-26)

The requested product target is end-to-end stable NewPipe playback and interaction, a responsive launcher with real app icons/names and per-app controls, desktop-compatible movable/resizable/closable windows, forced termination of hung guests, and a PenguDroid-owned transparent keymapper control outside the Android renderer. A real game, ultimately Genshin Impact, follows this foundation. This goal remains active.

### Settings provider reply contract corrected (2026-10-26)

Live keyboard input produced repeated `Bundle` type warnings in
`Settings.NameValueCache.getStringForUser`. The default response had a serialized
map under `value`, even for scalar GET calls. Installed per-key responses also
used the setting name as their Bundle key, although Android 13 reads `value`.

- Scalar settings now carry their string under `value`; unknown scalar settings
  receive an empty Bundle and use the framework's default.
- Unsupported `LIST_config` returns a null Bundle. Android 13 explicitly treats
  this as an empty result, instead of reading a scalar or a missing map.
- The real ART integration probe transacts through BinderProxy, reads replies
  with Java Parcel/Bundle, and verifies a known scalar, an unknown scalar and
  an unsupported list. A list containing a known setting name is also covered
  by the broker regression. This probe uses the hosted provider's request-key
  matching, not a complete IContentProvider AIDL argument parser.
- Build and **247 workspace tests passed**; logs are
  `/tmp/pd-resume/settings-build.log`, `settings-probe.log`,
  `settings-tests-exact.log`, and `checkjni-probe.log`.

Matching now requires a whole aligned Parcel String16 field, so an unknown key
such as `prefix_android_id_suffix` cannot select `android_id`'s reply. Both the
broker regression and the real framework probe cover this case.

The updated NewPipe run (`settings-live.log`) has **zero** instances of the
previous scalar Bundle type warning or LazyValue-to-String cast error,
including delivered keyboard events. The actual ART integration test also
passes with `-Xcheck:jni`, now enabled permanently in that test. An independent
NewPipe startup under CheckJNI reached its UI without a JNI fatal diagnostic;
that temporary diagnostic guest was then stopped. Log: `checkjni.log`.
This is startup/probe coverage, not a full pause/seek/reconfiguration test.

Source contract: [Android 13 Settings.java](https://android.googlesource.com/platform/frameworks/base/+/android-13.0.0_r1/core/java/android/provider/Settings.java),
`getStringForUser` and `getStringsForPrefix`.

The resume daemon also needed restarting after the workspace rebuild: leaving
an older daemon running while loading newer guest libraries closed the first
presentation connection. The matched runtime is logged in
`/tmp/pd-resume/daemon-current.log` and `new.log`. This run submitted over
4,000 software video frames. The ART shadow-frame failure has not recurred;
pause/seek/resize/exit/relaunch coverage is still incomplete.

### Resume verification and launcher preference fix (2026-10-26)

The desktop keymapper checkbox was persisted but not forwarded to the guest:
any saved profile could remain active even when the UI showed it disabled.
The GUI now passes `PENGUDROID_KEYMAPPER=0/1` per app. The ART pump skips
profile loading for `0`, so ordinary key delivery remains active. CLI launches
retain automatic profile loading by default. Changes apply on the next launch;
live profile reload and the external transparent button remain open.
A real child-process test verifies enabled/disabled preferences override an
inherited environment. A fresh NewPipe run logged
`input: keymap disabled for org.schabi.newpipe`.

The first resume check found stale top-level runtime shared libraries:
`cargo test` had rebuilt `target/debug/deps/libpengudroid_shim.so`, but the
launcher loaded the older `target/debug/libpengudroid_shim.so`. After
`cargo build --workspace --offline`, the real ART BLAST lifecycle check passed.
Build runtime artifacts before running integration checks; test compilation
alone does not guarantee these dynamically loaded libraries are refreshed.
The hung-child test also now polls its actual process state with a one-second
bound, because SIGKILL delivery to descendants is asynchronous.

New runtime evidence is under `/tmp/pd-resume/`. The fresh guest retains
`PENGUDROID_TRACE_BT=top_shadow_frame_`; no recurrence has yet been captured.
This does not resolve or disprove the previously observed ART failure.

### Runtime per-app keymapper

Keymap profiles are applied at runtime for ART apps. The input pump loads
`$XDG_CONFIG_HOME/pengudroid/keymaps/<package>.keymap` (or `~/.config/…`), the
same path the GUI editor writes, and turns each bound key into an Android touch:
`tap` presses and releases at a point, `hold` presses until the key is released,
and `swipe` interpolates a drag. A bound key is consumed, so it never also
reaches the app as a key press, and auto-repeat does not repeat a gesture. The
GUI's controls editor and the runtime therefore agree on the file format and the
location.

The profile path is resolved by `pengudroid launch` *before* the per-app prefix
overrides the guest's `HOME`, and passed as `PENGUDROID_KEYMAP_PATH`; without
that step the pump looked inside the prefix and found nothing. Profiles are read
once per launch, so an edit applies on the next launch.

Verified live in NewPipe with a three-binding profile: `F2 = tap 668 64` opened
search, `F3 = hold 360 500` opened the first feed item (short press; a one-second
press is a long-press), and `F4 = swipe 400 1000 400 300 10` scrolled the detail
page to the comments. Gesture conversion is unit-tested in
`crates/pengudroid-cli/src/keymap_gesture.rs` without ART. The transparent
guest-attached keymapper button is still not implemented.

### Desktop Back key

A desktop keyboard has no Android Back key, and `KEYCODE_ESCAPE` is not a substitute:
most apps never handle it, so leaving an Android screen required the mouse.
`KEY_ESC` and `KEY_BACK` now translate to Android `KEYCODE_BACK` (4). Verified
live in NewPipe: opening search and pressing either key dispatched `android=4`
and returned to the feed (`/tmp/np7.log`); the unit test is in
`crates/pengudroid-cli/src/keyboard.rs`.

### Input subscription lifetime fix

One process can open multiple daemon connections for rendering, Binder traffic and framework service calls. Input subscriptions now carry a unique connection id as well as the process id. Closing one connection removes only the subscriptions it owns; process exit still removes all subscriptions for that process. Previously, a retiring connection from the same process could silently remove a still-live input subscription and make keyboard/pointer input stop arriving.

### Host hardware decode and GPU video import milestone

A real host-only VA-API decoder is now implemented behind an optional, versioned C bridge, with a Rust RAII API and the `video-decode-test` CLI command. It rejects software fallback, accepts complete compressed access units with microsecond timestamps, supports flush/EOS, and retains at most four returned GPU frames. GPU export synchronizes the VA surface and preserves composed DMA-BUF plane/object metadata. Returned frames safely outlive their decoder; closing frames releases descriptors and decode surfaces.

On Intel UHD 630, real 128×128 and 1280×720 constrained-baseline H.264 gradients decoded to two-plane NV12. Explicit diagnostic readback verified luma 64. The normal decode/export path does not read pixels back. A 2,000-frame lifecycle stress run kept descriptor count at 5 → 5 and RSS at 31,496 → 31,580 KiB after warmup, while also testing held-frame backpressure, timestamps, EOS, flush and decoder-before-frame destruction. Logs: `/tmp/pd-video-{decode-test,hardware-lifecycle}.log`.

The graphics broker has a direct multi-plane video-import API. A real GNOME compositor test imported 32 GPU-decoded NV12 frames without CPU conversion/upload, preserved both planes and modifiers, and waited for `wl_buffer.release` before recycling source frames. The last frame was released after its window closed. DMA-BUF v3 is bound to obtain the explicit format/modifier list; unsupported pairs are rejected before import. Log: `/tmp/pd-video-hardware-display.log`. The workspace build and 237 workspace tests passed; explicit hardware decoder/compositor runs and the no-Wayland daemon check also passed. The native bridge build script was verified against a locally extracted SDK, then its output decoded the 720p fixture successfully.

The captured NewPipe run selects `c2.android.avc.decoder` (`video/avc`) and produces 1280×720 HAL `RGB_565` buffers through its current software Surface/BLAST route. This identifies the hardware adapter boundary; it has not been switched to the host decoder.

**This is not yet NewPipe hardware decoding.** The Android Codec2 component adapter and compressed-packet broker transport are still missing. The new host decoder has not been enabled for the running NewPipe guest. NVIDIA NVDEC, AMD hardware validation, other compressed codecs, HDR/color metadata, audio/video pacing and Vulkan WSI remain separate work. See [HARDWARE_VIDEO.md](HARDWARE_VIDEO.md) for build requirements, exact reproduction commands and the next integration boundary.

### GPU execution and hardware video decode target

The user requires GPU 3D work and hardware video decoding, with Intel, AMD and NVIDIA coverage. OpenGL/EGL, Vulkan and media decoding are separate compatibility paths and must be validated independently. Software rendering/decoding must be visible in diagnostics; capability discovery is not evidence that an application is using that backend.

Current findings on this machine: Intel UHD 630, accessible DRM render node, working Intel iHD VA-API 1.22. Runtime driver queries report VLD support for H.264 Main/High/Constrained Baseline, HEVC Main/Main10, VP8 and VP9 profiles 0/2 (also MPEG2 and JPEG). `pengudroid gpu-info` now reports actual per-render-node decode profiles and explicitly identifies the current guest software Codec2 path. No support is inferred from GPU vendor alone. The RAII probe terminates VA displays and closes library/device handles.

NewPipe still decodes through Android software Codec2 plugins. The host hardware decoder is implemented separately, but it is not connected to the guest. The needed adapter must consume MediaCodec/Codec2 compressed input, use a host decode backend, preserve timestamps/flush/EOS, and submit bounded, synchronized GPU surfaces. VA-API is the first backend on this Intel host; AMD VA-API and NVIDIA NVDEC/driver-specific paths require separate implementation and real hardware validation. The present wire format still carries one DMA-BUF plane; the new direct broker API imports multiple planes, but guest transport still needs extension. Host Vulkan loader linkage alone does not implement Android Vulkan window-system integration.

The software presentation path now uses the compositor viewport for video scaling when available, avoiding per-frame CPU resizing. Pixel conversion/copying and software decoding remain until the hardware pipeline is connected. Official references: [VA-API core and synchronization](https://intel.github.io/libva/group__api__core.html), [DRM PRIME surface descriptors](https://github.com/intel/libva/blob/master/va/va_drmcommon.h).

### Memory lifetime fixes (current priority)

Two unbounded graphics-resource lifetimes were identified and corrected:

- GraphicBufferMapper CPU locks (including YCbCr and async unlock) and AHardwareBuffer locks now share a native-handle-keyed mapping registry. Nested locks reuse a mapping, the final unlock unmaps it, and buffer destruction clears outstanding mappings before closing the descriptor. Previously each lock created another mapping and unlock never removed it.
- Imported DMA-BUF Wayland buffers now use release-driven destruction, like software snapshot buffers. They no longer replace the separately owned fallback SHM buffer. Each surface accepts at most three outstanding frame buffers; incoming frames are dropped before import/copy when that bound is reached. Surface destruction also retires outstanding buffers.

Validation: the Linux `/proc/self/maps` regression performed 2,000 nested lock/write/unlock cycles of a 1 MiB memfd and verified zero remaining mappings after each final unlock, plus cleanup of an outstanding lock on destruction. A real GNOME/Mutter test performed 1,000 software-frame submission attempts, verified the three-buffer bound and compositor release cleanup, then destroyed the surface cleanly. The workspace test run passed 236 tests; the explicit GNOME frame-lifetime stress run also passed. A subsequent 60-second live NewPipe decoder run measured guest RSS from 371,216 to 379,620 KiB, daemon RSS from 14,764 to 15,920 KiB, guest mappings from 2,562 to 2,565, and guest file descriptors from 193 to 192. Daemon mapping count stayed at 79. The remaining guest growth needs longer profiling; these measurements do not establish that every leak is fixed. Raw samples: `/tmp/pd-desktop-live/memory-samples.jsonl`.

Remaining memory investigation: measure guest/daemon RSS and PSS during sustained playback, seek/reconfiguration, minimize/restore and repeated app open/close. BLAST native queue/global-reference ownership still needs a safe lifetime audit; its retained native allocations must not be freed while workers or Java surfaces reference them. The private live daemon was restarted with the rebuilt fixes.

### Desktop frame, compositor resize, and native signal ABI milestone

The Wayland broker now uses an Adwaita/SCTK client-side frame when the compositor does not supply server-side decorations. The frame is a host-owned set of Wayland subsurfaces, outside the Android renderer. It provides a title, close/minimize/maximize controls, compositor-driven move/resize requests using native input serials, and resize cursors when the cursor-shape protocol is available. Desktop-provided framing remains preferred when negotiated. Fullscreen hides the client frame and compositor capability/state changes update its controls.

Resize currently uses `wp_viewporter` to scale the existing guest image. Android's framebuffer/configuration dimensions remain fixed; this is compositor scaling, not an Android configuration-change lifecycle implementation. Video subsurfaces scale their destination and parent-local position with the root. Native pointer coordinates are converted back to guest pixels. New windows fit within 85% of the available logical monitor dimensions when the compositor does not propose a size; logical output information is preferred over physical-mode/integer-scale fallback. Output additions/removals are tracked.

A real GNOME/Mutter compositor test covered client-frame creation, resize geometry, video-layer scaling, inverse input-coordinate mapping, maximize/unmaximize configure events, close-request deduplication, and clean Wayland destruction. A second test checks initial portrait/landscape fitting. Interactive pointer dragging, other compositors, X11, and Android relayout after native resize still need broader validation.

Unexpected guest exits now retire their windows and input subscriptions without waiting for a guest DestroySurface call. A pidfd is retained from surface registration, so shutdown and exit detection use the captured process identity. The regression kills a real child process and verifies that its desktop surface and ownership records disappear.

#### Live NewPipe findings

The new desktop frame opened for NewPipe. During search input, the guest initially died from Linux signal 34. A syscall trace showed a default handler being installed and then a thread-directed signal 34. The shim's Bionic sigaction definition had used the LP32 field order for LP64. The Android 13 LP64 ABI puts flags first, handler at byte 8, mask at byte 16, and restorer at byte 24; the definition is now corrected. Source: [Android 13 Bionic signal types](https://android.googlesource.com/platform/bionic/+/android-13.0.0_r1/libc/include/bits/signal_types.h).

An independent C fixture using the Android LP64 layout reproduced termination by signal 34 before the fix. After the fix it installed a handler and received the actual kernel signal successfully. It is covered by `crates/pengudroid-shim/tests/signal_abi.rs`, rather than relying only on a Rust struct roundtrip.

The corrected live run reached the NewPipe player and submitted at least 60 software video frames. A separate ART failure (`Check failed: top_shadow_frame_ != nullptr`) then occurred around player/decoder reconfiguration. Its cause is not established. Do not call the whole NewPipe flow stable. A fresh NewPipe instance was left open with `PENGUDROID_TRACE_BT=top_shadow_frame_` to capture a native backtrace if this recurs.

Final validation for this milestone: workspace build succeeded, **235 tests passed**, no failures; the no-Wayland daemon build check and `git diff --check` passed. Logs: `/tmp/pd-desktop-{build,tests,no-wayland,lifecycle,signal-abi}.log`. Live evidence: `/tmp/pd-desktop-live/{daemon.log,daemon-final.log,app.log,signals-first.log,signals.log,fixed-app.log,final-app.log,signal-abi-before.log}`. Diagnostic strace was downloaded/extracted under `/tmp/pd-debug-tools`; no system package was installed.

### First implementation milestone

- GUI callbacks now share a single registry and live process map. The old callback-local registry clones made Stop and close confirmation lose track of launches.
- The GUI retains/reaps child handles and uses a dedicated process group per app. Stop runs off the UI thread and escalates from SIGTERM to SIGKILL after 300 ms, covering Android/audio descendants as well as the launcher.
- The daemon resolves window owners from EGL surface ownership, immediately removes their surfaces on a compositor close, and uses pidfd-based SIGTERM/SIGKILL escalation after 500 ms. This avoids relying on a mismatched RegisterProc surface id or guest event-loop cooperation.
- The library is now a scrolling list of app cards with actual labels/icons when resolvable. A selected app has its own right-hand preferences panel. APK resource metadata is resolved with optional Android SDK aapt/unzip; unsupported icons have an explicit fallback.
- FPS and keymapper profile preferences are persisted per package with atomic file replacement. Launch output is retained in per-app log files. FPS surface options travel through the broker rather than a shared daemon environment toggle; preferences currently apply on the next launch.
- Configure controls opens a separate PenguDroid window with profile parsing and saving. Runtime keymapping and the transparent guest-attached button are not connected yet. The UI reports that limitation; the old automatic global evdev/uinput startup is no longer used by the launcher.
- Native Wayland keyboard events now follow keyboard focus instead of pointer focus. Held keys are released on focus loss. The low-level mapper's physical keyboard reads now have a timeout so stopping cannot wait indefinitely for an idle keyboard; repeat records do not retrigger taps/swipes.

### Desktop and input work still required

The host client-side decoration fallback and compositor scaling are implemented in the milestone above. Android configuration/relayout propagation and an X11 desktop path remain open. Complete physical touch/multitouch forwarding, focus-scoped runtime keymapping, and the external transparent keymapper button remain open. uinput output is kernel-backed but virtual; it must not be described as physical touch hardware. No Genshin Impact launch has been attempted.

The GUI preview was opened on the real desktop with an isolated registry and NewPipe's real launcher icon. Preview image: `/tmp/pd-gui-foundation/launcher-final.png`. Final workspace build and **231 tests** passed; the no-Wayland daemon build check also passed. Logs: `/tmp/pd-gui-foundation-{build,tests,no-wayland}.log`. A parallel AudioTrack recovery test exposed a sink-liveness gap during this run; the pump now checks its owned child exit status as well as pipe writes, and the final suite passed after that fix.

## Previous milestone: adapt SurfaceView sizing when the window changes (2026-10-26)

The BLAST consumer already recreated its Wayland video layer when `nativeUpdate` changed the video layout, but apphost kept forcing the initial `720x405` SurfaceView size. The input pump now checks `ViewRootImpl` dimensions every 500 ms and reruns the SurfaceView sizing pass when the root geometry changes. It fits a 16:9 video frame inside the current root. `PENGUDROID_VIDEO_W` and `PENGUDROID_VIDEO_H` can still override the automatic dimensions. If the initial root size is unavailable, the code uses the existing portrait default of `720x1280` and a `720x405` video frame.

`cargo build --workspace --offline` succeeded after this change. Tests were not run in that turn. Live rotation and compositor presentation at the new size remain unverified. Apphost still creates the Android window using fixed `720x1280` dimensions, so the current change reacts to Android relayouts; it does not yet deliver the host display orientation to Android.

Build log: `/tmp/pd-orientation-build.log`.

## BLAST queue teardown and video layout updates

- `BLASTBufferQueue.nativeDestroy` now marks the queue stopped. Its consumer exits its acquire loop, destroys its daemon video layer when connected, destroys the acquired `BufferItem`, and clears the published video-surface id. Java metadata is protected by a mutex; the consumer reads immutable native pointers and atomics.
- `nativeUpdate` publishes new buffer geometry. On the next frame, the consumer retires the old Wayland layer and creates a replacement at the new layout. It reuses the current layer for ordinary frames of the same size. If the window is not ready yet, it retries on a later frame.
- The daemon associates each subsurface with its parent and removes children before destroying a window. New video subsurfaces select the most recent live toplevel for that process, rather than accidentally nesting under an older video layer.
- The ART probe created, updated, destroyed, and recreated real framework `BLASTBufferQueue` objects. Socket coverage exercises 720x405 to 1280x720 layer replacement, parent teardown, rejected replacement retry, and recreation on a new window.
- The full workspace build and 226 tests passed after that set of changes. The headless ART probe checks teardown while the consumer is waiting to connect; the socket tests cover the daemon messages for layer replacement and cleanup.

The native queue storage and Java global reference are still retained for the process lifetime. Full `RefBase` and JNI reference cleanup is not implemented. A process with multiple live toplevel windows still uses the newest toplevel as the parent. Guest-exit cleanup and an Android-driven rotation have not been validated end to end.

Build/test logs: `/tmp/pd-blast-{lifecycle,art,build,workspace}.log` and `/tmp/pd-surface-{before,after,build,no-wayland,workspace}.log`.

## Live audio recovery during NewPipe VOD playback

A real NewPipe, Wayland, and PipeWire run tested loss of the AudioTrack-owned `pw-cat` process. The system PipeWire service was left running; only that guest's sink process was killed.

- The Blender “Big Buck Bunny 60fps 4K - Official Blender Foundation” VOD was opened through normal search, keyboard input, and tapping. No forced Surface was used.
- Video advanced from 360 to 420 frames during the three seconds before interruption. `pw-cat` PID 17991, a child of ART PID 17659, was killed with `SIGKILL`.
- The app logged `AudioTrack output failed: Broken pipe (os error 32)` and then `AudioTrack sink restarted (attempt 1/5)`. A new sink process (PID 21355) started under the same ART process. Video advanced to 450 frames.
- `pw-dump` showed the new PipeWire node in `running` state with class `Stream/Output/Audio`. At about 55.7 seconds after interruption, the video reached 810 frames. Snapshots 300 and 600 had different SHA-256 values. This sample is not a fixed frame-rate guarantee.
- The user also confirmed the actual video image in a desktop screenshot. A black video rectangle in the EGL UI dump is expected because the video is presented in a separate Wayland subsurface.

This verifies recovery when the audio client process dies. Restarting the PipeWire server, changing physical audio devices, audible quality, and exact A/V synchronization have not been tested.

Evidence: `/tmp/pd-live-recovery/{app3.log,daemon.log,vod-before.json,result.json,pipewire-after.json,vod-*.ppm}`.

## AudioTrack sink recovery implementation

- A child-process pipe test kills the first sink and captures the retried PCM in a second process. Three chunks are compared byte-for-byte and in order; the playback head advances only for successfully written frames.
- A permanently broken sink is bounded to the initial start plus five restart attempts, then the track reports `ERROR_DEAD_OBJECT` (`-6`).
- If the initial `pw-cat` or `aplay` process cannot start, the track is now marked dead instead of accepting data until full and returning zero forever.
- These checks use child pipes and `/bin/cat`; they do not use the user's audio session.

The workspace build and 223 tests passed for that change. Logs: `/tmp/pd-resume-build.log` and `/tmp/pd-resume-workspace-tests.log`.

## Verified NewPipe playback and interaction

- A NewPipe 0.29.1 search opened the Blender Big Buck Bunny VOD through the ordinary UI path. More than 4,200 frames were submitted over 191.5 seconds in one earlier run (about 21.9 frames/s across that sample). The video path still uses CPU copying and scaling.
- Real keyboard input was verified in NewPipe search, including Shift, Backspace, and Enter. The virtual keyboard uses a US physical layout; Turkish/XKB mapping and IME composition are not implemented.
- Android `WindowConfiguration` bounds are populated with 720x1280 and fullscreen mode, preventing a later relayout from collapsing the window to 0x0. The framework's own `WindowLayout.computeFrames` probe returned 720x1280.
- Autofill is reported absent because no autofill engine is hosted, avoiding a five-second callback timeout.
- The video subsurface has an empty input region so pointer events reach the app's `ViewRootImpl`.

These observations verify the listed app and run only. They do not establish general Android application compatibility, exact A/V synchronization, or stable playback across all streams.

## Build and verification history

- The latest workspace build and test run passed: **255 tests, zero failures**
  (the API-33 full-framework gate was deleted with the Android 17 hard lock;
  the two API-37 CheckJNI gates remain).
  Build runtime shared libraries before integration checks: test compilation
  alone does not refresh `target/debug/libpengudroid_shim.so`.
- Optional hardware fixtures still require the explicit environment described in HARDWARE_VIDEO.md.
- Earlier BLAST and parent-lifecycle changes passed 226 tests. The orientation-sizing work was included in the latest full run.
- `git diff --check` was clean after the last edit.
- Build warnings remain in the existing codebase.

## Next work

0. Finish the remaining hard-lock checklist in the HANDOFF section above
   (binder/SurfaceFlinger legacy paths, keyboard parcel, `force_window_frame`,
   boot-jar list, props 37/17, `audiofocus` LEGACY codes, compat-policy
   targetSdk). Nothing below starts before this is done and re-verified.
1. Diagnose the observed ART `top_shadow_frame_` failure using the retained native-backtrace hook. Validate the new frame interactively on GNOME, then test KDE/Wayland and an X11 desktop path. Android configuration/relayout propagation remains distinct from the implemented compositor scaling.
2. Verify the API-37 `MediaCodec`/Codec2 path with retained logs now that `PlayerService` binds, then implement the required GraphicBuffer allocator/mapper contracts for real video buffers. Exercise NewPipe from launch through keyboard search, deterministic VOD playback, pause/seek, resize, exit, and relaunch. Include hung-guest close and retained logs; do not call the app stable until that flow is covered.
3. Route physical touch/multitouch from the native seat, then connect focus-scoped per-app keymaps to the host controls editor and transparent button outside the guest renderer.
4. Finish guest-exit cleanup and JNI/RefBase lifetime management, then return to ARM64 native execution. The NDK translator still fails during lazy code translation; see [LIMITATIONS.md](LIMITATIONS.md) §2.1.
5. Attempt a real game after the NewPipe/desktop foundation is stable, with Genshin Impact as the later target. It has not been launched or claimed compatible.

The compatibility goal remains open: PenguDroid does not yet run every Android app or game.
