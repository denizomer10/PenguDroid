#!/bin/sh
# Build the Vulkan NativeActivity fixture. Vulkan is loaded at runtime with
# dlopen, so the library links against nothing.
set -eu
cd "$(dirname "$0")"
CC=${CC:-cc}
"$CC" -shared -fPIC -O2 -o libmain.so app.c -ldl
echo "built $(pwd)/libmain.so"
