#!/usr/bin/env python3
"""Find the argument count of an interface method's `onTransact` call site.

    antcall.py <classes.dex> <InterfaceClass> <method>

`IWindow$Stub.onTransact` is a `switch` over `code` that ends each case with
`((ViewRootImpl$W) this).resized(...)` — an `invoke-virtual`/`invoke-virtual-range`
whose method reference is the implementing class's method, not the interface's.
This walks the dex to that call site and prints its argument table, which is the
authoritative answer to "how many arguments does the generated stub send".

It exists because the decompiler shows `new ViewRootImpl$W(0)` and then a plain
`resized(...)`: neither says how many register slots the `invoke` instruction
declares, and the wire format is sized by the *sender*'s table, so the number
decides how the receiving `onTransact` slices the parcel.
"""
import struct
import sys

sys.path.insert(0, __file__.rsplit("/", 1)[0])
from nestargs import Dex, INVOKES  # noqa: E402


def uleb(blob, i):
    r = sh = 0
    while True:
        b = blob[i]
        i += 1
        r |= (b & 0x7F) << sh
        sh += 7
        if not b & 0x80:
            return r, i


def methods_of(blob, cls_off, class_index):
    class_data = struct.unpack_from("<I", blob, cls_off + 32 * class_index + 24)[0]
    if not class_data:
        return []
    i = class_data
    sf, i = uleb(blob, i)
    inf, i = uleb(blob, i)
    dm, i = uleb(blob, i)
    vm, i = uleb(blob, i)
    for _ in range(sf + inf):
        _, i = uleb(blob, i)
        _, i = uleb(blob, i)
    out = []
    for count, kind in ((dm, "direct"), (vm, "virtual")):
        idx = 0
        for _ in range(count):
            diff, i = uleb(blob, i)
            _, i = uleb(blob, i)
            code_off, i = uleb(blob, i)
            idx += diff
            out.append((kind, idx, code_off))
    return out


def call_sites(blob, code_off, target_name):
    size = struct.unpack_from("<I", blob, code_off + 12)[0]
    units = [struct.unpack_from("<H", blob, code_off + 16 + 2 * p)[0] for p in range(size)]
    hits = []
    for n, w in enumerate(units):
        op = w & 0xFF
        if op not in INVOKES or n + 2 >= len(units):
            continue
        name, is_range = INVOKES[op]
        if is_range:
            per = units[n + 1] & 0xFF
            midx = units[n + 2]
            counts = [per]
            slots = None
        else:
            per = (units[n + 1] >> 12) & 0xF
            midx = units[n + 1] & 0xFFFF
            third = units[n + 2]
            counts = [per]
            slots = [(w >> 8) & 0xF, (w >> 12) & 0xF, third & 0xFFFF, third >> 16][:per]
        hits.append((n * 2, name, midx, counts, slots))
    return hits


def main(argv):
    if len(argv) < 4:
        print(__doc__)
        return 2
    dex = Dex(open(argv[1], "rb").read())
    iface, target = argv[2], argv[3]
    stub = iface if iface.endswith("$Stub") else iface + "$Stub"
    blob = dex.blob
    cls_count, cls_off = struct.unpack_from("<II", blob, 0x60)
    on_transact = None
    for c in range(cls_count):
        if dex.type_desc(c) != stub:
            continue
        for kind, midx, code_off in methods_of(blob, cls_off, c):
            if dex.method(midx)[1] == "onTransact" and code_off:
                on_transact = code_off
        break
    if on_transact is None:
        print(f"no onTransact found for {stub}")
        return 1
    print(f"{stub}.onTransact at {on_transact:#x}")
    for offset, name, midx, counts, slots in call_sites(blob, on_transact, target):
        owner, m_name, proto = dex.method(midx)
        if m_name != target:
            continue
        print(f"  @{offset:#x} {name} target={owner} args_table={counts[0]} slots={slots}"
              f" proto={proto}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
