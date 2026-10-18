#!/bin/sh
# Build the Android-style demo library. Requires gcc.
#
#   fixtures/native-demo/lib/libandroid_demo.so
#
# The library is deliberately built with -nostdlib and linked only against the
# stub libc.so so that its sole dynamic dependencies are the Bionic symbols it
# imports, versioned under `LIBC` — i.e. it looks like a real Android library.
set -eu
cd "$(dirname "$0")"

OUT=lib
mkdir -p "$OUT"
rm -f "$OUT"/*.so

# 1. Stub libc.so exporting the Bionic symbols under version node LIBC.
gcc -shared -fPIC -fno-stack-protector -nostdlib \
    -Wl,-soname,libc.so -Wl,--version-script=demo_libc.map \
    -o "$OUT/libc.so" demo_libc_stub.c

# 2. The demo library, linked against that stub.
gcc -shared -fPIC -fno-stack-protector -nostdlib \
    -o "$OUT/libandroid_demo.so" demo.c -L"$OUT" -l:libc.so

echo "built $OUT/libandroid_demo.so"
