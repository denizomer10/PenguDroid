#!/usr/bin/env python3
"""Convert a captured `IWindowSession.relayoutAsync` request into the
`IWindow.resized` body the window manager would send back.

    relayout2resized.py <request.hex> [--body]

Input is the parcel hex as `PENGUDROID_TRACE_BINDER` prints it (one long line of
space-separated bytes). Nothing here talks to Android: it exists so the wiring
can be derived and checked from real bytes before it is mirrored in the shim.

Why the two calls line up
-------------------------
`ViewRootImpl.relayoutWindow` computes the window frames locally
(`LOCAL_LAYOUT`) and sends `relayoutAsync` when they did not change;
`ViewRootImpl$W.resized` rebuilds `ClientWindowFrames` from the
`WindowManager.LayoutParams` it is given. So the body of the answer is the
`layoutParams` of the request, plus the few values the request does not carry.

Measured request layout (traced NewPipe run)
--------------------------------------------
    0  parcel header: 04 00 00 c2 | strict-mode uid | "SYST" | token length
   12  the token length word (27)  -> token payload follows
   ..  "android.view.IWindowSession" UTF-16LE, NUL, padded to 4
   ..  IWindow: flat_binder_object (4 words) + handle pair (2 words)
   ..  layoutParams: writeTypedObject = [presence=1][len][payload][offset]
   ..  requestedWidth, requestedHeight, viewVisibility, flags, seq, syncSeq
"""
import struct
import sys

BINDER_TYPES = {0x73622A85, 0x73682A85}  # BINDER_TYPE_BINDER / _HANDLE


def u32(data, at):
    if at + 4 > len(data):
        return None
    return struct.unpack_from("<I", data, at)[0]


def string16_at(data, at):
    """End offset of a `string16` at `at`, or None."""
    count = u32(data, at)
    if not count or count > 256:
        return None
    start = at + 4
    payload_end = start + count * 2
    end = (payload_end + 2 + 3) & ~3
    if end > len(data):
        return None
    payload = data[start:payload_end]
    if not all(32 <= b <= 126 for b in payload[0::2]):
        return None
    if any(b != 0 for b in payload[1::2]):
        return None
    if any(b != 0 for b in data[payload_end:end]):
        return None
    if b"." not in payload[0::2]:
        return None
    return end


def token_end(data):
    # The header's last word is the token length, so the token starts there.
    for at in (12, 0, 4, 8, 16, 20):
        end = string16_at(data, at)
        if end is not None:
            return end
    return None


def typed_object_at(data, at):
    """`[presence=1][len][payload][offset]` that fits and looks like params."""
    if u32(data, at) != 1:
        return None
    raw = u32(data, at + 4)
    if raw is None:
        return None
    for length in (raw, raw * 4):
        # `writeToParcel` counts its own words on this tree; `Parcel` stores
        # bytes. Both readings are tried, and the payload must fit.
        if not 40 <= length <= 400:
            continue
        payload_at = at + 8
        end = payload_at + length + 4
        if end > len(data):
            continue
        payload = data[payload_at:payload_at + length]
        # A `WindowManager.LayoutParams` payload is mostly numeric fields: reject
        # blobs that are mostly printable ASCII (an `Intent`/class name, which
        # also has a `1` in front of it after `writeTypedObject`).
        printable = sum(1 for b in payload if 32 <= b <= 126)
        if printable * 2 > len(payload):
            continue
        return length, end
    return None


def layout_params(data):
    start = token_end(data)
    if start is None:
        return None
    at = start
    while at + 8 <= len(data):
        found = typed_object_at(data, at)
        if found:
            length, end = found
            return at, length, end
        at += 4
    return None


def resized_body(data, report_draw=1):
    found = layout_params(data)
    if not found:
        return None
    at, length, frames_end = found
    body = bytearray(data[at:frames_end])
    width, height = u32(data, frames_end) or 0, u32(data, frames_end + 4) or 0
    for word in (report_draw, width, height,
                 0,  # viewVisibility
                 0,  # flags
                 0,  # lastSyncSeqId
                 0,  # null MergedConfiguration
                 0,  # InsetsState.SIZE
                 0,  # forceLayout
                 0,  # alwaysConsumeBars
                 0,  # displayId
                 0,  # syncSeqId
                 0):  # syncInputSeqId
        body += struct.pack("<I", word)
    return bytes(body), width, height


def main(argv):
    if len(argv) < 2:
        print(__doc__)
        return 2
    raw = "".join(open(argv[1]).read().split())
    data = bytes.fromhex(raw)
    print(f"request: {len(data)} bytes")
    end = token_end(data)
    print(f"  token ends at {end}: {data[20:end - 2].decode('utf-16-le')!r}"
          if end else "  no token")
    found = layout_params(data)
    if not found:
        print("  no layoutParams found")
        return 1
    at, length, frames_end = found
    print(f"  layoutParams at {at}, length word {length}, frames end {frames_end}")
    out, width, height = resized_body(data)
    print(f"  resized body: {len(out)} bytes, {width}x{height}")
    if "--body" in argv:
        for i in range(0, len(out), 16):
            row = out[i:i + 16]
            words = struct.unpack_from(f"<{min(4, len(row) // 4)}I", row)
            print(f"   {i:3d} {' '.join(f'{b:02x}' for b in row):<48} {list(words)}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
