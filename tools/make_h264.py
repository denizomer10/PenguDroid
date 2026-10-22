#!/usr/bin/env python3
"""Write a minimal H.264 baseline stream: SPS + PPS + one IDR I_PCM frame.

Used by the MediaCodec probe: the soft AVC decoder needs a real elementary
stream to produce a frame, and no H.264 encoder is installed on the host.
A single 16x16 macroblock coded as I_PCM keeps the bitstream trivial — no
transform, no prediction, no CABAC.
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


def sps():
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
    b.ue(0)  # pic_width_in_mbs_minus1 -> 16 px
    b.ue(0)  # pic_height_in_map_units_minus1 -> 16 px
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
    b.ue(25)  # mb_type: I_PCM
    b.align()
    rbsp = bytearray(b.bytes())
    rbsp += bytes([y]) * 256
    rbsp += bytes([u]) * 64
    rbsp += bytes([v]) * 64
    rbsp += bytes([0x80])
    return nal(0x65, bytes(rbsp))


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else "/tmp/test.h264"
    data = sps() + pps() + idr(0x80, 0x80, 0x80)
    with open(path, "wb") as f:
        f.write(data)
    print(f"wrote {len(data)} bytes to {path}")


if __name__ == "__main__":
    main()
