# Genshin Impact: where the target stands

Genshin Impact is the project's stated end goal. **It has not been launched
under PenguDroid, and nothing in this document claims that it runs.** This is
the gap list between what the tree does today and a playable session, in the
order the game would hit each gap, with what was done about it and what is
still open.

Check any build of the game (or any other title) yourself:

```sh
pengudroid doctor GenshinImpact.apkm
```

`doctor` reads the package — engine, ABI, the system libraries the native code
references, WebView and Play-services use, the launcher activity's orientation —
and prints each requirement as `ok`, `partial` or `MISSING`. It does not
predict whether a gap is fatal; only a run can show that.

## What the game is, technically

| Fact | Consequence here |
|---|---|
| `arm64-v8a` only | every native instruction goes through the Berberis native bridge |
| Unity, IL2CPP (`libunity.so`, `libil2cpp.so`) | `UnityPlayerActivity` renders into a `SurfaceView`; game logic is one very large translated library |
| Landscape-only activity | needs a landscape display |
| Touch-only controls: floating movement pad, camera drag, skill buttons | needs simultaneous touches from keyboard and mouse |
| Native audio engine writing to OpenSL ES | needs a bridged `libOpenSLES.so` |
| Account login, captcha and notices in a `WebView` | needs a hosted WebView provider |
| Downloads tens of GB of game data on first start | needs disk in the per-app prefix and a stable network path |
| Tamper/emulator checks | unknown until it is run |

## Status by requirement

| # | Requirement | State | Notes |
|---|---|---|---|
| 1 | ART + activity lifecycle | works | NewPipe, Beach Buggy Racing 2 |
| 2 | ARM64 translation | works for one game | BBR2 (SDL + FMOD). Not yet a Unity/IL2CPP title |
| 3 | GLES through the bridge | works for one game | `libEGL`/`libGLESv2`/`libGLESv3` guest stubs exist |
| 4 | Landscape display | **implemented, not yet run** | this document's first change, below |
| 5 | Multi-touch keyboard/mouse controls | **implemented, not yet run** | second change, below |
| 6 | Built-in Genshin keymap | **implemented, positions approximate** | third change, below |
| 7 | `libOpenSLES.so` for ARM64 guests | missing | no stub in `tools/build-guest-stubs.py` |
| 8 | Vulkan for ARM64 guests | missing | no stub, no `vkCreateAndroidSurfaceKHR`; the game must use GLES |
| 9 | WebView provider | missing | measured blocker for Roblox, see `STATUS.md` |
| 10 | Mouse-look (pointer lock) | missing | the daemon has no relative-pointer/pointer-constraints path |
| 11 | Performance of `libil2cpp.so` under translation | unknown | |
| 12 | Tamper/emulator detection | unknown | never patch the game to get past it |

"Implemented, not yet run" means: the code is in the tree with unit tests, and
it has not been exercised against a live Android runtime, because the
environment it was written in had no Android 17 system tree, no native bridge
build and no GPU. Treat rows 4–6 as unverified until the checks at the end of
this document pass on a real desktop.

## What changed for this target

### 1. The display follows the activity's orientation

The guest display was a fixed 720x1280 portrait panel, so a landscape game was
laid out, rendered and presented in a portrait window (the BBR2 screenshots in
`docs/screenshots/` are 720x1280 for exactly this reason).

`pengudroid launch` now reads `android:screenOrientation` of the launcher
activity and creates the display accordingly:

| Manifest | Display |
|---|---|
| `landscape`, `sensorLandscape`, `reverseLandscape`, `userLandscape` | 1280x720 |
| anything else, or absent | 720x1280 (unchanged) |

`PENGUDROID_DISPLAY=1920x1080` picks an exact size instead. The launcher exports
the result as `PENGUDROID_SURFACE_W`/`_H`, the pair the shim's `ANativeWindow`,
the gleshim surface and the app host already read; the app host's
`DisplayInfo`, `Configuration` (now including `orientation`), window frame and
the shim's SurfaceFlinger display mode all take their size from it instead of
from constants. Code: `crates/pengudroid-cli/src/display.rs`.

To get the old behaviour for a landscape app: `PENGUDROID_DISPLAY=720x1280`.

### 2. Mapped keys are separate fingers

Every mapped key and the mouse used to share one pointer, so holding a
direction and dragging the camera were two gestures fighting over one finger.
The keymapper now gives each gesture its own pointer id and the input pump
sends real multi-pointer `MotionEvent`s (`ACTION_POINTER_DOWN`/`_UP` with
`PointerProperties[]`/`PointerCoords[]`, source `TOUCHSCREEN`).

A new `stick` action is a virtual joystick: all keys bound to the same centre
and radius drive one finger, which lands on the centre and is dragged toward
the sum of the held directions.

```text
W = stick 198 536 101 up
A = stick 198 536 101 left
S = stick 198 536 101 down
D = stick 198 536 101 right
MOUSE_RIGHT = hold 1107 587
```

Mouse buttons other than the left one (`MOUSE_RIGHT`, `MOUSE_MIDDLE`,
`MOUSE_SIDE`, `MOUSE_EXTRA`) can be bound like keys. The left button stays the
touch itself: dragging with it turns the camera while WASD walks.

The multi-pointer path is used only when a keymap is active. Without one, the
single-pointer path NewPipe was verified on is untouched;
`PENGUDROID_LEGACY_TOUCH=1` forces it even with a keymap.

Code: `crates/pengudroid-cli/src/keymap_gesture.rs` (pure, unit-tested, with a
randomised test that the stream handed to Android is always legal),
`MultiTouchJni` in `apphost.rs`.

### 3. A built-in Genshin keymap

When the user has no profile for `com.miHoYo.GenshinImpact` (or the `Yuanshen`
/ `ys.bilibili` packages), the runtime uses a built-in one scaled to the
display: WASD movement pad, right mouse button attack, `E` skill, `Q` burst,
`Space` jump, `Shift` sprint, `F` interact, `1`–`4` party, `M` map.

```sh
pengudroid keymap-preset com.miHoYo.GenshinImpact 1920x1080 \
  > ~/.config/pengudroid/keymaps/com.miHoYo.GenshinImpact.keymap
```

The positions come from the stock control layout as fractions of the screen;
they were **not** measured in a running game here, and the game lets players
move its buttons. Expect to adjust them.

## What to do next, in order

1. **Verify rows 4–6 on a real desktop** (checks below). BBR2 is a landscape
   game that already runs, so it is the fastest test of the display change.
2. **Run a small ARM64 Unity/IL2CPP game** before Genshin. It isolates "Unity
   under translation" from everything else on this list, with a package that
   starts in seconds instead of after a 30 GB download.
3. **Bridge `libOpenSLES.so`** (and `libaaudio.so`): add the libraries to
   `STUB_LIBRARIES` in `tools/build-guest-stubs.py`, build the matching
   `libberberis_proxy_*` host libraries in `tools/build-nativebridge.py`, and
   point them at the shim's existing OpenSL ES player.
4. **Host a WebView provider**, following the order already recorded in
   `STATUS.md` ("Current Roblox blocker"). This is the largest single item and
   it gates login.
5. **Pointer-lock mouse-look**: `zwp_relative_pointer_v1` +
   `zwp_pointer_constraints_v1` in the daemon, and a `look` keymap action that
   turns relative motion into a re-centring camera drag.
6. Only then launch Genshin, starting from `pengudroid doctor` on the real
   package, and record the first failure in `STATUS.md` the way the BBR2
   bring-up was recorded.

## Checks for the unverified changes

```sh
# Display: a landscape game must report and present 1280x720.
PENGUDROID_NATIVE_BRIDGE_LIB=<bridge dir> pengudroid launch <bbr2.apkm>
#   expect "  display : 1280x720 (the activity is landscape-only)"
#   and a landscape window; PENGUDROID_DUMP_FRAME frames must be 1280x720.

# Portrait apps must be unchanged.
pengudroid launch NewPipe.apk
#   expect "  display : 720x1280 (portrait default)"

# Multi-touch: with a keymap for the package, hold a stick key and drag.
PENGUDROID_TRACE_INPUT=1 pengudroid launch <game>
#   expect "input: mapped touches are multi-pointer" and, while a stick key is
#   held and the left button dragged, "input: touch action=0x5 ..." (or `0x105`) frames
#   listing two pointers.
```

If the 14-argument `MotionEvent.obtain` is rejected by the framework, the log
says `single-pointer (legacy)` instead and mapped keys fall back to one finger
at a time.
