#!/bin/sh
# Build the optional host decoder plugin using distribution development headers.
# Requires pkg-config, libavcodec-dev, libavutil-dev and libva-dev (or equivalents).
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
out=${1:-"$root/target/debug/libpengudroid_video.so"}
mkdir -p "$(dirname -- "$out")"
video_cflags=$(pkg-config --cflags libavcodec libavutil libva)
video_libs=$(pkg-config --libs libavcodec libavutil libva)
# Additional flags allow a locally extracted SDK without modifying the system.
# Deliberate word splitting: compiler/pkg-config flag lists.
${CC:-cc} -std=c11 -O2 -fPIC -shared -Wall -Wextra -Werror \
    ${CPPFLAGS:-} $video_cflags \
    "$root/crates/pengudroid-hostgpu/native/video_bridge.c" \
    ${LDFLAGS:-} $video_libs \
    -Wl,-z,defs -Wl,-z,relro,-z,now -o "$out"
printf 'Built %s\n' "$out"
