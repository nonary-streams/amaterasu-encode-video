/* ABI boundary: only this file sees x264's versioned public structures.
 * Function pointers and the shared library remain owned by the Rust encoder.
 * Input is borrowed during encode; x264 copies input before returning. Output
 * is borrowed until the next encode and must be copied by Rust immediately. */
#include <stdint.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>
#include <limits.h>
#include <x264.h>

struct amaterasu_x264_functions {
    int (*default_preset)(x264_param_t *, const char *, const char *);
    int (*apply_profile)(x264_param_t *, const char *);
    void (*param_cleanup)(x264_param_t *);
    void (*picture_init)(x264_picture_t *);
    x264_t *(*encoder_open)(x264_param_t *);
    void (*encoder_close)(x264_t *);
    int (*encoder_encode)(x264_t *, x264_nal_t **, int *, x264_picture_t *, x264_picture_t *);
    int (*delayed_frames)(x264_t *);
    int (*maximum_delayed_frames)(x264_t *);
    void (*encoder_parameters)(x264_t *, x264_param_t *);
};

struct amaterasu_x264_configuration {
    uint32_t width, height, fps, bitrate, reference_frames, min_threads;
};

struct amaterasu_x264_output {
    const uint8_t *data;
    size_t len;
    int64_t timestamp;
};

struct amaterasu_x264_encoder {
    struct amaterasu_x264_functions functions;
    x264_t *encoder;
    int width, height;
    int effective_threads;
};

int amaterasu_x264_build(void) { return X264_BUILD; }

struct amaterasu_x264_encoder *amaterasu_x264_open(
    const struct amaterasu_x264_functions *functions,
    const struct amaterasu_x264_configuration *configuration,
    const char *preset, const char *tune, int *status)
{
    x264_param_t parameters;
    memset(&parameters, 0, sizeof(parameters));
    *status = 1;
    if (functions->default_preset(&parameters, preset, tune) < 0) {
        functions->param_cleanup(&parameters);
        return NULL;
    }
    parameters.i_log_level = X264_LOG_NONE;
    parameters.i_width = (int)configuration->width;
    parameters.i_height = (int)configuration->height;
    parameters.i_csp = X264_CSP_NV12;
    parameters.i_bitdepth = 8;
    parameters.i_fps_num = configuration->fps;
    parameters.i_fps_den = 1;
    parameters.i_timebase_num = 1;
    parameters.i_timebase_den = 10000000;
    parameters.b_vfr_input = 1;
    parameters.b_annexb = 1;
    parameters.b_repeat_headers = 1;
    parameters.b_aud = 1;
    parameters.b_interlaced = 0;
    parameters.b_intra_refresh = 0;
    parameters.b_open_gop = 0;
    parameters.i_keyint_max = INT_MAX;
    parameters.i_keyint_min = 1;
    parameters.i_scenecut_threshold = 0;
    /* These streaming constraints take precedence over preset/tune choices.
     * No B pictures or unbounded lookahead may violate the native queue ABI. */
    parameters.i_bframe = 0;
    parameters.i_threads = (int)configuration->min_threads;
    parameters.i_lookahead_threads = 1;
    parameters.i_sync_lookahead = 0;
    parameters.rc.i_lookahead = 0;
    parameters.rc.i_rc_method = X264_RC_ABR;
    parameters.rc.i_bitrate = (int)(configuration->bitrate / 1000);
    parameters.rc.i_vbv_max_bitrate = parameters.rc.i_bitrate;
    parameters.rc.i_vbv_buffer_size = parameters.rc.i_bitrate / (int)configuration->fps;
    if (parameters.rc.i_vbv_buffer_size < 1)
        parameters.rc.i_vbv_buffer_size = 1;
    parameters.vui.b_fullrange = 0;
    parameters.vui.i_colorprim = 1;
    parameters.vui.i_transfer = 1;
    parameters.vui.i_colmatrix = 1;
    if (configuration->reference_frames != 0)
        parameters.i_frame_reference = (int)configuration->reference_frames;
    if (functions->apply_profile(&parameters, "baseline") < 0) {
        functions->param_cleanup(&parameters);
        return NULL;
    }
    *status = 2;
    x264_t *encoder = functions->encoder_open(&parameters);
    functions->param_cleanup(&parameters);
    if (encoder == NULL)
        return NULL;
    x264_param_t effective;
    functions->encoder_parameters(encoder, &effective);
    if (effective.i_bframe != 0 || effective.b_interlaced || !effective.b_annexb ||
        functions->maximum_delayed_frames(encoder) >= 16 ||
        (configuration->reference_frames != 0 &&
         effective.i_frame_reference != (int)configuration->reference_frames)) {
        functions->encoder_close(encoder);
        return NULL;
    }
    struct amaterasu_x264_encoder *result = malloc(sizeof(*result));
    if (result == NULL) {
        functions->encoder_close(encoder);
        return NULL;
    }
    result->functions = *functions;
    result->encoder = encoder;
    result->width = (int)configuration->width;
    result->height = (int)configuration->height;
    result->effective_threads = effective.i_threads;
    *status = 0;
    return result;
}

int amaterasu_x264_thread_count(struct amaterasu_x264_encoder *encoder)
{
    return encoder->effective_threads;
}

void amaterasu_x264_close(struct amaterasu_x264_encoder *encoder)
{
    encoder->functions.encoder_close(encoder->encoder);
    free(encoder);
}

int amaterasu_x264_delayed_frames(struct amaterasu_x264_encoder *encoder)
{
    return encoder->functions.delayed_frames(encoder->encoder);
}

int amaterasu_x264_encode(struct amaterasu_x264_encoder *encoder,
    const uint8_t *input, int64_t timestamp, int force_idr,
    struct amaterasu_x264_output *output)
{
    x264_picture_t picture, encoded;
    encoder->functions.picture_init(&picture);
    encoder->functions.picture_init(&encoded);
    if (input != NULL) {
        picture.img.i_csp = X264_CSP_NV12;
        picture.img.i_plane = 2;
        picture.img.i_stride[0] = encoder->width;
        picture.img.i_stride[1] = encoder->width;
        picture.img.plane[0] = (uint8_t *)input;
        picture.img.plane[1] = (uint8_t *)input + (size_t)encoder->width * encoder->height;
        picture.i_pts = timestamp;
        picture.i_type = force_idr ? X264_TYPE_IDR : X264_TYPE_AUTO;
    }
    x264_nal_t *units = NULL;
    int count = 0;
    int bytes = encoder->functions.encoder_encode(encoder->encoder, &units, &count,
        input == NULL ? NULL : &picture, &encoded);
    if (bytes <= 0)
        return bytes;
    if (units == NULL || count <= 0 || count > 1024 || bytes > 16 * 1024 * 1024)
        return -1;
    size_t total = 0;
    for (int index = 0; index < count; index++) {
        if (units[index].i_payload <= 0 || units[index].p_payload != units[0].p_payload + total)
            return -1;
        total += (size_t)units[index].i_payload;
    }
    if (total != (size_t)bytes)
        return -1;
    output->data = units[0].p_payload;
    output->len = total;
    output->timestamp = encoded.i_pts;
    return bytes;
}
