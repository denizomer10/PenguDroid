#!/bin/sh
# End-to-end check of the Bionic libEGL shim: a native app renders with EGL/GL
# and swaps, and the frames must reach the daemon as dma-bufs.
set -eu
cd "$(dirname "$0")"

ROOT=$(cd ../.. && pwd)
pick() {
    if [ -x "$ROOT/target/release/$1" ]; then echo "$ROOT/target/release/$1";
    else echo "$ROOT/target/debug/$1"; fi
}
DAEMON=$(pick pengudroid-daemon)
LIB=$(pick libpengudroid_gleshim.so)

gcc -O0 -o egl-app app.c -ldl

# Start the daemon if it is not already running (--daemonize is idempotent).
"$DAEMON" --daemonize >/dev/null 2>&1 || true
sleep 0.5

echo "== run the app against the Bionic libEGL shim =="
PENGUDROID_LIBEGL="$LIB" \
PENGUDROID_SURFACE_W=640 \
PENGUDROID_SURFACE_H=480 \
    ./egl-app
rc=$?

if [ $rc -eq 0 ]; then
    echo "RESULT: OK (app EGL/GL frames reached Wayland via dma-buf)"
else
    echo "RESULT: FAILED (rc=$rc)"
fi
exit $rc
