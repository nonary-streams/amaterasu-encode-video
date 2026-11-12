#define _GNU_SOURCE
#include "adapter.h"
#include <dlfcn.h>
#include <fcntl.h>
#include <errno.h>
#include <poll.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>
#include <sys/ioctl.h>
#include <linux/sync_file.h>
#include <time.h>

#define AM_FOURCC(a,b,c,d) ((uint32_t)(a) | ((uint32_t)(b)<<8) | ((uint32_t)(c)<<16) | ((uint32_t)(d)<<24))
struct am_pyrowave_gpu {
    struct am_pyrowave_gpu_functions api;
    pyrowave_device device;
    VkPhysicalDevice physical;
    VkDevice vk_device;
    void *loader;
    PFN_vkGetPhysicalDeviceProperties2 properties;
    PFN_vkGetPhysicalDeviceFormatProperties2 formats;
    PFN_vkGetPhysicalDeviceImageFormatProperties2 images;
    PFN_vkDeviceWaitIdle wait_idle;
};
struct am_pyrowave_gpu_image {
    struct am_pyrowave_gpu *gpu;
    pyrowave_image image;
    pyrowave_scaled_encode_info scaling;
};
static VkFormat pixel_format(uint32_t fourcc) {
    switch (fourcc) {
    case AM_FOURCC('X','R','2','4'): case AM_FOURCC('A','R','2','4'): return VK_FORMAT_B8G8R8A8_UNORM;
    case AM_FOURCC('X','B','2','4'): case AM_FOURCC('A','B','2','4'): return VK_FORMAT_R8G8B8A8_UNORM;
    case AM_FOURCC('X','R','3','0'): case AM_FOURCC('A','R','3','0'): return VK_FORMAT_A2R10G10B10_UNORM_PACK32;
    case AM_FOURCC('X','B','3','0'): case AM_FOURCC('A','B','3','0'): return VK_FORMAT_A2B10G10R10_UNORM_PACK32;
    default: return VK_FORMAT_UNDEFINED;
    }
}
struct am_pyrowave_gpu *am_pyrowave_gpu_create(const struct am_pyrowave_gpu_functions *api, void *device) {
    if (!api || !device) return NULL;
    struct am_pyrowave_gpu *gpu = calloc(1, sizeof(*gpu));
    if (!gpu) return NULL;
    gpu->api = *api;
    gpu->device = device;
    gpu->loader = dlopen("libvulkan.so.1", RTLD_NOW | RTLD_LOCAL);
    if (!gpu->loader) { free(gpu); return NULL; }
    PFN_vkGetInstanceProcAddr get = (PFN_vkGetInstanceProcAddr)dlsym(gpu->loader, "vkGetInstanceProcAddr");
    VkInstance instance = VK_NULL_HANDLE;
    api->get_handles(device, &instance, &gpu->physical, &gpu->vk_device);
    if (!get || !instance || !gpu->physical || !gpu->vk_device) goto fail;
    gpu->properties = (PFN_vkGetPhysicalDeviceProperties2)get(instance, "vkGetPhysicalDeviceProperties2");
    gpu->formats = (PFN_vkGetPhysicalDeviceFormatProperties2)get(instance, "vkGetPhysicalDeviceFormatProperties2");
    gpu->images = (PFN_vkGetPhysicalDeviceImageFormatProperties2)get(instance, "vkGetPhysicalDeviceImageFormatProperties2");
    PFN_vkGetDeviceProcAddr get_device = (PFN_vkGetDeviceProcAddr)get(instance, "vkGetDeviceProcAddr");
    if (!gpu->properties || !gpu->formats || !gpu->images || !get_device) goto fail;
    gpu->wait_idle = (PFN_vkDeviceWaitIdle)get_device(gpu->vk_device, "vkDeviceWaitIdle");
    if (!gpu->wait_idle) goto fail;
    return gpu;
fail:
    am_pyrowave_gpu_destroy(gpu);
    return NULL;
}
void am_pyrowave_gpu_destroy(struct am_pyrowave_gpu *gpu) {
    if (gpu) { if (gpu->loader) dlclose(gpu->loader); free(gpu); }
}
int am_pyrowave_gpu_identity(struct am_pyrowave_gpu *gpu, struct am_pyrowave_gpu_identity *output) {
    if (!gpu || !output) return -1;
    VkPhysicalDeviceDrmPropertiesEXT drm = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRM_PROPERTIES_EXT };
    VkPhysicalDeviceIDProperties identity = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ID_PROPERTIES, .pNext = &drm };
    VkPhysicalDeviceProperties2 properties = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2, .pNext = &identity };
    gpu->properties(gpu->physical, &properties);
    if (!drm.hasRender) return -1;
    memcpy(output->device_uuid, identity.deviceUUID, sizeof(output->device_uuid));
    output->render_major = (uint32_t)drm.renderMajor;
    output->render_minor = (uint32_t)drm.renderMinor;
    return 0;
}
int am_pyrowave_gpu_modifiers(struct am_pyrowave_gpu *gpu, uint32_t fourcc, uint64_t *modifiers, uint32_t capacity, uint32_t *count) {
    if (!gpu || !modifiers || !count || !capacity) return -1;
    VkFormat format = pixel_format(fourcc);
    if (format == VK_FORMAT_UNDEFINED) return -1;
    VkDrmFormatModifierPropertiesListEXT list = { .sType = VK_STRUCTURE_TYPE_DRM_FORMAT_MODIFIER_PROPERTIES_LIST_EXT };
    VkFormatProperties2 properties = { .sType = VK_STRUCTURE_TYPE_FORMAT_PROPERTIES_2, .pNext = &list };
    gpu->formats(gpu->physical, format, &properties);
    if (list.drmFormatModifierCount > 4096) return -1;
    if (!list.drmFormatModifierCount) { *count = 0; return 0; }
    uint32_t available = list.drmFormatModifierCount;
    list.pDrmFormatModifierProperties = calloc(available, sizeof(*list.pDrmFormatModifierProperties));
    if (!list.pDrmFormatModifierProperties) return -1;
    gpu->formats(gpu->physical, format, &properties);
    *count = 0;
    for (uint32_t i = 0; i < available && i < list.drmFormatModifierCount && *count < capacity; ++i) {
        uint32_t planes = 0;
        uint64_t modifier = list.pDrmFormatModifierProperties[i].drmFormatModifier;
        /* Query the largest supported stream geometry here. Per-stream
         * enumeration subsequently filters these candidates to its extent. */
        if (am_pyrowave_gpu_supports(gpu, fourcc, modifier, 3840, 2160, &planes) == 0) modifiers[(*count)++] = modifier;
    }
    free(list.pDrmFormatModifierProperties);
    return 0;
}
int am_pyrowave_gpu_supports(struct am_pyrowave_gpu *gpu, uint32_t fourcc, uint64_t modifier, uint32_t width, uint32_t height, uint32_t *planes) {
    VkFormat format = pixel_format(fourcc);
    if (!gpu || !planes || format == VK_FORMAT_UNDEFINED || !width || !height) return -1;
    /* NVIDIA can report large block-height modifiers for small images even
     * though GBM/Vulkan round trips disagree on that layout. Only admit a
     * block height that fits the actual presentation; do not advertise the
     * queried format tuple alone as proof of a usable allocation. */
    if ((modifier >> 56) == 3 && (modifier & 0x10) && height < (8u << (modifier & 0xf))) return -1;
    VkDrmFormatModifierPropertiesListEXT list = { .sType = VK_STRUCTURE_TYPE_DRM_FORMAT_MODIFIER_PROPERTIES_LIST_EXT };
    VkFormatProperties2 properties = { .sType = VK_STRUCTURE_TYPE_FORMAT_PROPERTIES_2, .pNext = &list };
    gpu->formats(gpu->physical, format, &properties);
    if (!list.drmFormatModifierCount || list.drmFormatModifierCount > 4096) return -1;
    uint32_t capacity = list.drmFormatModifierCount;
    list.pDrmFormatModifierProperties = calloc(capacity, sizeof(*list.pDrmFormatModifierProperties));
    if (!list.pDrmFormatModifierProperties) return -1;
    gpu->formats(gpu->physical, format, &properties);
    uint32_t count = 0;
    for (uint32_t i = 0; i < list.drmFormatModifierCount && i < capacity; ++i) {
        const VkDrmFormatModifierPropertiesEXT *entry = &list.pDrmFormatModifierProperties[i];
        if (entry->drmFormatModifier == modifier && (entry->drmFormatModifierTilingFeatures & VK_FORMAT_FEATURE_SAMPLED_IMAGE_BIT)) count = entry->drmFormatModifierPlaneCount;
    }
    free(list.pDrmFormatModifierProperties);
    if (!count || count > 4) return -1;
    VkPhysicalDeviceExternalImageFormatInfo external = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_IMAGE_FORMAT_INFO, .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT };
    VkPhysicalDeviceImageDrmFormatModifierInfoEXT drm = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_DRM_FORMAT_MODIFIER_INFO_EXT, .pNext = &external, .drmFormatModifier = modifier, .sharingMode = VK_SHARING_MODE_EXCLUSIVE };
    VkPhysicalDeviceImageFormatInfo2 info = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2, .pNext = &drm, .format = format, .type = VK_IMAGE_TYPE_2D, .tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT, .usage = VK_IMAGE_USAGE_SAMPLED_BIT };
    VkExternalImageFormatProperties external_properties = { .sType = VK_STRUCTURE_TYPE_EXTERNAL_IMAGE_FORMAT_PROPERTIES };
    VkImageFormatProperties2 limits = { .sType = VK_STRUCTURE_TYPE_IMAGE_FORMAT_PROPERTIES_2, .pNext = &external_properties };
    if (gpu->images(gpu->physical, &info, &limits) != VK_SUCCESS || !(external_properties.externalMemoryProperties.externalMemoryFeatures & VK_EXTERNAL_MEMORY_FEATURE_IMPORTABLE_BIT) || width > limits.imageFormatProperties.maxExtent.width || height > limits.imageFormatProperties.maxExtent.height) return -1;
    *planes = count;
    return 0;
}
static uint64_t monotonic_ms(void) {
    struct timespec time;
    if (clock_gettime(CLOCK_MONOTONIC,&time)!=0) return 0;
    return (uint64_t)time.tv_sec*1000+(uint64_t)time.tv_nsec/1000000;
}
static int wait_fence(int fd) {
    if (fd < 0) return 0;
    uint64_t deadline=monotonic_ms()+2000;
    for (;;) {
        struct sync_file_info info={0};
        if (ioctl(fd,SYNC_IOC_FILE_INFO,&info)!=0 || info.status<0) return -1;
        if (info.status==1) return 0;
        uint64_t now=monotonic_ms();
        if (!now || now>=deadline) return -1;
        struct pollfd pollfd={.fd=fd,.events=POLLIN};
        int result=poll(&pollfd,1,(int)(deadline-now));
        if (result<0 && errno==EINTR) continue;
        if (result<=0 || pollfd.revents&(POLLERR|POLLHUP|POLLNVAL)) return -1;
    }
}

int am_pyrowave_gpu_import(struct am_pyrowave_gpu *gpu, const struct am_pyrowave_gpu_frame *frame, struct am_pyrowave_gpu_image **output) {
    if (!gpu || !frame || !output) return -1;
    *output = NULL;
    uint32_t count = 0;
    if (am_pyrowave_gpu_supports(gpu, frame->fourcc, frame->modifier, frame->width, frame->height, &count) != 0 || count != frame->plane_count) return -1;
    VkFormat format = pixel_format(frame->fourcc);
    if (frame->hdr && format != VK_FORMAT_A2R10G10B10_UNORM_PACK32 && format != VK_FORMAT_A2B10G10R10_UNORM_PACK32) return -1;
    struct stat backing;
    if (frame->fds[0] < 0 || fstat(frame->fds[0], &backing) != 0) return -1;
    VkSubresourceLayout planes[4] = {{0}};
    for (uint32_t i = 0; i < count; ++i) {
        struct stat candidate;
        if (frame->fds[i] < 0 || !frame->pitches[i] || fstat(frame->fds[i], &candidate) != 0 || candidate.st_dev != backing.st_dev || candidate.st_ino != backing.st_ino || wait_fence(frame->fences[i]) != 0) return -1;
        planes[i].offset = frame->offsets[i];
        planes[i].rowPitch = frame->pitches[i];
    }
    struct am_pyrowave_gpu_image *image = calloc(1, sizeof(*image));
    if (!image) return -1;
    image->gpu = gpu;
    VkImageDrmFormatModifierExplicitCreateInfoEXT modifier = { .sType = VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT, .drmFormatModifier = frame->modifier, .drmFormatModifierPlaneCount = count, .pPlaneLayouts = planes };
    VkImageCreateInfo vk_image = { .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO, .pNext = &modifier, .imageType = VK_IMAGE_TYPE_2D, .format = format, .extent = { frame->width, frame->height, 1 }, .mipLevels = 1, .arrayLayers = 1, .samples = VK_SAMPLE_COUNT_1_BIT, .tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT, .usage = VK_IMAGE_USAGE_SAMPLED_BIT, .sharingMode = VK_SHARING_MODE_EXCLUSIVE, .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED };
    int duplicate = fcntl(frame->fds[0], F_DUPFD_CLOEXEC, 0);
    if (duplicate < 0) { free(image); return -1; }
    pyrowave_image_create_info info = { .device = gpu->device, .external_handle = (uintptr_t)duplicate, .handle_type = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT, .image_create_info = &vk_image };
    /* The required native-library marker guarantees import consumes our private
     * fd only on success, including failures after Vulkan memory allocation. */
    if (gpu->api.image_create(&info, &image->image) != PYROWAVE_SUCCESS) { close(duplicate); free(image); return -1; }
    if (gpu->api.image_view(image->image, VK_IMAGE_ASPECT_COLOR_BIT, VK_IMAGE_USAGE_SAMPLED_BIT, &image->scaling.view) != PYROWAVE_SUCCESS) { am_pyrowave_gpu_retire(image); return -1; }
    image->scaling.input_color_space = frame->hdr ? VK_COLOR_SPACE_HDR10_ST2084_EXT : VK_COLOR_SPACE_SRGB_NONLINEAR_KHR;
    image->scaling.output_color_space = image->scaling.input_color_space;
    image->scaling.intermediate_plane_format = frame->hdr ? VK_FORMAT_R16_UNORM : VK_FORMAT_R8_UNORM;
    image->scaling.ycbcr_chroma_midpoint = frame->hdr ? 512.0f / 1023.0f : 128.0f / 255.0f;
    image->scaling.skip_dither = true;
    *output = image;
    return 0;
}
int am_pyrowave_gpu_encode(struct am_pyrowave_gpu_image *image, void *encoder, size_t budget) {
    if (!image || !encoder) return -1;
    pyrowave_gpu_external_reference reference = { .image = image->image, .queue_family_index = VK_QUEUE_FAMILY_FOREIGN_EXT };
    pyrowave_gpu_sync_operation sync = { .images = &reference, .num_images = 1 };
    pyrowave_rate_control rate = { .maximum_bitstream_size = budget };
    return image->gpu->api.encode_scaled(encoder, &sync, &sync, &image->scaling, &rate) == PYROWAVE_SUCCESS ? 0 : -1;
}
void am_pyrowave_gpu_retire(struct am_pyrowave_gpu_image *image) {
    if (image) {
        /* Even a failed conversion may have submitted work. Never retire the
         * capture borrow until all possible reads are independently complete. */
        image->gpu->api.wait_idle(image->gpu->device);
        if (image->gpu->wait_idle(image->gpu->vk_device) != VK_SUCCESS) abort();
        image->gpu->api.image_destroy(image->image);
        free(image);
    }
}
