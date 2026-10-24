/* FFmpeg VAAPI ABI boundary. FFmpeg/VAAPI objects are encoder-worker owned.
 * Input NV12 is copied into a CPU frame and transferred to a VAAPI surface;
 * the surface is retained by FFmpeg after avcodec_send_frame returns. */
#include <errno.h>
#include <stdint.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>

#include <libavcodec/avcodec.h>
#include <libavutil/error.h>
#include <libavutil/frame.h>
#include <libavutil/hwcontext.h>
#include <libavutil/imgutils.h>
#include <libavutil/opt.h>
#include <libavutil/pixfmt.h>

#define AMATERASU_VAAPI_MAX_PACKET (16 * 1024 * 1024)

enum {
    AMATERASU_VAAPI_ERROR = -1,
    AMATERASU_VAAPI_AGAIN = 0,
    AMATERASU_VAAPI_PACKET = 1,
    AMATERASU_VAAPI_EOF = 2,
    AMATERASU_VAAPI_RETRY = 3,
};

struct amaterasu_ffmpeg_vaapi {
    AVCodecContext *context;
    AVBufferRef *device_ref;
    AVBufferRef *frames_ref;
    AVFrame *software_frame;
    AVPacket *packet;
    int width;
    int height;
    int draining;
};

static void amaterasu_vaapi_destroy(struct amaterasu_ffmpeg_vaapi *encoder)
{
    if (encoder == NULL)
        return;
    av_packet_free(&encoder->packet);
    av_frame_free(&encoder->software_frame);
    avcodec_free_context(&encoder->context);
    av_buffer_unref(&encoder->frames_ref);
    av_buffer_unref(&encoder->device_ref);
    free(encoder);
}

static struct amaterasu_ffmpeg_vaapi *amaterasu_vaapi_open(
    int width, int height, int fps, int bitrate, int *error_code)
{
    if (error_code != NULL)
        *error_code = 1;
    if (width < 16 || height < 16 || (width & 1) != 0 || (height & 1) != 0 ||
        fps < 1 || fps > 120 || bitrate < 100000 || bitrate > 100000000)
        return NULL;

    const AVCodec *codec = avcodec_find_encoder_by_name("h264_vaapi");
    if (codec == NULL || codec->id != AV_CODEC_ID_H264) {
        if (error_code != NULL)
            *error_code = 2;
        return NULL;
    }

    struct amaterasu_ffmpeg_vaapi *encoder =
        (struct amaterasu_ffmpeg_vaapi *)calloc(1, sizeof(*encoder));
    if (encoder == NULL) {
        if (error_code != NULL)
            *error_code = 3;
        return NULL;
    }

    if (av_hwdevice_ctx_create(&encoder->device_ref, AV_HWDEVICE_TYPE_VAAPI,
                               NULL, NULL, 0) < 0) {
        if (error_code != NULL)
            *error_code = 2;
        goto fail;
    }

    encoder->frames_ref = av_hwframe_ctx_alloc(encoder->device_ref);
    if (encoder->frames_ref == NULL) {
        if (error_code != NULL)
            *error_code = 3;
        goto fail;
    }
    AVHWFramesContext *frames = (AVHWFramesContext *)encoder->frames_ref->data;
    frames->format = AV_PIX_FMT_VAAPI;
    frames->sw_format = AV_PIX_FMT_NV12;
    frames->width = width;
    frames->height = height;
    /* Fixed bound accommodates encoder references plus a small async queue. */
    frames->initial_pool_size = 16;
    if (av_hwframe_ctx_init(encoder->frames_ref) < 0) {
        if (error_code != NULL)
            *error_code = 2;
        goto fail;
    }

    encoder->context = avcodec_alloc_context3(codec);
    encoder->software_frame = av_frame_alloc();
    encoder->packet = av_packet_alloc();
    if (encoder->context == NULL || encoder->software_frame == NULL || encoder->packet == NULL) {
        if (error_code != NULL)
            *error_code = 3;
        goto fail;
    }

    AVCodecContext *context = encoder->context;
    context->width = width;
    context->height = height;
    context->pix_fmt = AV_PIX_FMT_VAAPI;
    context->time_base = (AVRational){ 1, 10000000 };
    context->framerate = (AVRational){ fps, 1 };
    context->bit_rate = bitrate;
    context->rc_max_rate = bitrate;
    context->rc_buffer_size = bitrate / fps > 0 ? bitrate / fps : 1;
    context->gop_size = fps * 2;
    context->max_b_frames = 0;
    context->profile = AV_PROFILE_H264_CONSTRAINED_BASELINE;
    context->thread_count = 1;
    context->flags |= AV_CODEC_FLAG_LOW_DELAY | AV_CODEC_FLAG_GLOBAL_HEADER;
    context->color_range = AVCOL_RANGE_MPEG;
    context->color_primaries = AVCOL_PRI_BT709;
    context->color_trc = AVCOL_TRC_BT709;
    context->colorspace = AVCOL_SPC_BT709;
    context->hw_device_ctx = av_buffer_ref(encoder->device_ref);
    context->hw_frames_ctx = av_buffer_ref(encoder->frames_ref);
    if (context->hw_device_ctx == NULL || context->hw_frames_ctx == NULL) {
        if (error_code != NULL)
            *error_code = 3;
        goto fail;
    }

    AVDictionary *options = NULL;
    int option_status = 0;
    option_status |= av_dict_set(&options, "profile", "constrained_baseline", 0);
    option_status |= av_dict_set(&options, "async_depth", "1", 0);
    option_status |= av_dict_set(&options, "rc_mode", "CBR", 0);
    option_status |= av_dict_set(&options, "coder", "cavlc", 0);
    option_status |= av_dict_set(&options, "idr_interval", "1", 0);
    if (option_status < 0 || avcodec_open2(context, codec, &options) < 0 || options != NULL) {
        av_dict_free(&options);
        if (error_code != NULL)
            *error_code = 4;
        goto fail;
    }
    av_dict_free(&options);

    if (context->pix_fmt != AV_PIX_FMT_VAAPI || context->max_b_frames != 0 ||
        context->profile != AV_PROFILE_H264_CONSTRAINED_BASELINE ||
        context->extradata == NULL || context->extradata_size <= 0 ||
        context->extradata_size > 1024 * 1024 ||
        context->color_range != AVCOL_RANGE_MPEG ||
        context->color_primaries != AVCOL_PRI_BT709 ||
        context->color_trc != AVCOL_TRC_BT709 || context->colorspace != AVCOL_SPC_BT709) {
        if (error_code != NULL)
            *error_code = 5;
        goto fail;
    }

    encoder->software_frame->format = AV_PIX_FMT_NV12;
    encoder->software_frame->width = width;
    encoder->software_frame->height = height;
    encoder->software_frame->color_range = AVCOL_RANGE_MPEG;
    encoder->software_frame->color_primaries = AVCOL_PRI_BT709;
    encoder->software_frame->color_trc = AVCOL_TRC_BT709;
    encoder->software_frame->colorspace = AVCOL_SPC_BT709;
    if (av_frame_get_buffer(encoder->software_frame, 32) < 0) {
        if (error_code != NULL)
            *error_code = 3;
        goto fail;
    }

    encoder->width = width;
    encoder->height = height;
    if (error_code != NULL)
        *error_code = 0;
    return encoder;

fail:
    amaterasu_vaapi_destroy(encoder);
    return NULL;
}

void *amaterasu_ffmpeg_vaapi_open(int width, int height, int fps, int bitrate,
                                   int *error_code)
{
    return amaterasu_vaapi_open(width, height, fps, bitrate, error_code);
}

void amaterasu_ffmpeg_vaapi_close(void *opaque)
{
    amaterasu_vaapi_destroy((struct amaterasu_ffmpeg_vaapi *)opaque);
}

int amaterasu_ffmpeg_vaapi_header(void *opaque, const uint8_t **data, size_t *length)
{
    struct amaterasu_ffmpeg_vaapi *encoder = (struct amaterasu_ffmpeg_vaapi *)opaque;
    if (encoder == NULL || data == NULL || length == NULL ||
        encoder->context->extradata == NULL || encoder->context->extradata_size <= 0 ||
        encoder->context->extradata_size > 1024 * 1024)
        return AMATERASU_VAAPI_ERROR;
    *data = encoder->context->extradata;
    *length = (size_t)encoder->context->extradata_size;
    return 0;
}

int amaterasu_ffmpeg_vaapi_submit(void *opaque, const uint8_t *nv12,
                                   size_t length, int64_t timestamp_100ns,
                                   int force_idr)
{
    struct amaterasu_ffmpeg_vaapi *encoder = (struct amaterasu_ffmpeg_vaapi *)opaque;
    if (encoder == NULL || nv12 == NULL || encoder->draining || timestamp_100ns < 0 ||
        (force_idr != 0 && force_idr != 1))
        return AMATERASU_VAAPI_ERROR;
    size_t y_bytes = (size_t)encoder->width * (size_t)encoder->height;
    size_t expected = y_bytes + y_bytes / 2;
    if (length != expected || av_frame_make_writable(encoder->software_frame) < 0)
        return AMATERASU_VAAPI_ERROR;

    for (int row = 0; row < encoder->height; row++)
        memcpy(encoder->software_frame->data[0] +
                   (size_t)row * encoder->software_frame->linesize[0],
               nv12 + (size_t)row * encoder->width, (size_t)encoder->width);
    const uint8_t *source_uv = nv12 + y_bytes;
    for (int row = 0; row < encoder->height / 2; row++)
        memcpy(encoder->software_frame->data[1] +
                   (size_t)row * encoder->software_frame->linesize[1],
               source_uv + (size_t)row * encoder->width, (size_t)encoder->width);
    encoder->software_frame->pts = timestamp_100ns;

    AVFrame *hardware_frame = av_frame_alloc();
    if (hardware_frame == NULL)
        return AMATERASU_VAAPI_ERROR;
    int status = av_hwframe_get_buffer(encoder->frames_ref, hardware_frame, 0);
    if (status >= 0)
        status = av_hwframe_transfer_data(hardware_frame, encoder->software_frame, 0);
    if (status < 0) {
        av_frame_free(&hardware_frame);
        return AMATERASU_VAAPI_ERROR;
    }
    /* Transfer does not guarantee these properties on all FFmpeg versions. */
    hardware_frame->pts = timestamp_100ns;
    hardware_frame->color_range = AVCOL_RANGE_MPEG;
    hardware_frame->color_primaries = AVCOL_PRI_BT709;
    hardware_frame->color_trc = AVCOL_TRC_BT709;
    hardware_frame->colorspace = AVCOL_SPC_BT709;
    hardware_frame->pict_type = force_idr ? AV_PICTURE_TYPE_I : AV_PICTURE_TYPE_NONE;
    status = avcodec_send_frame(encoder->context, hardware_frame);
    av_frame_free(&hardware_frame);
    if (status == AVERROR(EAGAIN))
        return AMATERASU_VAAPI_RETRY;
    return status == 0 ? 0 : AMATERASU_VAAPI_ERROR;
}

int amaterasu_ffmpeg_vaapi_drain(void *opaque)
{
    struct amaterasu_ffmpeg_vaapi *encoder = (struct amaterasu_ffmpeg_vaapi *)opaque;
    if (encoder == NULL)
        return AMATERASU_VAAPI_ERROR;
    if (encoder->draining)
        return 0;
    int status = avcodec_send_frame(encoder->context, NULL);
    if (status == AVERROR(EAGAIN))
        return AMATERASU_VAAPI_RETRY;
    if (status < 0 && status != AVERROR_EOF)
        return AMATERASU_VAAPI_ERROR;
    encoder->draining = 1;
    return 0;
}

/* Packet storage is owned by AVPacket until the next receive; Rust copies it. */
int amaterasu_ffmpeg_vaapi_receive(void *opaque, const uint8_t **data,
                                    size_t *length, int64_t *timestamp_100ns,
                                    int *keyframe)
{
    struct amaterasu_ffmpeg_vaapi *encoder = (struct amaterasu_ffmpeg_vaapi *)opaque;
    if (encoder == NULL || data == NULL || length == NULL ||
        timestamp_100ns == NULL || keyframe == NULL)
        return AMATERASU_VAAPI_ERROR;
    av_packet_unref(encoder->packet);
    int status = avcodec_receive_packet(encoder->context, encoder->packet);
    if (status == AVERROR(EAGAIN))
        return AMATERASU_VAAPI_AGAIN;
    if (status == AVERROR_EOF)
        return AMATERASU_VAAPI_EOF;
    if (status < 0 || encoder->packet->data == NULL || encoder->packet->size <= 0 ||
        encoder->packet->size > AMATERASU_VAAPI_MAX_PACKET ||
        encoder->packet->pts == AV_NOPTS_VALUE)
        return AMATERASU_VAAPI_ERROR;
    *data = encoder->packet->data;
    *length = (size_t)encoder->packet->size;
    *timestamp_100ns = encoder->packet->pts;
    *keyframe = (encoder->packet->flags & AV_PKT_FLAG_KEY) != 0;
    return AMATERASU_VAAPI_PACKET;
}
