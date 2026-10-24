#!/usr/bin/env python3
"""Write a minimal H.264 baseline stream: SPS + PPS + one IDR frame of I_PCM MBs.

Used by the MediaCodec probe: the soft AVC decoder needs a real elementary
stream to produce a frame, and no H.264 encoder is installed on the host.
Every macroblock is coded as I_PCM, so the bitstream needs no transform, no
prediction and no CABAC — and the sample values are exactly what we wrote,
which makes the decoded frame verifiable byte for byte.

    tools/make_h264.py out.h264 [width_mb] [height_mb]
"""

import sys


class Bits:
    def __init__(self):
        self.bits = []

    def u(self, value, n):
        for i in range(n - 1, -1, -1):
            self.bits.append((value >> i) & 1)

    def ue(self, value):
        v = value + 1
        n = v.bit_length()
        self.u(0, n - 1)
        self.u(v, n)

    def se(self, value):
        self.ue(2 * value - 1 if value > 0 else -2 * value)

    def align(self):
        while len(self.bits) % 8:
            self.bits.append(0)

    def trailing(self):
        self.bits.append(1)
        self.align()

    def bytes(self):
        self.align()
        out = bytearray()
        for i in range(0, len(self.bits), 8):
            b = 0
            for bit in self.bits[i : i + 8]:
                b = (b << 1) | bit
            out.append(b)
        return bytes(out)


def escape(rbsp):
    """Insert emulation-prevention bytes (00 00 03) as the spec requires."""
    out = bytearray()
    zeros = 0
    for b in rbsp:
        if zeros >= 2 and b <= 3:
            out.append(3)
            zeros = 0
        out.append(b)
        zeros = zeros + 1 if b == 0 else 0
    return bytes(out)


def nal(header, rbsp):
    return b"\x00\x00\x00\x01" + bytes([header]) + escape(rbsp)


def sps(mb_w=1, mb_h=1):
    b = Bits()
    b.u(66, 8)  # profile_idc: baseline
    b.u(0, 8)  # constraint flags
    b.u(10, 8)  # level_idc: 1.0
    b.ue(0)  # seq_parameter_set_id
    b.ue(0)  # log2_max_frame_num_minus4
    b.ue(0)  # pic_order_cnt_type
    b.ue(0)  # log2_max_pic_order_cnt_lsb_minus4
    b.ue(1)  # max_num_ref_frames
    b.u(0, 1)  # gaps_in_frame_num_value_allowed_flag
    b.ue(mb_w - 1)  # pic_width_in_mbs_minus1
    b.ue(mb_h - 1)  # pic_height_in_map_units_minus1
    b.u(1, 1)  # frame_mbs_only_flag
    b.u(1, 1)  # direct_8x8_inference_flag
    b.u(0, 1)  # frame_cropping_flag
    b.u(0, 1)  # vui_parameters_present_flag
    b.trailing()
    return nal(0x67, b.bytes())


def pps():
    b = Bits()
    b.ue(0)  # pic_parameter_set_id
    b.ue(0)  # seq_parameter_set_id
    b.u(0, 1)  # entropy_coding_mode_flag: CAVLC
    b.u(0, 1)  # bottom_field_pic_order_in_frame_present_flag
    b.ue(0)  # num_slice_groups_minus1
    b.ue(0)  # num_ref_idx_l0_default_active_minus1
    b.ue(0)  # num_ref_idx_l1_default_active_minus1
    b.u(0, 1)  # weighted_pred_flag
    b.u(0, 2)  # weighted_bipred_idc
    b.se(0)  # pic_init_qp_minus26
    b.se(0)  # pic_init_qs_minus26
    b.se(0)  # chroma_qp_index_offset
    b.u(0, 1)  # deblocking_filter_control_present_flag
    b.u(0, 1)  # constrained_intra_pred_flag
    b.u(0, 1)  # redundant_pic_cnt_present_flag
    b.trailing()
    return nal(0x68, b.bytes())


def idr(y, u, v):
    """One IDR slice whose every macroblock is I_PCM with the given samples.

    The slice header and the slice data are a single continuous bitstream: only
    the PCM samples are byte aligned (`pcm_alignment_zero_bit`), so everything is
    built in one `Bits` and only packed to bytes at the very end.
    """
    b = Bits()
    b.ue(0)  # first_mb_in_slice
    b.ue(7)  # slice_type: I (all slices)
    b.ue(0)  # pic_parameter_set_id
    b.u(0, 4)  # frame_num
    b.ue(0)  # idr_pic_id
    b.u(0, 4)  # pic_order_cnt_lsb
    b.u(0, 1)  # no_output_of_prior_pics_flag
    b.u(0, 1)  # long_term_reference_flag
    b.se(0)  # slice_qp_delta
    for my in range(MB_H):
        for mx in range(MB_W):
            b.ue(25)  # mb_type: I_PCM
            b.align()  # pcm_alignment_zero_bit
            for row in range(16):
                for col in range(16):
                    # A gradient across the frame so a decoded picture is
                    # numerically and visually distinguishable from a zeroed
                    # buffer.
                    b.u(0x40 + ((mx * 10 + col) & 0x3F), 8)
            for _ in range(64):
                b.u(u, 8)
            for _ in range(64):
                b.u(v, 8)
    b.trailing()  # rbsp_slice_trailing_bits
    return nal(0x65, b.bytes())


MB_W = 1
MB_H = 1


def main():
    global MB_W, MB_H
    path = sys.argv[1] if len(sys.argv) > 1 else "/tmp/test.h264"
    MB_W = int(sys.argv[2]) if len(sys.argv) > 2 else 1
    MB_H = int(sys.argv[3]) if len(sys.argv) > 3 else 1
    data = sps(MB_W, MB_H) + pps() + idr(0x40, 0x80, 0xC0)
    with open(path, "wb") as f:
        f.write(data)
    print(f"wrote {len(data)} bytes to {path} ({MB_W * 16}x{MB_H * 16})")


if __name__ == "__main__":
    main()
