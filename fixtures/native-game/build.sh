#!/bin/sh
# Build the playable NativeActivity game fixture.
set -eu
cd "$(dirname "$0")"
CC=${CC:-cc}
"$CC" -shared -fPIC -O2 -o libmain.so app.c -lpthread
echo "built $(pwd)/libmain.so"
