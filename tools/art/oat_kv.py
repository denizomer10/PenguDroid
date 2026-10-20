#!/usr/bin/env python3
"""Rewrite a single key/value pair in an OAT file's key-value store.

    oat_kv.py <file.oat> <key> <new-value>

ART's `ImageSpace::ValidateApexVersions` compares the `apex-versions` value
recorded in the boot OAT against the runtime's own APEX version string.  The
`boot.oat` shipped inside `system.img` records `/////` (a placeholder used by
the AOSP build), while a runtime that is not installed as an APEX reports the
empty string.  Keeping the key but emptying its value makes the comparison
succeed without changing the OAT checksum path (which is not validated here).

Store layout: at `kVStoreOffset` a little-endian u32 byte size, immediately
followed by a sequence of NUL-terminated `key\\0value\\0` pairs.  The region is
zero-padded to its original length after rewriting so nothing after it moves.
"""
import struct
import sys

# OatHeader is 4096 bytes of header + 32 bytes of fixed fields + 32 bytes of
# hashes; empirically the key-value store size lives at 4160.
KV_OFFSET = 4160


def parse(data):
    size = struct.unpack_from("<I", data, KV_OFFSET)[0]
    region = bytes(data[KV_OFFSET + 4:KV_OFFSET + 4 + size])
    if region and not region.endswith(b"\0"):
        raise SystemExit("key-value store is not NUL terminated")
    parts = region.split(b"\0")[:-1]
    if len(parts) % 2:
        raise SystemExit("key-value store has an odd number of strings")
    pairs = [(parts[i].decode(), parts[i + 1].decode()) for i in range(0, len(parts), 2)]
    return size, pairs


def main(argv):
    if len(argv) != 4:
        raise SystemExit(__doc__)
    path, key, value = argv[1], argv[2], argv[3]
    with open(path, "r+b") as f:
        data = bytearray(f.read())
        size, pairs = parse(data)
        found = False
        for i, (k, _) in enumerate(pairs):
            if k == key:
                pairs[i] = (k, value)
                found = True
        if not found:
            print(f"{path}: key {key!r} not present (nothing to do)")
            return 0
        blob = b"".join(k.encode() + b"\0" + v.encode() + b"\0" for k, v in pairs)
        if len(blob) > size:
            raise SystemExit(f"rewritten store ({len(blob)} B) exceeds original ({size} B)")
        blob += b"\0" * (size - len(blob))
        f.seek(KV_OFFSET)
        f.write(struct.pack("<I", size))
        f.write(blob)
        print(f"{path}: {key} = {value!r} (store {size} B, now {len(blob.rstrip(chr(0).encode()))} B used)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
