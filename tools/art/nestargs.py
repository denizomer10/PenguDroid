#!/usr/bin/env python3
"""Count the arguments of a method call in a dex file, from the raw bytecode.

    nestargs.py <classes.dex> <MethodName> [register]

The decompiler in `examples/` drops the parenthesised names it cannot place in
scope, so a line like `new ViewRootImpl$W(0)` tells you *that* a constructor was
called and nothing about how many arguments reached it. This reads the `invoke`
instruction directly: `invoke-direct {p0, p1}, ...-><init>(...)` carries its
argument count in the instruction word, and the encoded method reference gives
the exact prototype.

The output is one line per call site: `dex#offset registers={n} proto(args)`.
`register` (default `p0`) is the receiver expected in the first slot, which is
how a `new X(...)` is told apart from any other `<init>` call.
"""
import struct
import sys


def uleb(data, i):
    result = shift = 0
    while True:
        b = data[i]
        i += 1
        result |= (b & 0x7F) << shift
        shift += 7
        if not b & 0x80:
            return result, i


def decode_mutf8(data):
    # Method names and descriptors are plain ASCII in practice; a full MUTF-8
    # decoder would only matter for identifiers outside the BMP.
    return data.decode("utf-8", "replace")


class Dex:
    def __init__(self, blob):
        self.blob = blob
        (string_ids_size, string_ids_off, type_ids_size, type_ids_off,
         proto_ids_size, proto_ids_off, field_ids_size, field_ids_off,
         method_ids_size, method_ids_off) = struct.unpack_from("<IIIIIIIIII", blob, 0x38)
        self.string_ids = [struct.unpack_from("<I", blob, string_ids_off + 4 * i)[0]
                           for i in range(string_ids_size)]
        self.type_ids = [struct.unpack_from("<I", blob, type_ids_off + 4 * i)[0]
                         for i in range(type_ids_size)]
        self.proto_ids = [struct.unpack_from("<III", blob, proto_ids_off + 12 * i)
                          for i in range(proto_ids_size)]
        self.field_ids = [struct.unpack_from("<HHI", blob, field_ids_off + 8 * i)
                          for i in range(field_ids_size)]
        self.method_ids = [struct.unpack_from("<HHI", blob, method_ids_off + 8 * i)
                           for i in range(method_ids_size)]

    def string(self, idx):
        off = self.string_ids[idx]
        _, off = uleb(self.blob, off)  # utf16 length; MUTF-8 follows
        end = self.blob.index(b"\0", off)
        return decode_mutf8(self.blob[off:end])

    def type_desc(self, idx):
        return self.string(self.type_ids[idx])

    def proto_desc(self, idx):
        _shorty, ret, params_off = self.proto_ids[idx]
        return "(" + self.type_list(params_off) + ")" + self.type_desc(ret)

    def type_list(self, off):
        if off == 0:
            return ""
        n = struct.unpack_from("<I", self.blob, off)[0]
        return "".join(self.type_desc(struct.unpack_from("<H", self.blob, off + 4 + 2 * i)[0])
                       for i in range(n))

    def method(self, idx):
        cls, proto, name = self.method_ids[idx]
        return self.type_desc(cls), self.string(name), self.proto_desc(proto)


# invoke kinds: (opcode, 35c?, args_in_word)
INVOKES = {
    0x6E: ("invoke-virtual", False), 0x6F: ("invoke-super", False),
    0x70: ("invoke-direct", False), 0x71: ("invoke-static", False),
    0x72: ("invoke-interface", False),
    0x74: ("invoke-virtual/range", True), 0x75: ("invoke-super/range", True),
    0x76: ("invoke-direct/range", True), 0x77: ("invoke-static/range", True),
    0x78: ("invoke-interface/range", True),
}


def units(data, start):
    """Yield (offset, code_unit) over the code_item's instruction stream."""
    i = start
    while i + 2 <= len(data):
        yield i, struct.unpack_from("<H", data, i)[0]
        i += 2


def scan(dex, data, method_name, receiver):
    out = []
    words = list(units(data, 0))
    index = {off: n for n, (off, _) in enumerate(words)}
    for off, w in words:
        op = w & 0xFF
        if op in (0x7B,):  # invoke-polymorphic: skip its 4-unit form
            continue
        if op not in INVOKES:
            continue
        name, is_range = INVOKES[op]
        n = index[off]
        # `35c` is `[op][A|G|op][BBBB][F|E|D|C]`, so the argument count is the
        # high nibble of the *second* unit and the register list is spread over
        # units 1 and 3. `3rc` is `[op][AA|op][BBBB][CCCC]`.
        if not is_range:
            if n + 2 >= len(words):
                continue
            regs = (words[n + 1][1] >> 12) & 0xF
            midx = words[n + 1][1] & 0xFFFF
            third = words[n + 2][1]
            slots = [(words[n][1] >> 8) & 0xF, (words[n][1] >> 12) & 0xF,
                     third & 0xFFFF, third >> 16]
            slots = slots[:regs]
        else:
            if n + 2 >= len(words):
                continue
            regs = words[n][1] >> 8
            midx = words[n + 1][1]
            slots = None
        if midx >= len(dex.method_ids):
            continue
        cls, name_m, proto = dex.method(midx)
        if name_m != method_name:
            continue
        out.append({
            "offset": off,
            "op": name,
            "regs": regs,
            "class": cls,
            "proto": proto,
            "slots": slots,
            "receiver_ok": slots is not None and receiver in slots[:1],
        })
    return out


def main(argv):
    if len(argv) < 3:
        print(__doc__)
        return 2
    dex = Dex(open(argv[1], "rb").read())
    name = argv[2]
    receiver = argv[3] if len(argv) > 3 else "p0"
    total = 0
    for off, _ in list(units(dex.blob, 0))[:0]:
        pass
    # Walk class_defs to find each code_item; a raw instruction scan cannot tell
    # methods apart, and the receiver slot is only meaningful inside one.
    cls_count, cls_off = struct.unpack_from("<II", dex.blob, 0x60)
    for c in range(cls_count):
        class_data_off = struct.unpack_from("<I", dex.blob, cls_off + 32 * c + 24)[0]
        if not class_data_off:
            continue
        i = class_data_off
        sf, i = uleb(dex.blob, i)
        inf, i = uleb(dex.blob, i)
        dm, i = uleb(dex.blob, i)
        vm, i = uleb(dex.blob, i)
        for _ in range(sf + inf):
            _, i = uleb(dex.blob, i)
            _, i = uleb(dex.blob, i)  # access_flags
        for count, is_virtual in ((dm, False), (vm, True)):
            midx = 0
            for _ in range(count):
                diff, i = uleb(dex.blob, i)
                flags, i = uleb(dex.blob, i)
                code_off, i = uleb(dex.blob, i)
                midx += diff
                if not code_off:
                    continue
                nregs, _ins, _outs, _tries, _dbg, insns = struct.unpack_from(
                    "<HHHHII", dex.blob, code_off)
                start = code_off + 16
                size = struct.unpack_from("<I", dex.blob, start - 4)[0]
                body = dex.blob[start:start + size * 2]
                hits = scan(dex, body, name, receiver)
                if not hits:
                    continue
                cls_name, m_name, m_proto = dex.method(midx)
                for h in hits:
                    total += 1
                    print(f"{cls_name}->{m_name}{m_proto} @{code_off:#x}+{h['offset']:#x} "
                          f"{h['op']} args={h['regs']} call={h['class']}->{h['proto']}"
                          f" receiver_ok={h['receiver_ok']} slots={h['slots']}")
    print(f"# {total} call site(s)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
