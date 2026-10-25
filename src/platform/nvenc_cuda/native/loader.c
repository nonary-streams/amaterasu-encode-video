/*
 * Process-wide loading of the NVIDIA, EGL and GBM runtime libraries. Every
 * GPU library is opened with dlopen so the encoder module still loads on a
 * machine without them; the backend then reports itself unavailable.
 */
#include "internal.h"

#include <dlfcn.h>
#include <pthread.h>
#include <string.h>

void am_set_status(struct am_nvcuda_status *status, uint32_t stage, int32_t native_code) {
    if (status != NULL) {
        status->stage = stage;
        status->native_code = native_code;
    }
}

static int load_symbol(void *library, const char *name, void *destination) {
    void *symbol = dlsym(library, name);
    if (symbol == NULL) {
        return 0;
    }
    /* POSIX guarantees object and function pointers share a representation. */
    memcpy(destination, &symbol, sizeof(symbol));
    return 1;
}

static int load_cuda(struct am_cuda_api *cuda, void *library) {
    return load_symbol(library, "cuInit", &cuda->init) &&
           load_symbol(library, "cuDeviceGetCount", &cuda->device_get_count) &&
           load_symbol(library, "cuDeviceGet", &cuda->device_get) &&
           load_symbol(library, "cuDeviceGetName", &cuda->device_get_name) &&
           load_symbol(library, "cuDeviceGetPCIBusId", &cuda->device_get_pci_bus_id) &&
           load_symbol(library, "cuCtxCreate_v2", &cuda->ctx_create) &&
           load_symbol(library, "cuCtxDestroy_v2", &cuda->ctx_destroy) &&
           load_symbol(library, "cuCtxSynchronize", &cuda->ctx_synchronize) &&
           load_symbol(library, "cuMemAllocPitch_v2", &cuda->mem_alloc_pitch) &&
           load_symbol(library, "cuMemFree_v2", &cuda->mem_free) &&
           load_symbol(library, "cuMemcpy2D_v2", &cuda->memcpy_2d) &&
           load_symbol(library, "cuGraphicsGLRegisterImage", &cuda->graphics_gl_register_image) &&
           load_symbol(library, "cuGraphicsUnregisterResource", &cuda->graphics_unregister) &&
           load_symbol(library, "cuGraphicsMapResources", &cuda->graphics_map) &&
           load_symbol(library, "cuGraphicsUnmapResources", &cuda->graphics_unmap) &&
           load_symbol(library, "cuGraphicsSubResourceGetMappedArray",
                       &cuda->graphics_mapped_array);
}

/* A missing extension leaves its entry NULL; users check before calling. */
#define AM_EGL_EXTENSION(egl, field, name) \
    do { \
        __eglMustCastToProperFunctionPointerType pointer = (egl)->get_proc_address(name); \
        memcpy(&(egl)->field, &pointer, sizeof(pointer)); \
    } while (0)

/* EGL is optional at load time: without it the encoder cannot convert, and
 * enumeration reports no usable device rather than failing CUDA discovery. */
static void load_egl(struct am_egl_api *egl) {
    void *library = dlopen("libEGL.so.1", RTLD_NOW | RTLD_LOCAL);
    if (library == NULL) {
        return;
    }
    if (!load_symbol(library, "eglGetProcAddress", &egl->get_proc_address) ||
        !load_symbol(library, "eglGetError", &egl->get_error) ||
        !load_symbol(library, "eglInitialize", &egl->initialize) ||
        !load_symbol(library, "eglQueryString", &egl->query_string) ||
        !load_symbol(library, "eglBindAPI", &egl->bind_api) ||
        !load_symbol(library, "eglChooseConfig", &egl->choose_config) ||
        !load_symbol(library, "eglCreateContext", &egl->create_context) ||
        !load_symbol(library, "eglDestroyContext", &egl->destroy_context) ||
        !load_symbol(library, "eglMakeCurrent", &egl->make_current)) {
        return;
    }
    AM_EGL_EXTENSION(egl, query_devices, "eglQueryDevicesEXT");
    AM_EGL_EXTENSION(egl, query_device_attrib, "eglQueryDeviceAttribEXT");
    AM_EGL_EXTENSION(egl, query_device_string, "eglQueryDeviceStringEXT");
    AM_EGL_EXTENSION(egl, get_platform_display, "eglGetPlatformDisplayEXT");
    AM_EGL_EXTENSION(egl, query_dmabuf_formats, "eglQueryDmaBufFormatsEXT");
    AM_EGL_EXTENSION(egl, query_dmabuf_modifiers, "eglQueryDmaBufModifiersEXT");
    AM_EGL_EXTENSION(egl, create_image, "eglCreateImageKHR");
    AM_EGL_EXTENSION(egl, destroy_image, "eglDestroyImageKHR");
    AM_EGL_EXTENSION(egl, create_sync, "eglCreateSyncKHR");
    AM_EGL_EXTENSION(egl, destroy_sync, "eglDestroySyncKHR");
    AM_EGL_EXTENSION(egl, wait_sync, "eglWaitSyncKHR");
    egl->available = egl->query_devices != NULL && egl->query_device_attrib != NULL &&
                     egl->get_platform_display != NULL;
}

/* GBM only provides a fallback EGL platform for DMA-BUF import. */
static void load_gbm(struct am_driver *driver) {
    driver->gbm_library = dlopen("libgbm.so.1", RTLD_NOW | RTLD_LOCAL);
    if (driver->gbm_library == NULL) {
        return;
    }
    driver->gbm.available =
        load_symbol(driver->gbm_library, "gbm_create_device", &driver->gbm.create_device) &&
        load_symbol(driver->gbm_library, "gbm_device_destroy", &driver->gbm.destroy_device);
}

static struct am_driver loaded_driver;
static struct am_nvcuda_status load_status;
static int load_result = AM_NVCUDA_LIBRARY_UNAVAILABLE;
static pthread_once_t load_once = PTHREAD_ONCE_INIT;

static void load_driver_once(void) {
    void *cuda = dlopen("libcuda.so.1", RTLD_NOW | RTLD_LOCAL);
    void *nvenc = dlopen("libnvidia-encode.so.1", RTLD_NOW | RTLD_LOCAL);
    if (cuda == NULL || nvenc == NULL) {
        am_set_status(&load_status, AM_STAGE_LIBRARY, 0);
        load_result = AM_NVCUDA_LIBRARY_UNAVAILABLE;
        return;
    }
    if (!load_cuda(&loaded_driver.cuda, cuda) ||
        !load_symbol(nvenc, "NvEncodeAPICreateInstance", &loaded_driver.nvenc_create_instance) ||
        !load_symbol(nvenc, "NvEncodeAPIGetMaxSupportedVersion",
                     &loaded_driver.nvenc_max_version)) {
        am_set_status(&load_status, AM_STAGE_DRIVER, 0);
        load_result = AM_NVCUDA_DRIVER_UNAVAILABLE;
        return;
    }
    CUresult cuda_status = loaded_driver.cuda.init(0);
    if (cuda_status != CUDA_SUCCESS) {
        am_set_status(&load_status, AM_STAGE_DRIVER, (int32_t)cuda_status);
        load_result = AM_NVCUDA_DRIVER_UNAVAILABLE;
        return;
    }
    load_egl(&loaded_driver.egl);
    load_gbm(&loaded_driver);
    am_set_status(&load_status, AM_STAGE_NONE, 0);
    load_result = AM_NVCUDA_OK;
}

const struct am_driver *am_driver_load(struct am_nvcuda_status *status, int *result) {
    pthread_once(&load_once, load_driver_once);
    if (status != NULL) {
        *status = load_status;
    }
    *result = load_result;
    return load_result == AM_NVCUDA_OK ? &loaded_driver : NULL;
}

int amaterasu_nvcuda_driver_api(uint32_t *packed_version, struct am_nvcuda_status *status) {
    if (packed_version == NULL) {
        return AM_NVCUDA_INVALID_ARGUMENT;
    }
    int result = AM_NVCUDA_OK;
    const struct am_driver *driver = am_driver_load(status, &result);
    if (driver == NULL) {
        return result;
    }
    NVENCSTATUS native_status = driver->nvenc_max_version(packed_version);
    if (native_status != NV_ENC_SUCCESS) {
        am_set_status(status, AM_STAGE_API, (int32_t)native_status);
        return AM_NVCUDA_API_UNSUPPORTED;
    }
    return AM_NVCUDA_OK;
}
