#!/bin/sh
# Build the ARM64 guest fixture for the native-bridge boundary.
#
# This cross-compiles against the supplied API-37 AArch64 sysroot and checks the
# result is really AArch64. It does not run anything: the fixture is executed by
# a translator, not by this host.
#
# Usage: tools/build-arm64-bridge-fixture.sh [OUTPUT_DIR]
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
OUT=${1:-"$ROOT/target/arm64-fixture"}

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
SRC="$ROOT/fixtures/arm64-bridge/fixture.c"
OBJ="$OUT/fixture.o"
SO="$OUT/libpengudroid_arm64_fixture.so"

"$CLANG" \
    --target=aarch64-linux-android37 \
    --sysroot="$SYSROOT" \
    -isystem "$SYSROOT/usr/include/aarch64-linux-android" \
    -fPIC -O2 -Wall -Wextra -Werror \
    -c "$SRC" -o "$OBJ"

"$LLD" \
    -m aarch64linux -shared --no-undefined -z max-page-size=16384 \
    "$OBJ" "$LIBDIR/crtbegin_so.o" \
    -L"$LIBDIR" -lc -llog \
    "$LIBDIR/crtend_so.o" \
    -o "$SO"

# Fail loudly rather than leave a plausible-looking x86 artifact behind.
machine=$(readelf -h "$SO" | awk '/Machine:/ { print $2 }')
if [ "$machine" != "AArch64" ]; then
    echo "error: $SO is $machine, not AArch64" >&2
    exit 1
fi
# The fixture is guest code: its NEEDED entries are platform libraries the
# guest linker resolves, not something this host loads. Anything else would
# mean the link pulled in a host or helper library by mistake.
unexpected=$(readelf -d "$SO" | awk '/NEEDED/ { gsub(/[][]/, "", $NF); print $NF }' \
    | grep -vE '^(libc|liblog|libm|libdl)\.so$' || true)
if [ -n "$unexpected" ]; then
    echo "error: unexpected NEEDED entries in $SO: $unexpected" >&2
    exit 1
fi
for symbol in pengudroid_fixture_answer pengudroid_fixture_answer_with_log; do
    if ! nm -D --defined-only "$SO" | grep -q " $symbol\$"; then
        echo "error: $symbol is not exported from $SO" >&2
        exit 1
    fi
done

echo "built $SO"
echo "  machine: $machine"
readelf -d "$SO" | grep -E 'NEEDED|SONAME' || echo "  NEEDED: none"
