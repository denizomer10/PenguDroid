#!/usr/bin/env python3
"""Summarise a binary PPM (P6) frame: size, mean colour, distinct colours.

Usage: ppmstat.py <file> [more...]
"""
import sys
from collections import Counter


def read_ppm(path):
    data = open(path, "rb").read()
    fields = []
    i = 0
    while len(fields) < 4:
        while i < len(data) and data[i : i + 1].isspace():
            i += 1
        if data[i : i + 1] == b"#":
            while i < len(data) and data[i] != 0x0A:
                i += 1
            continue
        start = i
        while i < len(data) and not data[i : i + 1].isspace():
            i += 1
        fields.append(data[start:i])
    i += 1  # single whitespace after maxval
    magic, w, h, maxval = fields
    assert magic == b"P6", magic
    return int(w), int(h), data[i:]


def main():
    for path in sys.argv[1:]:
        w, h, pixels = read_ppm(path)
        n = w * h
        if len(pixels) < n * 3:
            print(f"{path}: truncated ({len(pixels)} bytes, want {n * 3})")
            continue
        rs = pixels[0::3]
        gs = pixels[1::3]
        bs = pixels[2::3]
        mean = (sum(rs) / n, sum(gs) / n, sum(bs) / n)
        counts = Counter(zip(pixels[0::3], pixels[1::3], pixels[2::3]))
        top = counts.most_common(5)
        print(f"{path}: {w}x{h}")
        print(f"  mean rgb   = ({mean[0]:.1f}, {mean[1]:.1f}, {mean[2]:.1f})")
        print(f"  distinct   = {len(counts)}")
        print(f"  min/max r  = {min(rs)}/{max(rs)}  g {min(gs)}/{max(gs)}  b {min(bs)}/{max(bs)}")
        print(f"  top colours= {[('#%02x%02x%02x' % c, k) for c, k in top]}")


if __name__ == "__main__":
    main()
