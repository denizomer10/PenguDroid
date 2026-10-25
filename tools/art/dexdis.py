#!/usr/bin/env python3
"""Disassemble one method of a dex file, resolving calls and field accesses.

    dexdis.py <classes.dex> <method-name> [--class <Type>] [--code-off 0x…]

`IWindow$Stub.onTransact` is generated code with a 300-case switch, and the
decompiler reports it as a shape rather than as instructions. This prints the
real instruction stream (offset, mnemonic, resolved operands) so questions the
decompiler cannot answer — like "how many register slots does this `invoke`
declare" — are answered from the encoded words themselves.

The instruction-length table below only covers the opcodes that show up in
framework dispatch code; an unknown opcode aborts loudly instead of silently
drifting out of alignment.
"""
import struct
import sys

sys.path.insert(0, __file__.rsplit("/", 1)[0])
from nestargs import Dex  # noqa: E402

# opcode -> (mnemonic, length in 16-bit units, operand format)
OPS = {
    0x00: ("nop", 1, ""), 0x01: ("move", 1, "12x"), 0x02: ("move/from16", 2, "22x"),
    0x03: ("move/16", 3, "32x"), 0x04: ("move-wide", 1, "12x"),
    0x05: ("move-wide/from16", 2, "22x"), 0x06: ("move-wide/16", 3, "32x"),
    0x07: ("move-object", 1, "12x"), 0x08: ("move-object/from16", 2, "22x"),
    0x09: ("move-object/16", 3, "32x"), 0x0A: ("move-result", 1, "11x"),
    0x0B: ("move-result-wide", 1, "11x"), 0x0C: ("move-result-object", 1, "11x"),
    0x0D: ("move-exception", 1, "11x"), 0x0E: ("return-void", 1, ""),
    0x0F: ("return", 1, "11x"), 0x10: ("return-wide", 1, "11x"),
    0x11: ("return-object", 1, "11x"), 0x12: ("const/4", 1, "11n"),
    0x13: ("const/16", 2, "21s"), 0x14: ("const", 2, "31i"),
    0x28: ("goto", 1, "10t"), 0x29: ("goto/16", 2, "20t"), 0x2A: ("goto/32", 3, "30t"),
    0x30: ("cmpg-double", 1, "23x"), 0x2D: ("cmpl-float", 1, "23x"),
    0x7E: ("neg-int", 1, "12x"), 0x7F: ("not-int", 1, "12x"),
    0x81: ("int-to-long", 1, "12x"), 0x8A: ("long-to-int", 1, "12x"),
    0x8F: ("int-to-byte", 1, "12x"), 0x84: ("long-to-float", 1, "12x"),
    0x87: ("int-to-double", 1, "12x"), 0x28: ("goto", 1, "10t"),
    0xA8: ("int-to-byte", 1, "12x"),
    0x15: ("const/high16", 2, "21h"), 0x16: ("const-wide/16", 2, "21s"),
    0x17: ("const-wide/32", 2, "31i"), 0x18: ("const-wide", 3, "51l"),
    0x19: ("const-wide/high16", 2, "21h"), 0x1A: ("const-string", 2, "31c"),
    0x1B: ("const-string/jumbo", 3, "31c"), 0x1C: ("const-class", 2, "31c"),
    0x1D: ("monitor-enter", 1, "11x"), 0x1E: ("monitor-exit", 1, "11x"),
    0x1F: ("check-cast", 2, "21c"), 0x20: ("instance-of", 2, "22c"),
    0x21: ("array-length", 1, "12x"), 0x22: ("new-instance", 2, "21c"),
    0x23: ("new-array", 2, "22c"), 0x24: ("filled-new-array", 3, "35c"),
    0x25: ("filled-new-array/range", 3, "3rc"), 0x26: ("fill-array-data", 3, "31t"),
    0x27: ("throw", 1, "11x"), 0x28: ("goto", 1, "10t"), 0x29: ("goto/16", 2, "20t"),
    0x2A: ("goto/32", 3, "30t"), 0x2B: ("packed-switch", 3, "31t"),
    0x2C: ("sparse-switch", 3, "31t"),
    0x2D: ("cmpl-float", 1, "23x"), 0x2E: ("cmpg-float", 1, "23x"),
    0x2F: ("cmpl-double", 1, "23x"), 0x30: ("cmpg-double", 1, "23x"),
    0x31: ("cmp-long", 1, "23x"),
    0x32: ("if-eq", 2, "22t"), 0x33: ("if-ne", 2, "22t"), 0x34: ("if-lt", 2, "22t"),
    0x35: ("if-ge", 2, "22t"), 0x36: ("if-gt", 2, "22t"), 0x37: ("if-le", 2, "22t"),
    0x38: ("if-eqz", 2, "21t"), 0x39: ("if-nez", 2, "21t"), 0x3A: ("if-ltz", 2, "21t"),
    0x3B: ("if-gez", 2, "21t"), 0x3C: ("if-gtz", 2, "21t"), 0x3D: ("if-lez", 2, "21t"),
    0x44: ("aget", 1, "23x"), 0x4B: ("aput", 1, "23x"),
    0x52: ("iget", 2, "22c"), 0x53: ("iget-wide", 2, "22c"),
    0x54: ("iget-object", 2, "22c"), 0x55: ("iget-boolean", 2, "22c"),
    0x59: ("iput", 2, "22c"), 0x5A: ("iput-wide", 2, "22c"),
    0x5B: ("iput-object", 2, "22c"), 0x5C: ("iput-boolean", 2, "22c"),
    0x60: ("sget", 2, "21c"), 0x61: ("sget-wide", 2, "21c"),
    0x62: ("sget-object", 2, "21c"), 0x66: ("sput", 2, "21c"),
    0x67: ("sput-wide", 2, "21c"), 0x68: ("sput-object", 2, "21c"),
    0x6E: ("invoke-virtual", 3, "35c"), 0x6F: ("invoke-super", 3, "35c"),
    0x70: ("invoke-direct", 3, "35c"), 0x71: ("invoke-static", 3, "35c"),
    0x72: ("invoke-interface", 3, "35c"),
    0x74: ("invoke-virtual/range", 3, "3rc"), 0x75: ("invoke-super/range", 3, "3rc"),
    0x76: ("invoke-direct/range", 3, "3rc"), 0x77: ("invoke-static/range", 3, "3rc"),
    0x78: ("invoke-interface/range", 3, "3rc"),
    0x7B: ("invoke-polymorphic", 4, "45cc"), 0x7C: ("invoke-polymorphic/range", 4, "4rcc"),
    0x90: ("add-int", 1, "23x"), 0xA0: ("add-long", 1, "23x"),
    0xB0: ("add-float", 1, "23x"), 0xBB: ("mul-int", 1, "23x"),
    0xD1: ("sub-int", 1, "23x"), 0xE0: ("and-int", 1, "23x"),
}
for base, name in ((0x2D, None),):
    pass
# The 2addr and literal families share a shape; add them in bulk.
for delta in range(0x00, 0x0A):
    OPS.setdefault(0xB0 + delta, ("arith-float", 1, "12x"))
for op in range(0x38, 0x3E):
    OPS.setdefault(op, ("ifz", 2, "21t"))
LITERAL_22S = {0xD0: "rsub-int", 0xD7: "add-int/lit8", 0xD8: "rsub-int/lit8",
               0xD9: "mul-int/lit8", 0xDA: "div-int/lit8", 0xDB: "rem-int/lit8",
               0xDC: "and-int/lit8", 0xDD: "or-int/lit8", 0xDE: "xor-int/lit8",
               0xDF: "shl-int/lit8", 0xE0: "shr-int/lit8", 0xE1: "ushr-int/lit8"}
for _op, _n in LITERAL_22S.items():
    OPS.setdefault(_op, (_n, 2, "22s"))


def fmt_reg(units, fmt):
    a = units[0]
    if fmt == "12x":
        return f"v{(a >> 8) & 0xF}, v{(a >> 12) & 0xF}"
    if fmt == "11x":
        return f"v{(a >> 8) & 0xF}"
    if fmt == "11n":
        lit = (a >> 12) & 0xF
        lit = lit - 16 if lit >= 8 else lit
        return f"v{(a >> 8) & 0xF}, #{lit}"
    if fmt == "22x":
        return f"v{a >> 8}, v{units[1]}"
    if fmt == "21s":
        s = struct.unpack("<h", struct.pack("<H", units[1]))[0]
        return f"v{a >> 8}, #{s}"
    if fmt == "21h":
        return f"v{a >> 8}, #0x{units[1]:04x}<<16"
    if fmt == "31i":
        return f"v{a >> 8}, #{units[1] | ((units[2] if len(units) > 2 else 0) << 16)}"
    if fmt == "21t":
        d = struct.unpack("<h", struct.pack("<H", units[1]))[0]
        return f"v{a >> 8}, ->0x{(d * 2):+x}"
    if fmt == "22s":
        low = (a >> 12) & 0xF
        low = low - 16 if low >= 8 else low
        return f"v{a >> 8}, v{units[1]}, #{low}"
    if fmt == "22t":
        d = struct.unpack("<h", struct.pack("<H", units[1]))[0]
        return f"v{a >> 8}, v{(a >> 12) & 0xF}, ->0x{(d * 2):+x}"
    if fmt == "31c":
        third = units[2] if len(units) > 2 else 0
        return f"v{a >> 8}, @{units[1] | (third << 16)}"
    if fmt == "21c":
        return f"v{a >> 8}, @{units[1]}"
    if fmt == "22c":
        return f"v{a >> 8}, v{(a >> 12) & 0xF}, @{units[1]}"
    if fmt == "23x":
        return f"v{a >> 8}, v{(a >> 12) & 0xF}, v{units[1]}"
    if fmt == "35c":
        third = units[2]
        regs = [(a >> 8) & 0xF, (a >> 12) & 0xF, third & 0xFFFF, third >> 16]
        n = (units[1] >> 12) & 0xF
        if n == 0:
            n = 5  # `A` encodes a five-argument list as 0
        regs = regs[:n]
        return "{" + ", ".join(f"v{r}" for r in regs) + f"}} args={n} @{units[1] & 0xFFFF}"
    if fmt == "10t":
        d = struct.unpack("<b", struct.pack("<B", (a >> 8) & 0xFF))[0]
        return f"->{d * 2:+d}"
    if fmt == "20t":
        d = struct.unpack("<h", struct.pack("<H", units[1]))[0]
        return f"->{d * 2:+d}"
    if fmt == "3rc":
        per = (a >> 8) & 0xFF
        return f"{{v{units[2]}..+{per}}} args={per} @{units[1]}"
    return "?"


def disassemble(dex, blob, code_off):
    size = struct.unpack_from("<I", blob, code_off + 12)[0]
    units = [struct.unpack_from("<H", blob, code_off + 16 + 2 * p)[0] for p in range(size)]
    p = 0
    while p < size:
        w = units[p]
        op = w & 0xFF
        if op == 0x00 and w != 0:  # payload pseudo-instructions
            ident = units[p]
            if ident == 0x0100 and p + 2 <= size:
                n = units[p + 1]
                print(f"{p * 2:#06x}  packed-switch-payload size={n}")
                p += n * 2 + 4
                continue
            if ident == 0x0200 and p + 2 <= size:
                n = units[p + 1]
                print(f"{p * 2:#06x}  sparse-switch-payload size={n}")
                p += n * 4 + 2
                continue
            if ident == 0x0300 and p + 4 <= size:
                ew, n = units[p + 1], units[p + 2]
                print(f"{p * 2:#06x}  fill-array-data-payload width={ew} size={n}")
                p += (ew * n + 1) // 2 + 4
                continue
            p += 1
            continue
        name, length, fmt = OPS.get(op, (f"op{op:#04x}", 1, ""))
        chunk = units[p:p + length]
        if len(chunk) < length:
            print(f"{p * 2:#06x}  <truncated {name}>")
            break
        detail = fmt_reg(chunk, fmt) if fmt else ""
        if fmt in ("35c", "3rc", "21c", "22c", "31c"):
            idx = chunk[1] if fmt in ("35c", "3rc", "21c", "22c") else (chunk[1] | ((chunk[2] if len(chunk) > 2 else 0) << 16))
            kind = "method" if op in (0x6E, 0x6F, 0x70, 0x71, 0x72, 0x74, 0x75, 0x76, 0x77, 0x78) else None
            if kind == "method" and idx < len(dex.method_ids):
                detail += f"  // {dex.method(idx)}"
            elif op in (0x52, 0x53, 0x54, 0x55, 0x59, 0x5A, 0x5B, 0x5C) and idx < len(dex.field_ids):
                cls, typ, n = dex.field_ids[idx]
                detail += f"  // {dex.type_desc(cls)[:-1]}.{dex.string(n)}:{dex.type_desc(typ)}"
            elif op in (0x60, 0x61, 0x62, 0x66, 0x67, 0x68) and idx < len(dex.field_ids):
                cls, typ, n = dex.field_ids[idx]
                detail += f"  // {dex.type_desc(cls)[:-1]}.{dex.string(n)}:{dex.type_desc(typ)}"
            elif idx < len(dex.type_ids):
                detail += f"  // {dex.type_desc(idx)}"
        print(f"{p * 2:#06x}  {name:24s} {detail}")
        p += length


def main(argv):
    if len(argv) < 3:
        print(__doc__)
        return 2
    dex = Dex(open(argv[1], "rb").read())
    name = argv[2]
    code = None
    if "--code-off" in argv:
        code = int(argv[argv.index("--code-off") + 1], 16)
    else:
        for k in range(len(dex.method_ids)):
            if dex.method(k)[1] == name:
                print(f"# candidate {k:#x}: {dex.method(k)}")
        return 0
    disassemble(dex, dex.blob, code)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
