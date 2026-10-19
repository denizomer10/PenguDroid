#!/bin/sh
# End-to-end check of the OpenSL ES buffer-queue player in pengudroid-shim.
#
# Compiles app.c against the canonical Khronos/AOSP headers and links it
# directly against libpengudroid_shim.so (no LD_PRELOAD needed here: the shim
# is a normal DT_NEEDED dependency of this test binary). The app enqueues
# silence, so nothing audible is produced.
set -eu
cd "$(dirname "$0")"

ROOT=$(cd ../.. && pwd)

if [ -f "$ROOT/target/debug/libpengudroid_shim.so" ]; then
    TARGET="$ROOT/target/debug"
elif [ -f "$ROOT/target/release/libpengudroid_shim.so" ]; then
    TARGET="$ROOT/target/release"
else
    echo "opensl: libpengudroid_shim.so not found; run: cargo build -p pengudroid-shim" >&2
    exit 1
fi

echo "== build the fixture against $TARGET/libpengudroid_shim.so =="
gcc -O0 -Wall -Wextra -I. app.c -o opensl-app \
    -L"$TARGET" -Wl,--no-as-needed -Wl,-rpath,"$TARGET" \
    -l:libpengudroid_shim.so

echo "== run the app =="
rc=0
out=$(env -u LD_PRELOAD ./opensl-app 2>&1) || rc=$?
printf '%s\n' "$out"

if [ "$rc" -ne 0 ]; then
    echo "RESULT: FAILED (app exit $rc)"
    exit 1
fi

if ! printf '%s\n' "$out" | grep -q "engine realized"; then
    echo "RESULT: FAILED (engine did not realize)"
    exit 1
fi

if ! printf '%s\n' "$out" | grep -q "player PLAYING"; then
    echo "RESULT: FAILED (player did not report PLAYING)"
    exit 1
fi

if ! printf '%s\n' "$out" | grep -Eq "[1-9][0-9]* callbacks"; then
    echo "RESULT: FAILED (no buffer-queue callback fired)"
    exit 1
fi

echo "RESULT: OK (openSL player realized, PLAYING, buffer-queue callbacks fired)"
