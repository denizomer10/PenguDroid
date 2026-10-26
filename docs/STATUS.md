# Project status and handoff

This is the current handoff note. It records verified behavior, known limits, and the next concrete task. See [ARCHITECTURE.md](ARCHITECTURE.md) for implementation details and [LIMITATIONS.md](LIMITATIONS.md) for broader compatibility gaps.

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
implemented code and migration gates. The default app runtime is still API 33;
reporting API 37 without its framework is not a completed migration.

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

- The latest resume workspace build and test run passed: **247 tests, zero failures**.
  Logs: `/tmp/pd-resume/build-final.log` and `/tmp/pd-resume/tests-final.log`.
  Optional hardware fixtures still require the explicit environment described in HARDWARE_VIDEO.md.
- Earlier BLAST and parent-lifecycle changes passed 226 tests. The orientation-sizing work was included in the latest full run.
- `git diff --check` was clean after the last edit.
- Build warnings remain in the existing codebase.

## Next work

1. Diagnose the observed ART `top_shadow_frame_` failure using the retained native-backtrace hook. Validate the new frame interactively on GNOME, then test KDE/Wayland and an X11 desktop path. Android configuration/relayout propagation remains distinct from the implemented compositor scaling.
2. Exercise NewPipe from launch through keyboard search, deterministic VOD playback, pause/seek, resize, exit, and relaunch. Include hung-guest close and retained logs; do not call the app stable until that flow is covered.
3. Route physical touch/multitouch from the native seat, then connect focus-scoped per-app keymaps to the host controls editor and transparent button outside the guest renderer.
4. Finish guest-exit cleanup and JNI/RefBase lifetime management, then return to ARM64 native execution. The NDK translator still fails during lazy code translation; see [LIMITATIONS.md](LIMITATIONS.md) §2.1.
5. Attempt a real game after the NewPipe/desktop foundation is stable, with Genshin Impact as the later target. It has not been launched or claimed compatible.

The compatibility goal remains open: PenguDroid does not yet run every Android app or game.
