#!/usr/bin/env python3
"""Pull the UTF-16LE strings out of a hex parcel dumped by the binder trace.

Usage: parcelstr.py <hex...>
"""
import re
import sys


def main():
    hexstr = "".join(sys.argv[1:])
    data = bytes.fromhex(hexstr)
    i = 0
    out = []
    while i + 4 <= len(data):
        n = int.from_bytes(data[i : i + 4], "little", signed=True)
        if 0 < n < 200 and i + 4 + n * 2 <= len(data):
            try:
                s = data[i + 4 : i + 4 + n * 2].decode("utf-16-le")
            except UnicodeDecodeError:
                s = None
            if s is not None and all(31 < ord(c) < 0x3000 for c in s):
                out.append(s)
                i += 4 + n * 2
                continue
        i += 4
    for s in out:
        print(repr(s))


if __name__ == "__main__":
    main()
