#!/bin/sh
# Build the NativeActivity fixture. It links against nothing: every Android
# symbol is resolved at load time from PenguDroid's preloaded shim.
set -eu
cd "$(dirname "$0")"
CC=${CC:-cc}
"$CC" -shared -fPIC -O2 -o libmain.so app.c -lpthread
echo "built $(pwd)/libmain.so"
