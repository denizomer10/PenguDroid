#!/bin/sh
# Build the PenguDroid AppImage: one file that installs, lists, launches, stops
# and removes Android packages.
#
# The image carries the desktop shell plus the `pengudroid` CLI it drives, the
# two host libraries the CLI needs at runtime (`libpengudroid_shim.so` for the
# Bionic surface, `libpengudroid_gleshim.so` for EGL/GLES), an icon and a
# desktop entry. The Android sysroot is far too large to embed, so the AppImage
# finds one the same way the CLI does: PENGUDROID_SYSROOT, the working
# directory's `.toolchain/sysroot-android17`, or the per-user install path.
#
# Usage: tools/build-appimage.sh [--skip-build] [--with-bridge] [--output PATH]
#
#   --skip-build    reuse whatever is already in target/release
#   --with-bridge   also embed the ARM64 translator and its GL proxies from
#                   target/guest-pc-ring/lib, so ARM-only games need no
#                   separately prepared native-bridge directory
#   --output PATH   write the AppImage here (default: target/PenguDroid-x86_64.AppImage)
#   --no-image      assemble the AppDir and stop (no appimagetool needed)
#
# Environment:
#   PENGUDROID_APPIMAGETOOL  appimagetool binary (default: cache, then PATH)
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
TARGET="$ROOT/target"
OUT="$TARGET/PenguDroid-x86_64.AppImage"
SKIP_BUILD=0
WITH_BRIDGE=0
NO_IMAGE=0

while [ $# -gt 0 ]; do
    case "$1" in
        --skip-build) SKIP_BUILD=1 ;;
        --with-bridge) WITH_BRIDGE=1 ;;
        --output) shift; OUT=${1:?--output needs a path} ;;
        --no-image) NO_IMAGE=1 ;;
        *) echo "error: unknown option $1" >&2; exit 2 ;;
    esac
    shift
done

if [ "$SKIP_BUILD" -eq 0 ]; then
    echo "building release binaries"
    cargo build --release --offline \
        -p pengudroid-cli -p pengudroid-gui \
        -p pengudroid-shim -p pengudroid-gleshim
fi

missing=""
for f in pengudroid pengudroid-gui libpengudroid_shim.so libpengudroid_gleshim.so; do
    [ -f "$TARGET/release/$f" ] || missing="$missing $f"
done
if [ -n "$missing" ]; then
    echo "error: missing release artifacts:$missing" >&2
    echo "hint: run without --skip-build" >&2
    exit 1
fi

APPDIR="$TARGET/appimage/PenguDroid.AppDir"
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" \
         "$APPDIR/usr/share/icons/hicolor/256x256/apps" "$APPDIR/usr/share/pengudroid"

install -m 0755 "$TARGET/release/pengudroid-gui" "$APPDIR/usr/bin/pengudroid-gui"
install -m 0755 "$TARGET/release/pengudroid" "$APPDIR/usr/bin/pengudroid"
install -m 0755 "$TARGET/release/libpengudroid_shim.so" "$APPDIR/usr/bin/libpengudroid_shim.so"
install -m 0755 "$TARGET/release/libpengudroid_gleshim.so" "$APPDIR/usr/bin/libpengudroid_gleshim.so"
install -m 0644 "$ROOT/logo.png" "$APPDIR/usr/share/icons/hicolor/256x256/apps/pengudroid.png"
cp "$APPDIR/usr/share/icons/hicolor/256x256/apps/pengudroid.png" "$APPDIR/pengudroid.png"

if [ "$WITH_BRIDGE" -eq 1 ]; then
    bridge_src="$TARGET/guest-pc-ring/lib"
    if [ ! -f "$bridge_src/libberberis_arm64.so" ]; then
        echo "error: --with-bridge needs $bridge_src/libberberis_arm64.so" >&2
        echo "hint: build the translator and proxies first (tools/build-nativebridge.py)" >&2
        exit 1
    fi
    mkdir -p "$APPDIR/usr/lib/pengudroid/bridge"
    # Only the bridge libraries: the directory is de-bionic'd per run, so it must
    # not contain the interpreter archives that also live beside the translator.
    for lib in "$bridge_src"/*.so; do
        install -m 0755 "$lib" "$APPDIR/usr/lib/pengudroid/bridge/"
    done
    echo "embedded $(ls "$APPDIR/usr/lib/pengudroid/bridge" | wc -l) bridge libraries"
fi

cat > "$APPDIR/usr/share/applications/pengudroid.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=PenguDroid
GenericName=Android App Manager
Comment=Install, launch and manage Android apps on Linux
Exec=pengudroid-gui %f
Icon=pengudroid
Terminal=false
Categories=Utility;System;
Keywords=android;apk;emulator;apps;
MimeType=application/vnd.android.package-archive;
StartupWMClass=pengudroid-gui
DESKTOP
install -m 0644 "$APPDIR/usr/share/applications/pengudroid.desktop" "$APPDIR/pengudroid.desktop"

cat > "$APPDIR/AppRun" <<'APPRUN'
#!/bin/sh
# AppRun: make the bundled pieces discoverable, then start the shell.
HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

# The CLI and the shell both look for their host libraries next to the binary.
export PATH="$HERE/usr/bin:$PATH"
export LD_LIBRARY_PATH="$HERE/usr/bin${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

# Bundled ARM64 native bridge, when this image was built --with-bridge. An
# explicit PENGUDROID_NATIVE_BRIDGE_LIB always wins so a user can test a
# differently prepared translator.
if [ -z "${PENGUDROID_NATIVE_BRIDGE_LIB:-}" ] && [ -d "$HERE/usr/lib/pengudroid/bridge" ]; then
    export PENGUDROID_NATIVE_BRIDGE_LIB="$HERE/usr/lib/pengudroid/bridge/libberberis_arm64.so"
fi

# The Android sysroot is not embedded (it is several GB). Point the CLI at a
# bundled copy first, then fall back to the per-user install location.
if [ -z "${PENGUDROID_SYSROOT:-}" ] && [ -d "$HERE/usr/share/pengudroid/sysroot" ]; then
    export PENGUDROID_SYSROOT="$HERE/usr/share/pengudroid/sysroot"
fi
if [ -z "${PENGUDROID_SYSROOT:-}" ] && [ -d "${XDG_DATA_HOME:-$HOME/.local/share}/pengudroid/sysroot" ]; then
    export PENGUDROID_SYSROOT="${XDG_DATA_HOME:-$HOME/.local/share}/pengudroid/sysroot"
fi

exec "$HERE/usr/bin/pengudroid-gui" "$@"
APPRUN
chmod 0755 "$APPDIR/AppRun"

if [ "$NO_IMAGE" -eq 1 ]; then
    echo "assembled $APPDIR"
    exit 0
fi

# appimagetool is not packaged by every distribution; use a cached copy when the
# caller did not pin one.
tool=${PENGUDROID_APPIMAGETOOL:-}
if [ -z "$tool" ]; then
    for candidate in "$HOME/.cache/pengudroid/appimagetool.AppImage" "$(command -v appimagetool || true)"; do
        if [ -n "$candidate" ] && [ -x "$candidate" ]; then tool=$candidate; break; fi
    done
fi
if [ -z "$tool" ]; then
    echo "error: appimagetool not found" >&2
    echo "hint: download appimagetool-x86_64.AppImage to ~/.cache/pengudroid/appimagetool.AppImage" >&2
    echo "      or set PENGUDROID_APPIMAGETOOL to its path" >&2
    exit 1
fi
# AppImages need FUSE to self-mount; extraction is enough to run the tool.
run_tool() {
    case "$tool" in
        *AppImage) "$tool" --appimage-extract-and-run "$@" ;;
        *) "$tool" "$@" ;;
    esac
}

rm -f "$OUT"
ARCH=x86_64 run_tool "$APPDIR" "$OUT"

if [ ! -s "$OUT" ]; then
    echo "error: appimagetool did not produce $OUT" >&2
    exit 1
fi
chmod +x "$OUT"
echo "built $OUT ($(du -h "$OUT" | cut -f1))"
