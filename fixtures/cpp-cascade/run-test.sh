#!/bin/sh
# Verify that PenguDroid loads an APK's own library set, resolving dependencies
# *within* the APK (a consumer that DT_NEEDEDs the bundled C++ runtime), and can
# call into the consumer.
set -eu
cd "$(dirname "$0")"

./build.sh >/dev/null

ROOT=$(cd ../.. && pwd)
if [ -x "$ROOT/target/release/pengudroid" ]; then
    BIN="$ROOT/target/release/pengudroid"
else
    BIN="$ROOT/target/debug/pengudroid"
fi

echo "== load the APK's own libraries (intra-APK dependency) =="
out=$("$BIN" nativelib --apk cpp-cascade.apk --abi x86_64 2>&1) || true
echo "$out"
printf '%s\n' "$out" | grep -q "2 loadable, 0 skipped, 0 failed" || {
    echo "RESULT: FAILED (the cascade did not load as a set)"
    exit 1
}

echo
echo "== call the consumer, which needs libc++_shared =="
out2=$("$BIN" nativelib --call consumer_run --lib-dir lib/x86_64 lib/x86_64/libconsumer.so 2>&1) || true
echo "$out2"
printf '%s\n' "$out2" | grep -q "CALL consumer_run() = 42" || {
    echo "RESULT: FAILED (consumer did not resolve the bundled runtime)"
    exit 1
}

echo
echo "RESULT: OK (APK's own libraries load and run together)"
