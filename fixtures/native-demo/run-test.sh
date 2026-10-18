#!/bin/sh
# End-to-end check for the native-loading pipeline: build the Android-style
# demo library, then load and call it through PenguDroid.
set -eu
cd "$(dirname "$0")"

./build.sh >/dev/null

ROOT=$(cd ../.. && pwd)
if [ -x "$ROOT/target/release/pengudroid" ]; then
    BIN="$ROOT/target/release/pengudroid"
else
    BIN="$ROOT/target/debug/pengudroid"
fi

echo "== the artifact is Android-style versioned =="
readelf -V lib/libandroid_demo.so | grep -E "File: libc.so|Name: LIBC" || true
readelf -d lib/libandroid_demo.so | grep NEEDED || true

echo
echo "== load + call via pengudroid =="
out=$("$BIN" nativelib lib/libandroid_demo.so --call pengudroid_demo_run 2>&1) || true
echo "$out"

echo
if printf '%s' "$out" | grep -q "500500"; then
    echo "RESULT: OK (pengudroid_demo_run returned 500500)"
else
    echo "RESULT: FAILED (did not observe 500500)"
    exit 1
fi
