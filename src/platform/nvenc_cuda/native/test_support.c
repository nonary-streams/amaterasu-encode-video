/*
 * Support for the ignored GPU tests only: allocates an XRGB8888 GBM buffer in
 * the GPU's preferred (on NVIDIA, block-linear) layout, the same kind of
 * buffer KWin scans out on a vibeshine_drm output, fills its two halves by
 * GPU clears through EGL, and exports it as a DMA-BUF. Tiled buffers cannot be
 * CPU-mapped, and NVIDIA imports linear buffers only as external-only images.
 * Production code never calls these functions. libgbm and libEGL are loaded
 * on first use.
 */
#include "internal.h"

#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <dlfcn.h>
#include <fcntl.h>
#include <gbm.h>
#include <string.h>
#include <unistd.h>

/* GPU-free test of the conversion handoff's stream selection and ordering. */
static struct {
    CUstream expected;
    int map, unmap, copies, sync, failed, fail_copy;
} am_test_copy;
static CUresult CUDAAPI am_test_map(unsigned int count, CUgraphicsResource *resources, CUstream stream) {
    (void)resources;
    am_test_copy.map++;
    if (count != 2 || stream != am_test_copy.expected) am_test_copy.failed = 1;
    return CUDA_SUCCESS;
}
static CUresult CUDAAPI am_test_unmap(unsigned int count, CUgraphicsResource *resources, CUstream stream) {
    (void)resources;
    am_test_copy.unmap++;
    if (count != 2 || stream != am_test_copy.expected) am_test_copy.failed = 1;
    return am_test_copy.fail_copy == 2 ? CUDA_ERROR_INVALID_VALUE : CUDA_SUCCESS;
}
static CUresult CUDAAPI am_test_array(CUarray *array, CUgraphicsResource resource, unsigned int index, unsigned int level) {
    (void)resource; (void)index; (void)level;
    *array = (CUarray)(uintptr_t)1;
    return CUDA_SUCCESS;
}
static CUresult CUDAAPI am_test_copy_sync(const CUDA_MEMCPY2D *copy) {
    (void)copy;
    am_test_copy.copies++;
    if (am_test_copy.expected != NULL) am_test_copy.failed = 1;
    return (am_test_copy.fail_copy == 1 ||
            (am_test_copy.fail_copy == 3 && am_test_copy.copies == 2))
        ? CUDA_ERROR_INVALID_VALUE : CUDA_SUCCESS;
}
static CUresult CUDAAPI am_test_copy_async(const CUDA_MEMCPY2D *copy, CUstream stream) {
    (void)copy;
    am_test_copy.copies++;
    if (stream != am_test_copy.expected || stream == NULL) am_test_copy.failed = 1;
    return (am_test_copy.fail_copy == 1 ||
            (am_test_copy.fail_copy == 3 && am_test_copy.copies == 2))
        ? CUDA_ERROR_INVALID_VALUE : CUDA_SUCCESS;
}
static CUresult CUDAAPI am_test_context_sync(void) {
    am_test_copy.sync++;
    return am_test_copy.fail_copy == 4 ? CUDA_ERROR_INVALID_VALUE : CUDA_SUCCESS;
}
int amaterasu_nvcuda_test_conversion_stream(int asynchronous, int fail_copy) {
    memset(&am_test_copy, 0, sizeof(am_test_copy));
    am_test_copy.expected = asynchronous ? (CUstream)(uintptr_t)1 : NULL;
    am_test_copy.fail_copy = fail_copy;
    struct am_driver driver = {0};
    driver.cuda.graphics_map = am_test_map;
    driver.cuda.graphics_unmap = am_test_unmap;
    driver.cuda.graphics_mapped_array = am_test_array;
    driver.cuda.memcpy_2d = am_test_copy_sync;
    driver.cuda.memcpy_2d_async = am_test_copy_async;
    driver.cuda.ctx_synchronize = am_test_context_sync;
    struct am_converter converter = {0};
    converter.driver = &driver;
    converter.width = 640; converter.height = 360; converter.bit_depth = 8;
    struct am_nvcuda_status status = {0};
    int result = am_converter_copy_nv12(&converter, 1, 640, am_test_copy.expected, &status);
    if (am_test_copy.failed || am_test_copy.map != 1 || am_test_copy.unmap != 1 ||
        am_test_copy.copies != (fail_copy == 1 ? 1 : 2) || am_test_copy.sync != (asynchronous ? 0 : 1) ||
        converter.mapping_outstanding != (fail_copy == 2) ||
        converter.retirement_unproven != (fail_copy == 4 && !asynchronous)) return 0;
    return fail_copy ? result == AM_NVCUDA_NATIVE_FAILURE : result == AM_NVCUDA_OK;
}

struct am_test_gbm_api {
    struct gbm_device *(*create_device)(int fd);
    void (*device_destroy)(struct gbm_device *device);
    struct gbm_bo *(*bo_create)(struct gbm_device *device, uint32_t width, uint32_t height,
                                uint32_t format, uint32_t flags);
    void (*bo_destroy)(struct gbm_bo *buffer);
    int (*bo_get_fd)(struct gbm_bo *buffer);
    uint32_t (*bo_get_stride)(struct gbm_bo *buffer);
    uint32_t (*bo_get_offset)(struct gbm_bo *buffer, int plane);
    uint64_t (*bo_get_modifier)(struct gbm_bo *buffer);
};

/* The handful of EGL and GLES 2 entry points the filler needs. */
struct am_test_egl_api {
    __eglMustCastToProperFunctionPointerType (*get_proc_address)(const char *name);
    EGLDisplay (*get_platform_display)(EGLenum platform, void *native, const EGLAttrib *attributes);
    EGLBoolean (*initialize)(EGLDisplay display, EGLint *major, EGLint *minor);
    EGLBoolean (*terminate)(EGLDisplay display);
    EGLBoolean (*bind_api)(EGLenum api);
    EGLContext (*create_context)(EGLDisplay display, EGLConfig config, EGLContext share,
                                 const EGLint *attributes);
    EGLBoolean (*destroy_context)(EGLDisplay display, EGLContext context);
    EGLBoolean (*make_current)(EGLDisplay display, EGLSurface draw, EGLSurface read,
                               EGLContext context);
    EGLImage (*create_image)(EGLDisplay display, EGLContext context, EGLenum target,
                             EGLClientBuffer buffer, const EGLAttrib *attributes);
    EGLBoolean (*destroy_image)(EGLDisplay display, EGLImage image);
    void (*gen_textures)(int count, unsigned *textures);
    void (*delete_textures)(int count, const unsigned *textures);
    void (*bind_texture)(unsigned target, unsigned texture);
    void (*image_target_texture)(unsigned target, void *image);
    void (*gen_framebuffers)(int count, unsigned *framebuffers);
    void (*delete_framebuffers)(int count, const unsigned *framebuffers);
    void (*bind_framebuffer)(unsigned target, unsigned framebuffer);
    void (*framebuffer_texture)(unsigned target, unsigned attachment, unsigned texture_target,
                                unsigned texture, int level);
    unsigned (*check_framebuffer_status)(unsigned target);
    void (*enable)(unsigned capability);
    void (*scissor)(int x, int y, int width, int height);
    void (*clear_color)(float red, float green, float blue, float alpha);
    void (*clear)(unsigned mask);
    void (*finish)(void);
};

enum {
    AM_TEST_GL_TEXTURE_2D = 0x0DE1,
    AM_TEST_GL_FRAMEBUFFER = 0x8D40,
    AM_TEST_GL_COLOR_ATTACHMENT0 = 0x8CE0,
    AM_TEST_GL_FRAMEBUFFER_COMPLETE = 0x8CD5,
    AM_TEST_GL_SCISSOR_TEST = 0x0C11,
    AM_TEST_GL_COLOR_BUFFER_BIT = 0x4000,
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
           load(library, "gbm_bo_get_fd", &gbm->bo_get_fd) &&
           load(library, "gbm_bo_get_stride", &gbm->bo_get_stride) &&
           load(library, "gbm_bo_get_offset", &gbm->bo_get_offset) &&
           load(library, "gbm_bo_get_modifier", &gbm->bo_get_modifier);
}

static int load_gl(struct am_test_egl_api *egl, const char *name, void *destination) {
    __eglMustCastToProperFunctionPointerType symbol = egl->get_proc_address(name);
    memcpy(destination, &symbol, sizeof(symbol));
    return symbol != NULL;
}

static int load_test_egl(struct am_test_egl_api *egl) {
    void *library = dlopen("libEGL.so.1", RTLD_NOW | RTLD_LOCAL);
    return library != NULL && load(library, "eglGetProcAddress", &egl->get_proc_address) &&
           load(library, "eglGetPlatformDisplay", &egl->get_platform_display) &&
           load(library, "eglInitialize", &egl->initialize) &&
           load(library, "eglTerminate", &egl->terminate) &&
           load(library, "eglBindAPI", &egl->bind_api) &&
           load(library, "eglCreateContext", &egl->create_context) &&
           load(library, "eglDestroyContext", &egl->destroy_context) &&
           load(library, "eglMakeCurrent", &egl->make_current) &&
           load(library, "eglCreateImage", &egl->create_image) &&
           load(library, "eglDestroyImage", &egl->destroy_image) &&
           load_gl(egl, "glGenTextures", &egl->gen_textures) &&
           load_gl(egl, "glDeleteTextures", &egl->delete_textures) &&
           load_gl(egl, "glBindTexture", &egl->bind_texture) &&
           load_gl(egl, "glEGLImageTargetTexture2DOES", &egl->image_target_texture) &&
           load_gl(egl, "glGenFramebuffers", &egl->gen_framebuffers) &&
           load_gl(egl, "glDeleteFramebuffers", &egl->delete_framebuffers) &&
           load_gl(egl, "glBindFramebuffer", &egl->bind_framebuffer) &&
           load_gl(egl, "glFramebufferTexture2D", &egl->framebuffer_texture) &&
           load_gl(egl, "glCheckFramebufferStatus", &egl->check_framebuffer_status) &&
           load_gl(egl, "glEnable", &egl->enable) && load_gl(egl, "glScissor", &egl->scissor) &&
           load_gl(egl, "glClearColor", &egl->clear_color) &&
           load_gl(egl, "glClear", &egl->clear) && load_gl(egl, "glFinish", &egl->finish);
}

static struct am_test_egl_api egl_api;

/* Clears image rows [0, rows). A texture attached to a framebuffer keeps GL
 * row 0 at the first row of its memory, which is image row 0 of the DMA-BUF. */
static void clear_leading_rows(const struct am_test_egl_api *egl, uint32_t width, uint32_t rows,
                               uint32_t xrgb, uint32_t format) {
    egl->scissor(0, 0, (int)width, (int)rows);
    if (format == GBM_FORMAT_ARGB2101010) {
        egl->clear_color((float)((xrgb >> 20) & 1023) / 1023.0f, (float)((xrgb >> 10) & 1023) / 1023.0f,
                         (float)(xrgb & 1023) / 1023.0f, 1.0f);
    } else {
        egl->clear_color((float)((xrgb >> 16) & 0xff) / 255.0f, (float)((xrgb >> 8) & 0xff) / 255.0f,
                         (float)(xrgb & 0xff) / 255.0f, 1.0f);
    }
    egl->clear(AM_TEST_GL_COLOR_BUFFER_BIT);
}

/* Renders image rows [0, height/2) as `top_xrgb` and the rest as `bottom_xrgb`. */
static int fill(struct gbm_device *device, const struct am_nvcuda_test_buffer *out,
                uint32_t width, uint32_t height, uint32_t top_xrgb, uint32_t bottom_xrgb, uint32_t format) {
    struct am_test_egl_api *egl = &egl_api;
    if (egl->get_proc_address == NULL && !load_test_egl(egl)) {
        return 0;
    }
    EGLDisplay display = egl->get_platform_display(EGL_PLATFORM_GBM_KHR, device, NULL);
    EGLint major = 0;
    EGLint minor = 0;
    if (display == EGL_NO_DISPLAY || !egl->initialize(display, &major, &minor) ||
        !egl->bind_api(EGL_OPENGL_ES_API)) {
        return 0;
    }
    const EGLint context_attributes[] = {EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE};
    EGLContext context =
        egl->create_context(display, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, context_attributes);
    if (context == EGL_NO_CONTEXT ||
        !egl->make_current(display, EGL_NO_SURFACE, EGL_NO_SURFACE, context)) {
        return 0;
    }
    const EGLAttrib image_attributes[] = {
        EGL_WIDTH, (EGLAttrib)width,
        EGL_HEIGHT, (EGLAttrib)height,
        EGL_LINUX_DRM_FOURCC_EXT, (EGLAttrib)format,
        EGL_DMA_BUF_PLANE0_FD_EXT, out->dmabuf_fd,
        EGL_DMA_BUF_PLANE0_OFFSET_EXT, (EGLAttrib)out->offset,
        EGL_DMA_BUF_PLANE0_PITCH_EXT, (EGLAttrib)out->pitch,
        EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT, (EGLAttrib)(out->modifier & 0xffffffffu),
        EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT, (EGLAttrib)(out->modifier >> 32),
        EGL_NONE,
    };
    EGLImage image =
        egl->create_image(display, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL, image_attributes);
    int filled = 0;
    if (image != EGL_NO_IMAGE) {
        unsigned texture = 0;
        unsigned framebuffer = 0;
        egl->gen_textures(1, &texture);
        egl->bind_texture(AM_TEST_GL_TEXTURE_2D, texture);
        egl->image_target_texture(AM_TEST_GL_TEXTURE_2D, image);
        egl->gen_framebuffers(1, &framebuffer);
        egl->bind_framebuffer(AM_TEST_GL_FRAMEBUFFER, framebuffer);
        egl->framebuffer_texture(AM_TEST_GL_FRAMEBUFFER, AM_TEST_GL_COLOR_ATTACHMENT0,
                                 AM_TEST_GL_TEXTURE_2D, texture, 0);
        if (egl->check_framebuffer_status(AM_TEST_GL_FRAMEBUFFER) ==
            AM_TEST_GL_FRAMEBUFFER_COMPLETE) {
            egl->enable(AM_TEST_GL_SCISSOR_TEST);
            clear_leading_rows(egl, width, height, bottom_xrgb, format);
            clear_leading_rows(egl, width, height / 2, top_xrgb, format);
            egl->finish();
            filled = 1;
        }
        egl->bind_framebuffer(AM_TEST_GL_FRAMEBUFFER, 0);
        egl->delete_framebuffers(1, &framebuffer);
        egl->delete_textures(1, &texture);
        egl->destroy_image(display, image);
    }
    egl->make_current(display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
    egl->destroy_context(display, context);
    egl->terminate(display);
    return filled;
}

/* Loaded on first allocation and kept; only the test process uses it. */
static struct am_test_gbm_api gbm_api;

int amaterasu_nvcuda_test_allocate_gbm(const char *render_node, uint32_t width,
                                       uint32_t height, uint32_t top_xrgb,
                                       uint32_t bottom_xrgb, uint32_t format,
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
        device != NULL
            ? gbm->bo_create(device, width, height, format, GBM_BO_USE_RENDERING)
            : NULL;
    out->device = device;
    out->buffer = buffer;
    if (buffer == NULL) {
        amaterasu_nvcuda_test_release_gbm(out);
        return AM_NVCUDA_ALLOCATION_FAILED;
    }
    out->dmabuf_fd = gbm->bo_get_fd(buffer);
    out->pitch = gbm->bo_get_stride(buffer);
    out->offset = gbm->bo_get_offset(buffer, 0);
    out->modifier = gbm->bo_get_modifier(buffer);
    if (out->dmabuf_fd < 0 ||
        !fill(device, out, width, height, top_xrgb, bottom_xrgb, format)) {
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
