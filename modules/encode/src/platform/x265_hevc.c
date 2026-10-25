#include <limits.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <x265.h>

#define AMATERASU_MAX_PACKET (16u * 1024u * 1024u)
#define AMATERASU_MAX_HEADER (1024u * 1024u)
#define AMATERASU_QUEUE_CAPACITY 4u

typedef struct queued_packet {
    uint8_t *bytes;
    size_t length;
    int64_t pts;
    int keyframe;
} queued_packet;

typedef struct amaterasu_x265 {
    const x265_api *api;
    x265_encoder *encoder;
    x265_param *param;
    uint8_t *u_plane;
    uint8_t *v_plane;
    size_t chroma_bytes;
    size_t frame_bytes;
    int width;
    int height;
    uint8_t *header;
    size_t header_length;
    uint8_t *last_output;
    queued_packet queue[AMATERASU_QUEUE_CAPACITY];
    uint32_t queue_head;
    uint32_t queue_count;
    int draining;
    int eof;
    int64_t last_pts;
    int have_last_pts;
} amaterasu_x265;

static void free_queue(amaterasu_x265 *state) {
    free(state->last_output);
    state->last_output = NULL;
    for (uint32_t i = 0; i < AMATERASU_QUEUE_CAPACITY; ++i) {
        free(state->queue[i].bytes);
        memset(&state->queue[i], 0, sizeof(state->queue[i]));
    }
    state->queue_head = 0;
    state->queue_count = 0;
}

static void destroy(amaterasu_x265 *state) {
    if (!state)
        return;
    free_queue(state);
    if (state->encoder)
        state->api->encoder_close(state->encoder);
    if (state->param)
        state->api->param_free(state->param);
    free(state->header);
    free(state->u_plane);
    free(state->v_plane);
    free(state);
}

static int valid_api(const x265_api *api) {
    return api && api->bit_depth == 8 && api->api_major_version == X265_MAJOR_VERSION &&
           api->api_build_number == X265_BUILD && api->sizeof_param == (int)sizeof(x265_param) &&
           api->sizeof_picture == (int)sizeof(x265_picture) && api->param_alloc &&
           api->param_free && api->param_default_preset && api->param_apply_profile &&
           api->picture_alloc && api->picture_free && api->picture_init && api->encoder_open &&
           api->encoder_headers && api->encoder_encode && api->encoder_close;
}

static int copy_nals(const x265_nal *nals, uint32_t count, uint8_t **output, size_t *length) {
    if (!nals || count == 0 || count > 1024)
        return -1;
    size_t total = 0;
    for (uint32_t i = 0; i < count; ++i) {
        if (!nals[i].payload || nals[i].sizeBytes == 0 ||
            nals[i].sizeBytes > AMATERASU_MAX_PACKET ||
            total > AMATERASU_MAX_PACKET - nals[i].sizeBytes)
            return -1;
        total += nals[i].sizeBytes;
    }
    uint8_t *bytes = (uint8_t *)malloc(total);
    if (!bytes)
        return -1;
    size_t offset = 0;
    for (uint32_t i = 0; i < count; ++i) {
        memcpy(bytes + offset, nals[i].payload, nals[i].sizeBytes);
        offset += nals[i].sizeBytes;
    }
    *output = bytes;
    *length = total;
    return 0;
}

static int enqueue_picture(amaterasu_x265 *state, const x265_nal *nals, uint32_t count,
                           const x265_picture *picture) {
    if (count == 0)
        return 0;
    if (state->queue_count >= AMATERASU_QUEUE_CAPACITY)
        return 3;
    uint32_t slot = (state->queue_head + state->queue_count) % AMATERASU_QUEUE_CAPACITY;
    queued_packet packet = {0};
    if (copy_nals(nals, count, &packet.bytes, &packet.length) != 0)
        return -1;
    packet.pts = picture->pts;
    for (uint32_t i = 0; i < count; ++i) {
        const uint8_t *payload = nals[i].payload;
        size_t size = nals[i].sizeBytes;
        size_t header = size >= 5 && payload[0] == 0 && payload[1] == 0 &&
                                payload[2] == 0 && payload[3] == 1
                            ? 4
                            : size >= 4 && payload[0] == 0 && payload[1] == 0 && payload[2] == 1
                                  ? 3
                                  : size;
        if (header + 2 <= size) {
            uint8_t type = (payload[header] >> 1) & 0x3f;
            if (type == 19 || type == 20)
                packet.keyframe = 1;
        }
    }
    state->queue[slot] = packet;
    state->queue_count++;
    return 0;
}

static int encode_one(amaterasu_x265 *state, x265_picture *input) {
    x265_nal *nals = NULL;
    uint32_t count = 0;
    x265_picture output;
    state->api->picture_init(state->param, &output);
    int result = state->api->encoder_encode(state->encoder, &nals, &count, input, &output);
    if (result < 0)
        return -1;
    return enqueue_picture(state, nals, count, &output);
}

void *amaterasu_x265_hevc_open(int width, int height, int fps, int bitrate,
                               int max_reference_frames, int full_range, int *error) {
    if (error)
        *error = 1;
    if (width < 16 || width > 3840 || height < 16 || height > 2160 ||
        (width & 1) || (height & 1) || fps < 1 || fps > 120 || bitrate < 100000 ||
        bitrate > 100000000 || max_reference_frames < 0 || max_reference_frames > 1 ||
        full_range < 0 || full_range > 1)
        return NULL;

    const x265_api *api = x265_api_get(8);
    if (!valid_api(api)) {
        if (error)
            *error = 2;
        return NULL;
    }
    amaterasu_x265 *state = (amaterasu_x265 *)calloc(1, sizeof(*state));
    if (!state) {
        if (error)
            *error = 3;
        return NULL;
    }
    state->api = api;
    state->width = width;
    state->height = height;
    state->frame_bytes = (size_t)width * (size_t)height * 3u / 2u;
    state->chroma_bytes = (size_t)width * (size_t)height / 4u;
    state->u_plane = (uint8_t *)malloc(state->chroma_bytes);
    state->v_plane = (uint8_t *)malloc(state->chroma_bytes);
    state->param = api->param_alloc();
    if (!state->u_plane || !state->v_plane || !state->param) {
        if (error)
            *error = 3;
        destroy(state);
        return NULL;
    }
    if (api->param_default_preset(state->param, "ultrafast", "zerolatency") < 0) {
        if (error)
            *error = 4;
        destroy(state);
        return NULL;
    }
    state->param->sourceWidth = width;
    state->param->sourceHeight = height;
    state->param->fpsNum = (uint32_t)fps;
    state->param->fpsDenom = 1;
    state->param->internalBitDepth = 8;
    state->param->internalCsp = X265_CSP_I420;
    state->param->rc.rateControlMode = X265_RC_ABR;
    state->param->rc.bitrate = bitrate / 1000;
    state->param->rc.vbvMaxBitrate = bitrate / 1000;
    state->param->rc.vbvBufferSize = bitrate / 1000;
    state->param->bframes = 0;
    state->param->maxNumReferences = max_reference_frames == 0 ? 3 : 1;
    state->param->lookaheadDepth = 0;
    state->param->frameNumThreads = 1;
    state->param->bOpenGOP = 0;
    state->param->bRepeatHeaders = 1;
    state->param->bAnnexB = 1;
    state->param->bCopyPicToFrame = 1;
    state->param->bEmitInfoSEI = 0;
    state->param->logLevel = X265_LOG_NONE;
    state->param->vui.bEnableVideoSignalTypePresentFlag = 1;
    state->param->vui.videoFormat = 5;
    state->param->vui.bEnableVideoFullRangeFlag = full_range;
    state->param->vui.bEnableColorDescriptionPresentFlag = 1;
    state->param->vui.colorPrimaries = 1;
    state->param->vui.transferCharacteristics = 1;
    state->param->vui.matrixCoeffs = 1;
    if (api->param_apply_profile(state->param, "main") < 0) {
        if (error)
            *error = 4;
        destroy(state);
        return NULL;
    }
    state->encoder = api->encoder_open(state->param);
    if (!state->encoder) {
        if (error)
            *error = 5;
        destroy(state);
        return NULL;
    }

    x265_nal *nals = NULL;
    uint32_t count = 0;
    if (api->encoder_headers(state->encoder, &nals, &count) <= 0 || !nals || count == 0 ||
        count > 1024) {
        if (error)
            *error = 6;
        destroy(state);
        return NULL;
    }
    size_t header_length = 0;
    for (uint32_t i = 0; i < count; ++i) {
        if (!nals[i].payload || nals[i].sizeBytes == 0 ||
            nals[i].sizeBytes > AMATERASU_MAX_HEADER ||
            header_length > AMATERASU_MAX_HEADER - nals[i].sizeBytes) {
            if (error)
                *error = 6;
            destroy(state);
            return NULL;
        }
        header_length += nals[i].sizeBytes;
    }
    state->header = (uint8_t *)malloc(header_length);
    if (!state->header) {
        if (error)
            *error = 3;
        destroy(state);
        return NULL;
    }
    size_t offset = 0;
    for (uint32_t i = 0; i < count; ++i) {
        memcpy(state->header + offset, nals[i].payload, nals[i].sizeBytes);
        offset += nals[i].sizeBytes;
    }
    state->header_length = header_length;
    if (error)
        *error = 0;
    return state;
}

void amaterasu_x265_hevc_close(void *opaque) {
    destroy((amaterasu_x265 *)opaque);
}

int amaterasu_x265_hevc_header(void *opaque, const uint8_t **data, size_t *length) {
    amaterasu_x265 *state = (amaterasu_x265 *)opaque;
    if (!state || !data || !length || !state->header || state->header_length == 0)
        return -1;
    *data = state->header;
    *length = state->header_length;
    return 0;
}

int amaterasu_x265_hevc_submit(void *opaque, const uint8_t *nv12, size_t length,
                               int64_t pts, int force_idr) {
    amaterasu_x265 *state = (amaterasu_x265 *)opaque;
    if (!state || !nv12 || length != state->frame_bytes || pts < 0 || state->draining ||
        (state->have_last_pts && pts <= state->last_pts))
        return -1;
    if (state->queue_count >= AMATERASU_QUEUE_CAPACITY)
        return 3;
    const uint8_t *uv = nv12 + (size_t)state->width * (size_t)state->height;
    for (size_t i = 0; i < state->chroma_bytes; ++i) {
        state->u_plane[i] = uv[i * 2u];
        state->v_plane[i] = uv[i * 2u + 1u];
    }
    x265_picture input;
    state->api->picture_init(state->param, &input);
    input.pts = pts;
    input.bitDepth = 8;
    input.colorSpace = X265_CSP_I420;
    input.planes[0] = (void *)nv12;
    input.planes[1] = state->u_plane;
    input.planes[2] = state->v_plane;
    input.stride[0] = state->width;
    input.stride[1] = state->width / 2;
    input.stride[2] = state->width / 2;
    input.sliceType = force_idr ? X265_TYPE_IDR : X265_TYPE_AUTO;
    int result = encode_one(state, &input);
    if (result != 0)
        return result;
    state->last_pts = pts;
    state->have_last_pts = 1;
    return 0;
}

int amaterasu_x265_hevc_drain(void *opaque) {
    amaterasu_x265 *state = (amaterasu_x265 *)opaque;
    if (!state)
        return -1;
    state->draining = 1;
    if (state->eof)
        return 0;
    if (state->queue_count >= AMATERASU_QUEUE_CAPACITY)
        return 3;
    x265_nal *nals = NULL;
    uint32_t count = 0;
    x265_picture output;
    state->api->picture_init(state->param, &output);
    int result = state->api->encoder_encode(state->encoder, &nals, &count, NULL, &output);
    if (result < 0)
        return -1;
    if (count == 0) {
        state->eof = 1;
        return 0;
    }
    result = enqueue_picture(state, nals, count, &output);
    return result == 0 ? 3 : result;
}

int amaterasu_x265_hevc_receive(void *opaque, const uint8_t **data, size_t *length,
                                int64_t *pts, int *keyframe) {
    amaterasu_x265 *state = (amaterasu_x265 *)opaque;
    if (!state || !data || !length || !pts || !keyframe)
        return -1;
    free(state->last_output);
    state->last_output = NULL;
    if (state->queue_count == 0)
        return state->eof ? 2 : 0;
    queued_packet *packet = &state->queue[state->queue_head];
    *data = packet->bytes;
    *length = packet->length;
    *pts = packet->pts;
    *keyframe = packet->keyframe;
    state->last_output = packet->bytes;
    memset(packet, 0, sizeof(*packet));
    state->queue_head = (state->queue_head + 1) % AMATERASU_QUEUE_CAPACITY;
    state->queue_count--;
    return 1;
}
