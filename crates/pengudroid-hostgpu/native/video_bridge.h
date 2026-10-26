/* PenguDroid host-only video ABI. No FFmpeg/Android layouts cross this boundary. */
#ifndef PENGUDROID_VIDEO_BRIDGE_H
#define PENGUDROID_VIDEO_BRIDGE_H
#include <stdint.h>
#define PD_VIDEO_ABI 1
#define PD_VIDEO_MAX_OBJECTS 4
#define PD_VIDEO_MAX_PLANES 4
#define PD_VIDEO_MAX_HELD 4
struct pd_video_decoder;
struct pd_video_frame;
struct pd_video_object { int32_t fd; uint32_t size; uint64_t modifier; };
struct pd_video_plane { uint32_t object_index, offset, stride; };
struct pd_video_frame_info {
    uint32_t width, height, fourcc, object_count, plane_count;
    int64_t pts_us;
    struct pd_video_object objects[PD_VIDEO_MAX_OBJECTS];
    struct pd_video_plane planes[PD_VIDEO_MAX_PLANES];
};
uint32_t pd_video_abi(void);
/* codec is h264/hevc/vp8/vp9/av1. This backend requires VA-API, never software. */
struct pd_video_decoder *pd_video_open(const char *codec, const char *device,
    const uint8_t *extra, uint32_t extra_size, char *error, uint32_t error_size);
void pd_video_close(struct pd_video_decoder *decoder);
/* 0 accepted, 1 drain frames before retry, negative error. NULL/0 sends EOS. */
int pd_video_send(struct pd_video_decoder *, const uint8_t *, uint32_t, int64_t,
    char *error, uint32_t error_size);
/* 0 frame, 1 needs input, 2 EOS, 3 all frame slots held, negative error. */
int pd_video_receive(struct pd_video_decoder *, struct pd_video_frame **,
    struct pd_video_frame_info *, char *error, uint32_t error_size);
void pd_video_frame_release(struct pd_video_frame *);
void pd_video_flush(struct pd_video_decoder *);
/* Explicit diagnostic readback; the playback/export path never calls this. */
int pd_video_test_luma(struct pd_video_frame *, uint32_t *sample,
    char *error, uint32_t error_size);
#endif
