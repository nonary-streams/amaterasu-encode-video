#ifndef AMATERASU_PYROWAVE_GPU_ADAPTER_H
#define AMATERASU_PYROWAVE_GPU_ADAPTER_H
#include <stddef.h>
#include <stdint.h>
#include <vulkan/vulkan_core.h>
#include <pyrowave.h>

/* Only these Amaterasu-owned records and opaque handles cross into Rust.
 * Native function pointers and borrowed device remain pinned by its runtime. */
struct am_pyrowave_gpu_functions {
    void (*get_handles)(pyrowave_device, VkInstance *, VkPhysicalDevice *, VkDevice *);
    pyrowave_result (*image_create)(const pyrowave_image_create_info *, pyrowave_image *);
    pyrowave_result (*image_view)(pyrowave_image, VkImageAspectFlagBits, VkImageUsageFlagBits, pyrowave_image_view *);
    void (*image_destroy)(pyrowave_image);
    void (*wait_idle)(pyrowave_device);
    pyrowave_result (*encode_scaled)(pyrowave_encoder, const pyrowave_gpu_sync_operation *, const pyrowave_gpu_sync_operation *, const pyrowave_scaled_encode_info *, const pyrowave_rate_control *);
};
struct am_pyrowave_gpu_frame {
    uint32_t width, height, fourcc, plane_count;
    uint64_t modifier;
    int32_t fds[4];
    int32_t fences[4];
    uint32_t offsets[4];
    uint32_t pitches[4];
    uint32_t hdr;
};
struct am_pyrowave_gpu_identity {
    uint8_t device_uuid[16];
    uint32_t render_major, render_minor;
};
struct am_pyrowave_gpu;
struct am_pyrowave_gpu_image;
struct am_pyrowave_gpu *am_pyrowave_gpu_create(const struct am_pyrowave_gpu_functions *, void *device);
void am_pyrowave_gpu_destroy(struct am_pyrowave_gpu *);
int am_pyrowave_gpu_identity(struct am_pyrowave_gpu *, struct am_pyrowave_gpu_identity *);
int am_pyrowave_gpu_modifiers(struct am_pyrowave_gpu *, uint32_t fourcc, uint64_t *modifiers, uint32_t capacity, uint32_t *count);
int am_pyrowave_gpu_supports(struct am_pyrowave_gpu *, uint32_t fourcc, uint64_t modifier, uint32_t width, uint32_t height, uint32_t *planes);
/* Borrowed fds are never consumed. Returned import owns private duplicates.
 * Every native GPU access completes before image retirement returns. */
int am_pyrowave_gpu_import(struct am_pyrowave_gpu *, const struct am_pyrowave_gpu_frame *, struct am_pyrowave_gpu_image **);
int am_pyrowave_gpu_encode(struct am_pyrowave_gpu_image *, void *encoder, size_t budget);
void am_pyrowave_gpu_retire(struct am_pyrowave_gpu_image *);
#endif
