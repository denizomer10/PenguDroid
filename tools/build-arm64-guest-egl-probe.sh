#!/bin/sh
# Build the ARM64 guest EGL probe.
#
# The probe is ordinary Android ARM64 code that links the platform libEGL and
# libGLESv2, exactly like the `libmain.so` of a translated game. Running it under
# the translator therefore exercises the whole guest GL chain:
#
#   guest libEGL.so stub -> libberberis_proxy_libEGL.so -> host libEGL.so
#   forwarder -> libpengudroid_gleshim.so -> host GPU
#
# Unlike tools/build-arm64-bridge-fixture.sh, NEEDED libEGL.so/libGLESv2.so are
# expected here: they are the point of the exercise.
#
# Usage: tools/build-arm64-guest-egl-probe.sh [OUTPUT_DIR]
#
# Environment:
#   PENGUDROID_NDK_SYSROOT  API-37 sysroot (default: the supplied r30 tree)
#   PENGUDROID_CLANG        clang with an AArch64 backend
#   PENGUDROID_LLD          ld.lld
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
SYSROOT=${PENGUDROID_NDK_SYSROOT:-"$ROOT/.toolchain/ndk-refs_tags_ndk-r30-platform-sysroot"}
CLANG=${PENGUDROID_CLANG:-/usr/lib/llvm-22/bin/clang}
LLD=${PENGUDROID_LLD:-/usr/lib/llvm-22/bin/ld.lld}
OUT=${1:-"$ROOT/target/arm64-egl-probe"}

LIBDIR="$SYSROOT/usr/lib/aarch64-linux-android/37"
if [ ! -d "$LIBDIR" ]; then
    echo "error: $LIBDIR is missing; this build requires the API-37 AArch64 sysroot" >&2
    echo "hint: set PENGUDROID_NDK_SYSROOT to an NDK platform sysroot containing usr/lib/aarch64-linux-android/37" >&2
    exit 1
fi
for tool in "$CLANG" "$LLD"; do
    if [ ! -x "$tool" ]; then
        echo "error: $tool is not executable" >&2
        exit 1
    fi
done

mkdir -p "$OUT"
SRC="$ROOT/fixtures/egl-app/guest_egl.c"
OBJ="$OUT/guest_egl.o"
SO="$OUT/libpengudroid_egl_probe.so"

"$CLANG" \
    --target=aarch64-linux-android37 \
    --sysroot="$SYSROOT" \
    -isystem "$SYSROOT/usr/include/aarch64-linux-android" \
    -fPIC -O2 -Wall -Wextra -Werror \
    -c "$SRC" -o "$OBJ"

"$LLD" \
    -m aarch64linux -shared --no-undefined -z max-page-size=16384 \
    "$OBJ" "$LIBDIR/crtbegin_so.o" \
    -L"$LIBDIR" -lEGL -lGLESv2 -lc -llog \
    "$LIBDIR/crtend_so.o" \
    -o "$SO"

machine=$(readelf -h "$SO" | awk '/Machine:/ { print $2 }')
if [ "$machine" != "AArch64" ]; then
    echo "error: $SO is $machine, not AArch64" >&2
    exit 1
fi

needed=$(readelf -d "$SO" | awk '/NEEDED/ { gsub(/[][]/, "", $NF); print $NF }')
for required in libEGL.so libGLESv2.so; do
    if ! printf '%s\n' "$needed" | grep -qx "$required"; then
        echo "error: $SO does not depend on $required; it would not exercise the guest GL stub" >&2
        echo "needed: $(printf '%s ' $needed)" >&2
        exit 1
    fi
done
unexpected=$(printf '%s\n' "$needed" | grep -vE '^(libEGL|libGLESv2|libc|liblog|libm|libdl)\.so$' || true)
if [ -n "$unexpected" ]; then
    echo "error: unexpected NEEDED entries in $SO: $unexpected" >&2
    exit 1
fi

if ! nm -D --defined-only "$SO" | grep -q ' pengudroid_egl_probe$'; then
    echo "error: pengudroid_egl_probe is not exported from $SO" >&2
    exit 1
fi

echo "built $SO"
echo "  machine: $machine"
echo "  needed:  $(printf '%s ' $needed)"
