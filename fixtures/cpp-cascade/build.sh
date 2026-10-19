#!/bin/sh
# Build a two-library cascade that mirrors an APK shipping its own C++ runtime:
#
#   lib/x86_64/libc++_shared.so   (provider)
#   lib/x86_64/libconsumer.so     (DT_NEEDED libc++_shared.so)
#
# and pack them into an APK-shaped zip. No network, no real APK required.
set -eu
cd "$(dirname "$0")"

OUT=lib/x86_64
rm -rf lib
mkdir -p "$OUT"

gcc -shared -fPIC -nostdlib \
    -Wl,-soname,libc++_shared.so \
    -o "$OUT/libc++_shared.so" provider.c

gcc -shared -fPIC -nostdlib \
    -o "$OUT/libconsumer.so" consumer.c \
    -L"$OUT" -l:libc++_shared.so

# Pack the entries exactly as an APK would (paths under lib/<abi>/).
python3 - <<'PY'
import zipfile
with zipfile.ZipFile("cpp-cascade.apk", "w", zipfile.ZIP_STORED) as z:
    z.write("lib/x86_64/libc++_shared.so")
    z.write("lib/x86_64/libconsumer.so")
PY

echo "built cpp-cascade.apk"
