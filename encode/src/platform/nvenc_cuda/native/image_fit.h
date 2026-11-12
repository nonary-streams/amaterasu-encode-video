#ifndef AM_NVENC_IMAGE_FIT_H
#define AM_NVENC_IMAGE_FIT_H
#include <stdint.h>

/* Same integer rectangle used by video's ImageFit and absolute input mapping.
 * Callers validate nonzero source and target sizes before using this helper. */
static inline void am_image_fit(uint32_t source_width, uint32_t source_height,
                                 uint32_t width, uint32_t height, int fit[4]) {
    uint32_t fitted_width = width, fitted_height = height;
    if ((uint64_t)width * source_height <= (uint64_t)height * source_width) {
        fitted_height = (uint32_t)((uint64_t)width * source_height / source_width);
        if (fitted_height == 0) fitted_height = 1;
    } else {
        fitted_width = (uint32_t)((uint64_t)height * source_width / source_height);
        if (fitted_width == 0) fitted_width = 1;
    }
    fit[0] = (int)((width - fitted_width) / 2);
    fit[1] = (int)((height - fitted_height) / 2);
    fit[2] = (int)fitted_width;
    fit[3] = (int)fitted_height;
}
#endif
