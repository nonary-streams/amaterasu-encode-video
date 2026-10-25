/*
 * Support for the ignored GPU tests only: allocates a linear XRGB8888 GBM
 * buffer, fills it on the CPU and exports it as a DMA-BUF. Production code
 * never calls these functions. libgbm is loaded on first use.
 */
#include "adapter.h"

#include <dlfcn.h>
#include <fcntl.h>
#include <gbm.h>
#include <string.h>
#include <unistd.h>

struct am_test_gbm_api {
    struct gbm_device *(*create_device)(int fd);
    void (*device_destroy)(struct gbm_device *device);
    struct gbm_bo *(*bo_create)(struct gbm_device *device, uint32_t width, uint32_t height,
                                uint32_t format, uint32_t flags);
    void (*bo_destroy)(struct gbm_bo *buffer);
    void *(*bo_map)(struct gbm_bo *buffer, uint32_t x, uint32_t y, uint32_t width,
                    uint32_t height, uint32_t flags, uint32_t *stride, void **map_data);
    void (*bo_unmap)(struct gbm_bo *buffer, void *map_data);
    int (*bo_get_fd)(struct gbm_bo *buffer);
    uint32_t (*bo_get_stride)(struct gbm_bo *buffer);
    uint32_t (*bo_get_offset)(struct gbm_bo *buffer, int plane);
    uint64_t (*bo_get_modifier)(struct gbm_bo *buffer);
};

static int load(void *library, const char *name, void *destination) {
    void *symbol = dlsym(library, name);
    memcpy(destination, &symbol, sizeof(symbol));
    return symbol != NULL;
}

static int load_test_gbm(struct am_test_gbm_api *gbm) {
    void *library = dlopen("libgbm.so.1", RTLD_NOW | RTLD_LOCAL);
    return library != NULL && load(library, "gbm_create_device", &gbm->create_device) &&
           load(library, "gbm_device_destroy", &gbm->device_destroy) &&
           load(library, "gbm_bo_create", &gbm->bo_create) &&
           load(library, "gbm_bo_destroy", &gbm->bo_destroy) &&
           load(library, "gbm_bo_map", &gbm->bo_map) &&
           load(library, "gbm_bo_unmap", &gbm->bo_unmap) &&
           load(library, "gbm_bo_get_fd", &gbm->bo_get_fd) &&
           load(library, "gbm_bo_get_stride", &gbm->bo_get_stride) &&
           load(library, "gbm_bo_get_offset", &gbm->bo_get_offset) &&
           load(library, "gbm_bo_get_modifier", &gbm->bo_get_modifier);
}

/* Loaded on first allocation and kept; only the test process uses it. */
static struct am_test_gbm_api gbm_api;

static int fill(const struct am_test_gbm_api *gbm, struct gbm_bo *buffer, uint32_t width,
                uint32_t height, uint32_t top_xrgb, uint32_t bottom_xrgb) {
    uint32_t stride = 0;
    void *map_data = NULL;
    uint8_t *pixels =
        gbm->bo_map(buffer, 0, 0, width, height, GBM_BO_TRANSFER_WRITE, &stride, &map_data);
    if (pixels == NULL) {
        return 0;
    }
    for (uint32_t row = 0; row < height; ++row) {
        uint32_t value = row < height / 2 ? top_xrgb : bottom_xrgb;
        uint32_t *line = (uint32_t *)(pixels + (size_t)row * stride);
        for (uint32_t column = 0; column < width; ++column) {
            line[column] = value;
        }
    }
    gbm->bo_unmap(buffer, map_data);
    return 1;
}

int amaterasu_nvcuda_test_allocate_gbm(const char *render_node, uint32_t width,
                                       uint32_t height, uint32_t top_xrgb,
                                       uint32_t bottom_xrgb,
                                       struct am_nvcuda_test_buffer *out) {
    struct am_test_gbm_api *gbm = &gbm_api;
    memset(out, 0, sizeof(*out));
    out->render_fd = -1;
    out->dmabuf_fd = -1;
    if (gbm->create_device == NULL && !load_test_gbm(gbm)) {
        return AM_NVCUDA_LIBRARY_UNAVAILABLE;
    }
    out->render_fd = open(render_node, O_RDWR | O_CLOEXEC);
    if (out->render_fd < 0) {
        return AM_NVCUDA_NO_DEVICE;
    }
    struct gbm_device *device = gbm->create_device(out->render_fd);
    struct gbm_bo *buffer =
        device != NULL ? gbm->bo_create(device, width, height, GBM_FORMAT_XRGB8888,
                                       GBM_BO_USE_RENDERING | GBM_BO_USE_LINEAR)
                       : NULL;
    out->device = device;
    out->buffer = buffer;
    if (buffer == NULL || !fill(gbm, buffer, width, height, top_xrgb, bottom_xrgb)) {
        amaterasu_nvcuda_test_release_gbm(out);
        return AM_NVCUDA_ALLOCATION_FAILED;
    }
    out->dmabuf_fd = gbm->bo_get_fd(buffer);
    out->pitch = gbm->bo_get_stride(buffer);
    out->offset = gbm->bo_get_offset(buffer, 0);
    out->modifier = gbm->bo_get_modifier(buffer);
    if (out->dmabuf_fd < 0) {
        amaterasu_nvcuda_test_release_gbm(out);
        return AM_NVCUDA_ALLOCATION_FAILED;
    }
    return AM_NVCUDA_OK;
}

void amaterasu_nvcuda_test_release_gbm(struct am_nvcuda_test_buffer *buffer) {
    if (buffer->buffer != NULL) {
        gbm_api.bo_destroy(buffer->buffer);
    }
    if (buffer->device != NULL) {
        gbm_api.device_destroy(buffer->device);
    }
    if (buffer->dmabuf_fd >= 0) {
        close(buffer->dmabuf_fd);
    }
    if (buffer->render_fd >= 0) {
        close(buffer->render_fd);
    }
    memset(buffer, 0, sizeof(*buffer));
    buffer->render_fd = -1;
    buffer->dmabuf_fd = -1;
}
