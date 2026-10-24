/* FFmpeg NVENC ABI boundary. All FFmpeg objects are confined to the encoder
 * worker. Packet data is borrowed until the next receive call; Rust copies it
 * before asking for another packet. */
#include <errno.h>
#include <stdint.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>

#include <libavcodec/avcodec.h>
#include <libavutil/error.h>
#include <libavutil/frame.h>
#include <libavutil/imgutils.h>
#include <libavutil/opt.h>

enum {
    AMATERASU_NVENC_ERROR = -1,
    AMATERASU_NVENC_AGAIN = 0,
    AMATERASU_NVENC_PACKET = 1,
    AMATERASU_NVENC_EOF = 2,
    AMATERASU_NVENC_RETRY = 3,
};

struct amaterasu_ffmpeg_nvenc {
    AVCodecContext *context;
    AVFrame *frame;
    AVPacket *packet;
    int width;
    int height;
    int draining;
};

static struct amaterasu_ffmpeg_nvenc *amaterasu_nvenc_open(
    int width, int height, int fps, int bitrate, int max_reference_frames,
    int full_range,
    int *error_code)
{
    if (error_code != NULL)
        *error_code = 1;
    if (width < 16 || height < 16 || (width & 1) != 0 || (height & 1) != 0 ||
        fps < 1 || fps > 120 || bitrate < 100000 || bitrate > 100000000 ||
        max_reference_frames < 0 || max_reference_frames > 1 || (full_range != 0 && full_range != 1))
        return NULL;

    const AVCodec *codec = avcodec_find_encoder_by_name("h264_nvenc");
    if (codec == NULL || codec->id != AV_CODEC_ID_H264)
        return NULL;

    struct amaterasu_ffmpeg_nvenc *encoder =
        (struct amaterasu_ffmpeg_nvenc *)calloc(1, sizeof(*encoder));
    if (encoder == NULL) {
        if (error_code != NULL)
            *error_code = 2;
        return NULL;
    }

    encoder->context = avcodec_alloc_context3(codec);
    encoder->frame = av_frame_alloc();
    encoder->packet = av_packet_alloc();
    if (encoder->context == NULL || encoder->frame == NULL || encoder->packet == NULL) {
        if (error_code != NULL)
            *error_code = 2;
        goto fail;
    }

    AVCodecContext *context = encoder->context;
    context->width = width;
    context->height = height;
    context->pix_fmt = AV_PIX_FMT_NV12;
    context->time_base = (AVRational){ 1, 10000000 };
    context->framerate = (AVRational){ fps, 1 };
    context->bit_rate = bitrate;
    context->rc_max_rate = bitrate;
    context->rc_buffer_size = bitrate / fps > 0 ? bitrate / fps : 1;
    context->gop_size = fps * 2;
    context->max_b_frames = 0;
    context->refs = 1;
    context->profile = AV_PROFILE_H264_BASELINE;
    context->thread_count = 1;
    context->flags |= AV_CODEC_FLAG_LOW_DELAY | AV_CODEC_FLAG_GLOBAL_HEADER;
    context->color_range = full_range ? AVCOL_RANGE_JPEG : AVCOL_RANGE_MPEG;
    context->color_primaries = AVCOL_PRI_BT709;
    context->color_trc = AVCOL_TRC_BT709;
    context->colorspace = AVCOL_SPC_BT709;
    AVDictionary *options = NULL;
    int option_status = 0;
    option_status |= av_dict_set(&options, "preset", "p1", 0);
    option_status |= av_dict_set(&options, "tune", "ull", 0);
    option_status |= av_dict_set(&options, "profile", "baseline", 0);
    option_status |= av_dict_set(&options, "rc", "cbr", 0);
    option_status |= av_dict_set(&options, "zerolatency", "1", 0);
    option_status |= av_dict_set(&options, "forced-idr", "1", 0);
    option_status |= av_dict_set(&options, "rc-lookahead", "0", 0);
    option_status |= av_dict_set(&options, "delay", "0", 0);
    /* Moonlight's constrained clients permit one reference picture. Bound the
     * decoded-picture buffer as well as FFmpeg's active reference lists. */
    option_status |= av_dict_set(&options, "dpb_size", "1", 0);
    if (option_status < 0 || avcodec_open2(context, codec, &options) < 0 || options != NULL) {
        av_dict_free(&options);
        if (error_code != NULL)
            *error_code = 3;
        goto fail;
    }
    av_dict_free(&options);

    if (context->pix_fmt != AV_PIX_FMT_NV12 || context->max_b_frames != 0 ||
        context->profile != AV_PROFILE_H264_BASELINE ||
        context->extradata == NULL || context->extradata_size <= 0 ||
        context->color_range != (full_range ? AVCOL_RANGE_JPEG : AVCOL_RANGE_MPEG) ||
        context->color_primaries != AVCOL_PRI_BT709 ||
        context->color_trc != AVCOL_TRC_BT709 || context->colorspace != AVCOL_SPC_BT709) {
        av_log(context, AV_LOG_ERROR,
            "Amaterasu NVENC negotiated unsupported format: pixel=%d bframes=%d profile=%d header=%d range=%d primaries=%d transfer=%d matrix=%d\n",
            context->pix_fmt, context->max_b_frames, context->profile,
            context->extradata_size, context->color_range, context->color_primaries,
            context->color_trc, context->colorspace);
        if (error_code != NULL)
            *error_code = 4;
        goto fail;
    }

    encoder->frame->format = AV_PIX_FMT_NV12;
    encoder->frame->width = width;
    encoder->frame->height = height;
    encoder->frame->color_range = full_range ? AVCOL_RANGE_JPEG : AVCOL_RANGE_MPEG;
    encoder->frame->color_primaries = AVCOL_PRI_BT709;
    encoder->frame->color_trc = AVCOL_TRC_BT709;
    encoder->frame->colorspace = AVCOL_SPC_BT709;
    if (av_frame_get_buffer(encoder->frame, 32) < 0) {
        if (error_code != NULL)
            *error_code = 2;
        goto fail;
    }

    encoder->width = width;
    encoder->height = height;
    if (error_code != NULL)
        *error_code = 0;
    return encoder;

fail:
    av_packet_free(&encoder->packet);
    av_frame_free(&encoder->frame);
    avcodec_free_context(&encoder->context);
    free(encoder);
    return NULL;
}

void *amaterasu_ffmpeg_nvenc_open(
    int width, int height, int fps, int bitrate, int max_reference_frames,
    int full_range,
    int *error_code)
{
    return amaterasu_nvenc_open(width, height, fps, bitrate, max_reference_frames,
                                full_range, error_code);
}

void amaterasu_ffmpeg_nvenc_close(void *opaque)
{
    struct amaterasu_ffmpeg_nvenc *encoder =
        (struct amaterasu_ffmpeg_nvenc *)opaque;
    if (encoder == NULL)
        return;
    av_packet_free(&encoder->packet);
    av_frame_free(&encoder->frame);
    avcodec_free_context(&encoder->context);
    free(encoder);
}

int amaterasu_ffmpeg_nvenc_header(void *opaque, const uint8_t **data, size_t *length)
{
    struct amaterasu_ffmpeg_nvenc *encoder =
        (struct amaterasu_ffmpeg_nvenc *)opaque;
    if (encoder == NULL || data == NULL || length == NULL ||
        encoder->context->extradata == NULL || encoder->context->extradata_size <= 0 ||
        encoder->context->extradata_size > 1024 * 1024)
        return AMATERASU_NVENC_ERROR;
    *data = encoder->context->extradata;
    *length = (size_t)encoder->context->extradata_size;
    return 0;
}

int amaterasu_ffmpeg_nvenc_submit(void *opaque, const uint8_t *nv12,
                                  size_t length, int64_t timestamp_100ns,
                                  int force_idr)
{
    struct amaterasu_ffmpeg_nvenc *encoder =
        (struct amaterasu_ffmpeg_nvenc *)opaque;
    if (encoder == NULL || nv12 == NULL || encoder->draining || timestamp_100ns < 0)
        return AMATERASU_NVENC_ERROR;
    size_t y_bytes = (size_t)encoder->width * (size_t)encoder->height;
    size_t expected = y_bytes + y_bytes / 2;
    if (length != expected || av_frame_make_writable(encoder->frame) < 0)
        return AMATERASU_NVENC_ERROR;

    for (int row = 0; row < encoder->height; row++)
        memcpy(encoder->frame->data[0] + (size_t)row * encoder->frame->linesize[0],
               nv12 + (size_t)row * encoder->width, (size_t)encoder->width);
    const uint8_t *source_uv = nv12 + y_bytes;
    for (int row = 0; row < encoder->height / 2; row++)
        memcpy(encoder->frame->data[1] + (size_t)row * encoder->frame->linesize[1],
               source_uv + (size_t)row * encoder->width, (size_t)encoder->width);

    encoder->frame->pts = timestamp_100ns;
    encoder->frame->pict_type = force_idr ? AV_PICTURE_TYPE_I : AV_PICTURE_TYPE_NONE;
    int status = avcodec_send_frame(encoder->context, encoder->frame);
    if (status == AVERROR(EAGAIN))
        return AMATERASU_NVENC_RETRY;
    return status == 0 ? 0 : AMATERASU_NVENC_ERROR;
}

int amaterasu_ffmpeg_nvenc_drain(void *opaque)
{
    struct amaterasu_ffmpeg_nvenc *encoder =
        (struct amaterasu_ffmpeg_nvenc *)opaque;
    if (encoder == NULL)
        return AMATERASU_NVENC_ERROR;
    if (encoder->draining)
        return 0;
    int status = avcodec_send_frame(encoder->context, NULL);
    if (status == AVERROR(EAGAIN))
        return AMATERASU_NVENC_RETRY;
    if (status < 0 && status != AVERROR_EOF)
        return AMATERASU_NVENC_ERROR;
    encoder->draining = 1;
    return 0;
}

/* A returned packet pointer remains owned by AVPacket and is valid until the
 * next receive call or close. The Rust caller copies it before either action. */
int amaterasu_ffmpeg_nvenc_receive(void *opaque, const uint8_t **data,
                                   size_t *length, int64_t *timestamp_100ns,
                                   int *keyframe)
{
    struct amaterasu_ffmpeg_nvenc *encoder =
        (struct amaterasu_ffmpeg_nvenc *)opaque;
    if (encoder == NULL || data == NULL || length == NULL ||
        timestamp_100ns == NULL || keyframe == NULL)
        return AMATERASU_NVENC_ERROR;

    av_packet_unref(encoder->packet);
    int status = avcodec_receive_packet(encoder->context, encoder->packet);
    if (status == AVERROR(EAGAIN))
        return AMATERASU_NVENC_AGAIN;
    if (status == AVERROR_EOF)
        return AMATERASU_NVENC_EOF;
    if (status < 0 || encoder->packet->data == NULL || encoder->packet->size <= 0 ||
        encoder->packet->size > 16 * 1024 * 1024 ||
        encoder->packet->pts == AV_NOPTS_VALUE)
        return AMATERASU_NVENC_ERROR;

    *data = encoder->packet->data;
    *length = (size_t)encoder->packet->size;
    *timestamp_100ns = encoder->packet->pts;
    *keyframe = (encoder->packet->flags & AV_PKT_FLAG_KEY) != 0;
    return AMATERASU_NVENC_PACKET;
}
