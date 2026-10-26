/* SPDX-License-Identifier: Apache-2.0
 * Host decoder bridge, compiled against public FFmpeg/libva headers.
 * https://ffmpeg.org/doxygen/7.1/group__lavc__decoding.html
 * https://intel.github.io/libva/group__api__core.html
 */
#include "video_bridge.h"
#include <libavcodec/avcodec.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_vaapi.h>
#include <libavutil/mem.h>
#include <va/va_drmcommon.h>
#include <stdatomic.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

struct slots { atomic_uint refs, held; };
struct pd_video_decoder { AVCodecContext *codec; struct slots *slots; };
struct pd_video_frame {
    AVFrame *frame; /* Retains the decode surface until the consumer releases it. */
    VADRMPRIMESurfaceDescriptor drm;
    struct slots *slots;
};
static int fail(char *error, uint32_t size, const char *operation, int code) {
    char text[AV_ERROR_MAX_STRING_SIZE];
    av_strerror(code, text, sizeof(text));
    if (error && size) snprintf(error, size, "%s: %s (%d)", operation, text, code);
    return code;
}
static void unref_slots(struct slots *slots) {
    if (atomic_fetch_sub(&slots->refs, 1) == 1) free(slots);
}
uint32_t pd_video_abi(void) { return PD_VIDEO_ABI; }
static enum AVPixelFormat hardware_only(AVCodecContext *context, const enum AVPixelFormat *formats) {
    (void)context;
    for (; *formats != AV_PIX_FMT_NONE; formats++)
        if (*formats == AV_PIX_FMT_VAAPI) return *formats;
    return AV_PIX_FMT_NONE;
}
struct pd_video_decoder *pd_video_open(const char *name, const char *device,
    const uint8_t *extra, uint32_t extra_size, char *error, uint32_t error_size) {
    struct pd_video_decoder *decoder = NULL;
    const AVCodec *codec = NULL;
    int rc = AVERROR(EINVAL), supported = 0;
    const char *operation = "invalid codec/configuration";
    if (!name || !device || extra_size > 1024 * 1024 || (extra_size && !extra)) goto failed;
    const char *allowed[] = {"h264", "hevc", "vp8", "vp9", "av1"};
    for (unsigned i = 0; i < sizeof(allowed) / sizeof(*allowed); i++)
        if (!strcmp(name, allowed[i])) { codec = avcodec_find_decoder_by_name(name); break; }
    if (!codec) goto failed;
    for (int i = 0;; i++) {
        const AVCodecHWConfig *config = avcodec_get_hw_config(codec, i);
        if (!config) break;
        if (config->device_type == AV_HWDEVICE_TYPE_VAAPI &&
            (config->methods & AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX)) supported = 1;
    }
    if (!supported) { operation = "codec has no VA-API decoder"; rc = AVERROR(ENOSYS); goto failed; }
    operation = "allocate decoder"; rc = AVERROR(ENOMEM);
    decoder = calloc(1, sizeof(*decoder));
    if (!decoder) goto failed;
    decoder->slots = calloc(1, sizeof(*decoder->slots));
    if (!decoder->slots) goto failed;
    atomic_init(&decoder->slots->refs, 1);
    atomic_init(&decoder->slots->held, 0);
    decoder->codec = avcodec_alloc_context3(codec);
    if (!decoder->codec) goto failed;
    operation = "create VA-API device";
    rc = av_hwdevice_ctx_create(&decoder->codec->hw_device_ctx, AV_HWDEVICE_TYPE_VAAPI, device, NULL, 0);
    if (rc < 0) goto failed;
    decoder->codec->get_format = hardware_only;
    decoder->codec->pkt_timebase = (AVRational){1, 1000000};
    decoder->codec->thread_count = 1;
    decoder->codec->max_pixels = 8192LL * 8192;
    decoder->codec->extra_hw_frames = PD_VIDEO_MAX_HELD;
    if (extra_size) {
        decoder->codec->extradata = av_mallocz(extra_size + AV_INPUT_BUFFER_PADDING_SIZE);
        if (!decoder->codec->extradata) { rc = AVERROR(ENOMEM); goto failed; }
        memcpy(decoder->codec->extradata, extra, extra_size);
        decoder->codec->extradata_size = (int)extra_size;
    }
    operation = "open hardware decoder";
    rc = avcodec_open2(decoder->codec, codec, NULL);
    if (rc < 0) goto failed;
    return decoder;
failed:
    fail(error, error_size, operation, rc);
    pd_video_close(decoder);
    return NULL;
}
void pd_video_close(struct pd_video_decoder *decoder) {
    if (!decoder) return;
    avcodec_free_context(&decoder->codec);
    if (decoder->slots) unref_slots(decoder->slots);
    free(decoder);
}
int pd_video_send(struct pd_video_decoder *decoder, const uint8_t *bytes,
    uint32_t size, int64_t pts, char *error, uint32_t error_size) {
    if (!decoder || size > 16 * 1024 * 1024 || (size && !bytes))
        return fail(error, error_size, "invalid packet", AVERROR(EINVAL));
    AVPacket *packet = NULL;
    int rc;
    if (bytes && size) {
        packet = av_packet_alloc();
        if (!packet) return fail(error, error_size, "allocate packet", AVERROR(ENOMEM));
        rc = av_new_packet(packet, (int)size);
        if (rc < 0) { av_packet_free(&packet); return fail(error, error_size, "allocate packet data", rc); }
        memcpy(packet->data, bytes, size);
        packet->pts = pts;
        packet->dts = AV_NOPTS_VALUE;
    }
    rc = avcodec_send_packet(decoder->codec, packet);
    av_packet_free(&packet);
    if (rc == AVERROR(EAGAIN)) return 1;
    return rc < 0 ? fail(error, error_size, "send hardware packet", rc) : 0;
}
void pd_video_frame_release(struct pd_video_frame *frame) {
    if (!frame) return;
    for (uint32_t i = 0; i < frame->drm.num_objects && i < PD_VIDEO_MAX_OBJECTS; i++) close(frame->drm.objects[i].fd);
    av_frame_free(&frame->frame);
    if (frame->slots) {
        atomic_fetch_sub(&frame->slots->held, 1);
        unref_slots(frame->slots);
    }
    free(frame);
}
int pd_video_receive(struct pd_video_decoder *decoder, struct pd_video_frame **out,
    struct pd_video_frame_info *info, char *error, uint32_t error_size) {
    if (!decoder || !out || !info) return fail(error, error_size, "invalid output", AVERROR(EINVAL));
    *out = NULL;
    if (atomic_load(&decoder->slots->held) >= PD_VIDEO_MAX_HELD) return 3;
    struct pd_video_frame *frame = calloc(1, sizeof(*frame));
    if (!frame) return fail(error, error_size, "allocate frame", AVERROR(ENOMEM));
    frame->frame = av_frame_alloc();
    if (!frame->frame) { free(frame); return fail(error, error_size, "allocate AVFrame", AVERROR(ENOMEM)); }
    int rc = avcodec_receive_frame(decoder->codec, frame->frame);
    if (rc < 0) {
        pd_video_frame_release(frame);
        if (rc == AVERROR(EAGAIN)) return 1;
        if (rc == AVERROR_EOF) return 2;
        return fail(error, error_size, "receive hardware frame", rc);
    }
    if (frame->frame->format != AV_PIX_FMT_VAAPI || !frame->frame->hw_frames_ctx) {
        rc = AVERROR(ENOSYS); goto failed;
    }
    AVHWFramesContext *frames = (AVHWFramesContext *)frame->frame->hw_frames_ctx->data;
    AVVAAPIDeviceContext *device = (AVVAAPIDeviceContext *)frames->device_ctx->hwctx;
    VASurfaceID surface = (VASurfaceID)(uintptr_t)frame->frame->data[3];
    if (vaSyncSurface(device->display, surface) != VA_STATUS_SUCCESS) {
        rc = AVERROR(EIO); goto failed;
    }
    /* Composed layers retain NV12/P010 and all planes/modifiers. No CPU map. */
    if (vaExportSurfaceHandle(device->display, surface, VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
        VA_EXPORT_SURFACE_READ_ONLY | VA_EXPORT_SURFACE_COMPOSED_LAYERS, &frame->drm) != VA_STATUS_SUCCESS) {
        frame->drm.num_objects = 0; rc = AVERROR(ENOSYS); goto failed;
    }
    if (frame->drm.num_objects > PD_VIDEO_MAX_OBJECTS || frame->drm.num_objects == 0 ||
        frame->drm.num_layers != 1 || frame->drm.layers[0].num_planes > PD_VIDEO_MAX_PLANES ||
        frame->drm.layers[0].num_planes == 0) { rc = AVERROR(ENOSYS); goto failed; }
    memset(info, 0, sizeof(*info));
    info->width = (uint32_t)frame->frame->width;
    info->height = (uint32_t)frame->frame->height;
    info->pts_us = frame->frame->pts;
    info->fourcc = frame->drm.layers[0].drm_format;
    info->object_count = frame->drm.num_objects;
    info->plane_count = frame->drm.layers[0].num_planes;
    for (uint32_t i = 0; i < info->object_count; i++) {
        info->objects[i] = (struct pd_video_object) {frame->drm.objects[i].fd,
            frame->drm.objects[i].size, frame->drm.objects[i].drm_format_modifier};
    }
    for (uint32_t i = 0; i < info->plane_count; i++) {
        if (frame->drm.layers[0].object_index[i] >= info->object_count) { rc = AVERROR(EINVAL); goto failed; }
        info->planes[i] = (struct pd_video_plane) {frame->drm.layers[0].object_index[i],
            frame->drm.layers[0].offset[i], frame->drm.layers[0].pitch[i]};
    }
    frame->slots = decoder->slots;
    atomic_fetch_add(&frame->slots->refs, 1);
    atomic_fetch_add(&frame->slots->held, 1);
    *out = frame;
    return 0;
failed:
    pd_video_frame_release(frame);
    return fail(error, error_size, "export synchronized VA-API frame", rc);
}
void pd_video_flush(struct pd_video_decoder *decoder) {
    if (decoder) avcodec_flush_buffers(decoder->codec);
}
int pd_video_test_luma(struct pd_video_frame *frame, uint32_t *sample,
    char *error, uint32_t error_size) {
    if (!frame || !sample) return fail(error, error_size, "invalid diagnostic frame", AVERROR(EINVAL));
    AVFrame *cpu = av_frame_alloc();
    if (!cpu) return fail(error, error_size, "allocate diagnostic readback", AVERROR(ENOMEM));
    int rc = av_hwframe_transfer_data(cpu, frame->frame, 0);
    if (rc >= 0) *sample = cpu->data[0][0];
    av_frame_free(&cpu);
    return rc < 0 ? fail(error, error_size, "diagnostic GPU readback", rc) : 0;
}
