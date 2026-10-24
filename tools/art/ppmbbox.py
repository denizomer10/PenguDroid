#!/usr/bin/env python3
"""Report the bounding box of the non-background region of a PPM.

Usage: ppmbbox.py <file> [row]
"""
import sys


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
    i += 1
    _magic, w, h, _ = fields
    return int(w), int(h), data[i:]


def main():
    path = sys.argv[1]
    w, h, px = read_ppm(path)

    def pixel(x, y):
        i = (y * w + x) * 3
        return px[i], px[i + 1], px[i + 2]

    # Background is whatever the corners are.
    bg = pixel(1, 1)
    print(f"{path}: {w}x{h} corner={bg}")

    # Horizontal extent of the "not background" region, sampled across rows.
    lefts, rights = [], []
    for y in range(0, h, max(1, h // 40)):
        row = [x for x in range(w) if pixel(x, y) != bg]
        if row:
            lefts.append(row[0])
            rights.append(row[-1])
    if lefts:
        print(f"  content x: {min(lefts)}..{max(rights)}")
    # Vertical extent.
    tops, bots = [], []
    for x in range(0, w, max(1, w // 40)):
        col = [y for y in range(h) if pixel(x, y) != bg]
        if col:
            tops.append(col[0])
            bots.append(col[-1])
    if tops:
        print(f"  content y: {min(tops)}..{max(bots)}")

    for y in (int(sys.argv[2]) if len(sys.argv) > 2 else h // 3,):
        row = [x for x in range(w) if pixel(x, y) != bg]
        if row:
            print(f"  row {y}: x {row[0]}..{row[-1]}  ({len(row)} px non-bg)")


if __name__ == "__main__":
    main()
