# PenguDroid desktop launcher

The desktop GUI is a workspace member built with Slint and the pinned stable Rust toolchain:

```sh
cargo build --workspace
./target/debug/pengudroid-gui
./target/debug/pengudroid-gui app.apk
```

The library shows application names and cached launcher icons. Select an app to launch, stop, uninstall, or change its own FPS/keymapper preferences in the right pane. APK installation, file selection, and stopping run on workers so the desktop event loop remains responsive.

## Process lifecycle

One shared registry is used by all callbacks. The GUI owns the launcher `Child` handles and checks for exits every 250 ms. Each launch has a separate Linux process group. Stop sends SIGTERM and, after 300 ms, SIGKILL to that group; launcher and Android/audio children are reaped or terminated together. Closing PenguDroid with running apps offers a close-all action.

The daemon also handles guest window close independently of Android responsiveness: it resolves the actual EGL window owner, removes that process's surfaces, sends SIGTERM using a Linux pidfd, and escalates to SIGKILL after 500 ms. The pidfd is captured at surface registration and also detects exits, so crashed apps do not leave frozen windows behind.

## Metadata and settings

- The registry is `$XDG_DATA_HOME/pengudroid/apps.txt`, or `~/.local/share/pengudroid/apps.txt`.
- Per-app preferences are stored in `settings.txt` using `<package>/<setting>` keys. Registry/settings writes use a temporary file and rename.
- Package names and launcher activities come from `pengudroid info --plain`.
- When Android SDK `aapt` and `unzip` are available, installation resolves resource-backed application labels and caches the real launcher PNG under `icons/`. Adaptive icons use a legacy raster variant of the same manifest resource when available. Unsupported artwork uses a visible generic fallback.
- Each application's launcher output is written to `logs/<package>.log`.
- FPS preferences apply on the next launch and are sent to each surface, so they do not affect other apps sharing the daemon. The title counter uses video frames when a separate video layer exists.

## Controls editor and remaining work

Configure controls opens a separate PenguDroid window. It parses and saves the selected app's keymap under `$XDG_CONFIG_HOME/pengudroid/keymaps/<package>.keymap` (or `~/.config/pengudroid/keymaps/`). Keymapper preference persistence and profile editing work, and the saved profile is applied at runtime by the guest's input pump: a bound key becomes a tap, hold or swipe instead of a key press, so touch-only games are playable from the keyboard. The requested transparent control attached to the guest window is still pending. The GUI does not inject its editor into the Android renderer.

The launcher passes the selected app’s keymapper checkbox to the guest on every launch, including its off state. Changes apply on the next launch. CLI launches automatically load an existing profile unless `PENGUDROID_KEYMAPPER=0` is set. The old automatic evdev/uinput mapper startup is no longer invoked by the launcher. Kernel uinput devices are virtual devices; direct physical touch forwarding and complete multitouch support remain open.

Wayland keyboard routing now follows compositor keyboard focus independently of the mouse and releases held keys on focus loss. The Wayland broker now supplies an Adwaita client-side title bar on GNOME/Mutter, with host move/resize/close controls, and prefers compositor framing when available. Viewport resizing scales the guest image and video layers while translating pointer coordinates back to guest pixels; the Android framebuffer/configuration remains fixed. A real GNOME compositor test covers resize and maximize/restore. Interactive desktop coverage, true Android relayout, and an X11 window path remain open. Compatibility across GNOME, KDE, XFCE, and other desktops has not yet been established.

## GUI review snapshot

`PENGUDROID_GUI_SCREENSHOT=/absolute/path/launcher.ppm` captures the launcher window after three seconds. This diagnostic does not capture the rest of the desktop.
