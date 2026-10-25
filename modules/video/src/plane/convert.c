/* FFmpeg objects stay on the preprocessing worker. Pixel pointers are borrowed
 * only during convert; Rust owns both initialized input and writable output. */
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <libavutil/pixfmt.h>
#include <libswscale/swscale.h>

struct am_video_converter { struct SwsContext *context; };

void *am_video_converter_open(void) {
    return calloc(1, sizeof(struct am_video_converter));
}

void am_video_converter_close(void *pointer) {
    struct am_video_converter *converter = pointer;
    if (!converter) return;
    sws_freeContext(converter->context);
    free(converter);
}

int am_video_convert(void *pointer, const uint8_t *source, int source_width,
    int source_height, int source_stride, uint8_t *destination, int width,
    int height, int bt709, int full_range) {
    struct am_video_converter *converter = pointer;
    if (!converter || !source || !destination || source_width <= 0 || source_height <= 0
        || source_width > 8192 || source_height > 8192 || source_stride < source_width * 4
        || width < 16 || height < 16 || width > 3840 || height > 2160 || (width & 1) || (height & 1)) return -1;
    int fit_width = width, fit_height = height;
    if ((int64_t)width * source_height <= (int64_t)height * source_width)
        fit_height = (int)((int64_t)width * source_height / source_width);
    else
        fit_width = (int)((int64_t)height * source_width / source_height);
    fit_width = fit_width < 2 ? 2 : fit_width & ~1;
    fit_height = fit_height < 2 ? 2 : fit_height & ~1;
    int left = ((width - fit_width) / 2) & ~1;
    int top = ((height - fit_height) / 2) & ~1;
    converter->context = sws_getCachedContext(converter->context, source_width,
        source_height, AV_PIX_FMT_BGRA, fit_width, fit_height, AV_PIX_FMT_NV12,
        SWS_BILINEAR, NULL, NULL, NULL);
    if (!converter->context) return -1;
    const int *coefficients = sws_getCoefficients(bt709 ? SWS_CS_ITU709 : SWS_CS_ITU601);
    if (sws_setColorspaceDetails(converter->context, coefficients, 1,
        coefficients, full_range, 0, 1 << 16, 1 << 16) < 0) return -1;
    size_t luma_bytes = (size_t)width * height;
    memset(destination, full_range ? 0 : 16, luma_bytes);
    memset(destination + luma_bytes, 128, luma_bytes / 2);
    const uint8_t *input[4] = {source, NULL, NULL, NULL};
    int input_stride[4] = {source_stride, 0, 0, 0};
    uint8_t *output[4] = {destination + (size_t)top * width + left,
        destination + luma_bytes + (size_t)(top / 2) * width + left, NULL, NULL};
    int output_stride[4] = {width, width, 0, 0};
    return sws_scale(converter->context, input, input_stride, 0, source_height,
        output, output_stride) == fit_height ? 0 : -1;
}
