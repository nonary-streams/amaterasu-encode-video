/*
 * CUDA resource input: converts a producer's packed X2B10G10R10 frame in
 * CUDA device memory to P010 in an encode slot, on the encoder's ordered
 * conversion stream, after waiting there for the producer's ready event.
 * No GL context is involved. The kernel (cuda/convert.cu) reproduces the GL
 * luma and chroma passes in convert.c and ships as checked-in PTX.
 */
#include "internal.h"
#include "image_fit.h"
#include "nvenc_convert_ptx.h"

#include <stdio.h>
#include <string.h>

#define AM_CUDA_INPUT_BLOCK 16u

/* Mirrors `struct Rows` in cuda/convert.cu: three float4 rows. */
struct am_cuda_rows {
    _Alignas(16) float luma[4];
    _Alignas(16) float cb[4];
    _Alignas(16) float cr[4];
};

int am_cuda_input_open(struct am_cuda_input *input, const struct am_driver *driver,
                       CUdevice device) {
    memset(input, 0, sizeof(*input));
    const struct am_cuda_api *cuda = &driver->cuda;
    if (cuda->module_load == NULL || cuda->module_unload == NULL ||
        cuda->module_get_function == NULL || cuda->launch_kernel == NULL ||
        cuda->stream_wait_event == NULL || cuda->device_get_uuid == NULL) {
        return 0;
    }
    CUuuid uuid;
    if (cuda->device_get_uuid(&uuid, device) != CUDA_SUCCESS) {
        return 0;
    }
    if (cuda->module_load(&input->module, am_nvenc_convert_ptx) != CUDA_SUCCESS) {
        input->module = NULL;
        return 0;
    }
    if (cuda->module_get_function(&input->to_p010, input->module,
                                  "am_nvenc_x2b10g10r10_to_p010") != CUDA_SUCCESS) {
        cuda->module_unload(input->module);
        input->module = NULL;
        return 0;
    }
    memcpy(input->device_uuid, uuid.bytes, sizeof(input->device_uuid));
    input->available = 1;
    return 1;
}

void am_cuda_input_close(struct am_cuda_input *input, const struct am_driver *driver) {
    if (input->module != NULL) {
        driver->cuda.module_unload(input->module);
    }
    memset(input, 0, sizeof(*input));
}

int am_cuda_input_convert(const struct am_cuda_input *input, const struct am_driver *driver,
                          const struct am_converter *converter, CUstream stream, CUevent ready,
                          CUdeviceptr source, uint32_t source_pitch, uint32_t source_width,
                          uint32_t source_height, CUdeviceptr target,
                          size_t target_pitch, struct am_nvcuda_status *status) {
    const struct am_cuda_api *cuda = &driver->cuda;
    if (!input->available || stream == NULL || ready == NULL || target_pitch > UINT32_MAX) {
        am_set_status(status, AM_STAGE_CUDA_INTEROP, 0);
        return AM_NVCUDA_UNSUPPORTED;
    }
    /* Orders the kernel after the producer's last write, without the CPU. */
    CUresult result = cuda->stream_wait_event(stream, ready, 0);
    if (result != CUDA_SUCCESS) {
        fprintf(stderr, "amaterasu nvenc: CUDA input event wait failed: %d\n", (int)result);
        am_set_status(status, AM_STAGE_CUDA_INTEROP, (int32_t)result);
        return AM_NVCUDA_NATIVE_FAILURE;
    }
    struct am_cuda_rows rows;
    memcpy(rows.luma, converter->luma_row, sizeof(rows.luma));
    memcpy(rows.cb, converter->cb_row, sizeof(rows.cb));
    memcpy(rows.cr, converter->cr_row, sizeof(rows.cr));
    unsigned int pitch_in = source_pitch;
    unsigned int pitch_out = (unsigned int)target_pitch;
    int width = (int)converter->width;
    int height = (int)converter->height;
    int source_w = (int)source_width, source_h = (int)source_height;
    int fit[4];
    am_image_fit(source_width, source_height, converter->width, converter->height, fit);
    void *arguments[] = {&source, &pitch_in, &target, &pitch_out, &width, &height,
                        &source_w, &source_h, &fit[0], &fit[1], &fit[2], &fit[3], &rows};
    const unsigned int blocks_x = (converter->width / 2 + AM_CUDA_INPUT_BLOCK - 1) / AM_CUDA_INPUT_BLOCK;
    const unsigned int blocks_y = (converter->height / 2 + AM_CUDA_INPUT_BLOCK - 1) / AM_CUDA_INPUT_BLOCK;
    result = cuda->launch_kernel(input->to_p010, blocks_x, blocks_y, 1, AM_CUDA_INPUT_BLOCK,
                                 AM_CUDA_INPUT_BLOCK, 1, 0, stream, arguments, NULL);
    if (result != CUDA_SUCCESS) {
        fprintf(stderr, "amaterasu nvenc: CUDA input conversion launch failed: %d\n", (int)result);
        am_set_status(status, AM_STAGE_CUDA_INTEROP, (int32_t)result);
        return AM_NVCUDA_NATIVE_FAILURE;
    }
    return AM_NVCUDA_OK;
}
