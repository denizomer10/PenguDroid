# pengudroid-gui

PenguDroid's desktop shell: install an APK, launch it, stop it, remove it, and
toggle the in-game helpers (keymapper, FPS meter). Written in Rust with Slint.

## Status: written, not yet buildable on this toolchain

The crate is **not** a workspace member yet, because Slint's dependency tree
cannot be built with the toolchain in `.toolchain/`:

```
rustc 1.85.0
slint 1.6/1.8 → accesskit/zbus → "requires rustc 1.87"
             → icu_collections/icu_locale_core → uses `is_multiple_of` (1.87)
```

`--ignore-rust-version` gets past the version check and then fails inside
`icu_collections`, so the requirement is real, not just declared.

Two ways to unblock it, both small:

1. **Newer toolchain** (recommended):
   ```sh
   rustup toolchain install 1.87.0
   rustup override set 1.87.0
   ```
   then add `"crates/pengudroid-gui"` back to the workspace `members` list.
2. **Pin the transitive chain** to releases that predate the 1.87 requirement
   (`zbus`, `zvariant`, `icu_*`), which is more fragile and needs re-pinning
   whenever the tree is refreshed.

## What it does

* Left sidebar: installed apps, a `+` button, and a hint that dropping an `.apk`
  on the window installs it. `pengudroid-gui game.apk` installs from the command
  line too (and works as a file manager's "open with").
* Detail pane: **Launch**, **Stop**, **Uninstall**, plus the keymapper and FPS
  meter toggles.
* Closing the window while an app is running asks first ("Quit all"), and only
  then stops every instance.
* The logo is the repository's `logo.png`.

## Design notes

* The GUI does not reimplement launching: it runs `pengudroid launch`, so the
  CLI and the GUI cannot drift. `runner::sysroot()` finds the Android tree from
  `PENGUDROID_SYSROOT`, the in-repo `.toolchain/sysroot`, or the per-user
  install location.
* The registry is a plain text file (`$XDG_DATA_HOME/pengudroid/apps.txt`,
  `package|label|activity|apk`), so it can be inspected and edited by hand.
  Running state is never persisted — a pid from a previous session is
  meaningless and a stale "running" flag would make the quit prompt lie.
* `install()` asks `pengudroid info` for the package and launcher activity
  instead of parsing the manifest a second time.
* The software renderer is deliberate: the UI is flat rectangles, and it works
  over a remote session where there may be no GL context.
