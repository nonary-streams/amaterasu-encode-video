/*
 * EGL display selection. The display must live on the same GPU as the CUDA
 * context so CUDA can register the converter's GL textures and the converter
 * can read DMA-BUFs allocated on that GPU.
 *
 * Preferred: EGL_EXT_platform_device on the EGL device whose
 * EGL_CUDA_DEVICE_NV equals the encoder's CUdevice. When that display cannot
 * import DMA-BUFs, the same EGL device's DRM render node is opened through
 * GBM (EGL_KHR_platform_gbm), which names the identical GPU.
 */
#include "internal.h"

#include <fcntl.h>
#include <pthread.h>
#include <string.h>
#include <unistd.h>

#define AM_MAX_EGL_DEVICES 16

static struct am_egl_display cached_displays[AM_NVCUDA_MAX_DEVICES];
static uint32_t cached_display_count;
static pthread_mutex_t display_lock = PTHREAD_MUTEX_INITIALIZER;

/* Extension strings are space-separated tokens; a substring match is not
 * enough because one name can prefix another. */
static int has_token(const char *list, const char *token) {
    if (list == NULL) {
        return 0;
    }
    size_t length = strlen(token);
    for (const char *cursor = list; (cursor = strstr(cursor, token)) != NULL; cursor += length) {
        int starts = cursor == list || cursor[-1] == ' ';
        int ends = cursor[length] == '\0' || cursor[length] == ' ';
        if (starts && ends) {
            return 1;
        }
    }
    return 0;
}

static EGLDeviceEXT find_cuda_egl_device(const struct am_egl_api *egl, CUdevice device) {
    EGLDeviceEXT devices[AM_MAX_EGL_DEVICES];
    EGLint count = 0;
    if (!egl->query_devices(AM_MAX_EGL_DEVICES, devices, &count)) {
        return EGL_NO_DEVICE_EXT;
    }
    for (EGLint index = 0; index < count && index < AM_MAX_EGL_DEVICES; ++index) {
        const char *extensions = egl->query_device_string != NULL
            ? egl->query_device_string(devices[index], EGL_EXTENSIONS)
            : NULL;
        if (!has_token(extensions, "EGL_NV_device_cuda")) {
            continue;
        }
        EGLAttrib cuda_device = -1;
        if (egl->query_device_attrib(devices[index], EGL_CUDA_DEVICE_NV, &cuda_device) &&
            cuda_device == (EGLAttrib)device) {
            return devices[index];
        }
    }
    return EGL_NO_DEVICE_EXT;
}

static int initialize_display(const struct am_egl_api *egl, EGLDisplay display) {
    EGLint major = 0;
    EGLint minor = 0;
    return display != EGL_NO_DISPLAY && egl->initialize(display, &major, &minor);
}

/* Records what this display can do. Returns 0 if it cannot host the GL
 * converter at all (no desktop GL or no surfaceless context). */
static int describe_display(const struct am_egl_api *egl, EGLDisplay display,
                            struct am_egl_display *description) {
    const char *extensions = egl->query_string(display, EGL_EXTENSIONS);
    const char *apis = egl->query_string(display, EGL_CLIENT_APIS);
    if (!has_token(apis, "OpenGL") || !has_token(extensions, "EGL_KHR_surfaceless_context")) {
        return 0;
    }
    description->display = display;
    description->dmabuf_import = has_token(extensions, "EGL_EXT_image_dma_buf_import") &&
                                 has_token(extensions, "EGL_KHR_image_base") &&
                                 egl->create_image != NULL && egl->destroy_image != NULL;
    description->dmabuf_modifiers =
        description->dmabuf_import &&
        has_token(extensions, "EGL_EXT_image_dma_buf_import_modifiers") &&
        egl->query_dmabuf_formats != NULL && egl->query_dmabuf_modifiers != NULL;
    description->native_fence_sync = has_token(extensions, "EGL_ANDROID_native_fence_sync") &&
                                     egl->create_sync != NULL && egl->destroy_sync != NULL;
    description->wait_sync = has_token(extensions, "EGL_KHR_wait_sync") && egl->wait_sync != NULL;
    description->no_config_context = has_token(extensions, "EGL_KHR_no_config_context");
    return 1;
}

/* Opens the device's render node through GBM and initializes an EGL display
 * on it. On success the descriptor and GBM device are kept for the process
 * lifetime with the display; on failure both are released here. */
static EGLDisplay open_gbm_display(const struct am_driver *driver, EGLDeviceEXT device) {
    const struct am_egl_api *egl = &driver->egl;
    if (!driver->gbm.available || egl->query_device_string == NULL) {
        return EGL_NO_DISPLAY;
    }
    const char *device_extensions = egl->query_device_string(device, EGL_EXTENSIONS);
    if (!has_token(device_extensions, "EGL_EXT_device_drm_render_node")) {
        return EGL_NO_DISPLAY;
    }
    const char *render_node = egl->query_device_string(device, EGL_DRM_RENDER_NODE_FILE_EXT);
    if (render_node == NULL) {
        return EGL_NO_DISPLAY;
    }
    int render_fd = open(render_node, O_RDWR | O_CLOEXEC);
    if (render_fd < 0) {
        return EGL_NO_DISPLAY;
    }
    void *gbm_device = driver->gbm.create_device(render_fd);
    if (gbm_device == NULL) {
        close(render_fd);
        return EGL_NO_DISPLAY;
    }
    EGLDisplay display = egl->get_platform_display(EGL_PLATFORM_GBM_KHR, gbm_device, NULL);
    if (!initialize_display(egl, display)) {
        driver->gbm.destroy_device(gbm_device);
        close(render_fd);
        return EGL_NO_DISPLAY;
    }
    return display;
}

static int create_display(const struct am_driver *driver, CUdevice device,
                          struct am_egl_display *description,
                          struct am_nvcuda_status *status) {
    const struct am_egl_api *egl = &driver->egl;
    const char *client_extensions = egl->query_string(EGL_NO_DISPLAY, EGL_EXTENSIONS);
    if (!has_token(client_extensions, "EGL_EXT_platform_device")) {
        am_set_status(status, AM_STAGE_EGL_DISPLAY, 0);
        return AM_NVCUDA_CONVERTER_UNAVAILABLE;
    }
    EGLDeviceEXT egl_device = find_cuda_egl_device(egl, device);
    if (egl_device == EGL_NO_DEVICE_EXT) {
        am_set_status(status, AM_STAGE_EGL_DISPLAY, (int32_t)egl->get_error());
        return AM_NVCUDA_CONVERTER_UNAVAILABLE;
    }
    memset(description, 0, sizeof(*description));
    description->cuda_device = device;

    EGLDisplay platform_display = egl->get_platform_display(EGL_PLATFORM_DEVICE_EXT, egl_device,
                                                            NULL);
    int device_usable = initialize_display(egl, platform_display) &&
                        describe_display(egl, platform_display, description);
    if (device_usable && description->dmabuf_import) {
        return AM_NVCUDA_OK;
    }

    struct am_egl_display gbm_description;
    memset(&gbm_description, 0, sizeof(gbm_description));
    gbm_description.cuda_device = device;
    gbm_description.uses_gbm = 1;
    EGLDisplay gbm_display = open_gbm_display(driver, egl_device);
    if (gbm_display != EGL_NO_DISPLAY &&
        describe_display(egl, gbm_display, &gbm_description) && gbm_description.dmabuf_import) {
        *description = gbm_description;
        return AM_NVCUDA_OK;
    }
    if (device_usable) {
        /* GPU conversion of CPU frames still works without DMA-BUF import. */
        return AM_NVCUDA_OK;
    }
    am_set_status(status, AM_STAGE_EGL_DISPLAY, (int32_t)egl->get_error());
    return AM_NVCUDA_CONVERTER_UNAVAILABLE;
}

int am_egl_display_for_device(const struct am_driver *driver, CUdevice device,
                              const struct am_egl_display **display,
                              struct am_nvcuda_status *status) {
    *display = NULL;
    if (!driver->egl.available) {
        am_set_status(status, AM_STAGE_LIBRARY, 0);
        return AM_NVCUDA_CONVERTER_UNAVAILABLE;
    }
    pthread_mutex_lock(&display_lock);
    for (uint32_t index = 0; index < cached_display_count; ++index) {
        if (cached_displays[index].cuda_device == device) {
            *display = &cached_displays[index];
            pthread_mutex_unlock(&display_lock);
            return AM_NVCUDA_OK;
        }
    }
    int result = AM_NVCUDA_CONVERTER_UNAVAILABLE;
    if (cached_display_count < AM_NVCUDA_MAX_DEVICES) {
        struct am_egl_display *slot = &cached_displays[cached_display_count];
        result = create_display(driver, device, slot, status);
        if (result == AM_NVCUDA_OK) {
            ++cached_display_count;
            *display = slot;
        }
    }
    pthread_mutex_unlock(&display_lock);
    return result;
}
