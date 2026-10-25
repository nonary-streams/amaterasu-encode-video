/*
 * Declarations shared by the adapter's translation units. Nothing here is
 * visible to Rust.
 */
#ifndef AMATERASU_NVENC_CUDA_INTERNAL_H
#define AMATERASU_NVENC_CUDA_INTERNAL_H

#include "adapter.h"

#include <dynlink_cuda.h>
#include <nvEncodeAPI.h>

#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GL/glcorearb.h>

/* Failure stages reported through am_nvcuda_status. Rust maps them to text. */
enum am_nvcuda_stage {
    AM_STAGE_NONE = 0,
    AM_STAGE_LIBRARY = 1,
    AM_STAGE_DRIVER = 2,
    AM_STAGE_API = 3,
    AM_STAGE_DEVICE = 4,
    AM_STAGE_SESSION = 5,
    AM_STAGE_CAPS = 6,
    AM_STAGE_PRESET = 7,
    AM_STAGE_INITIALIZE = 8,
    AM_STAGE_INPUT = 9,
    AM_STAGE_BITSTREAM = 10,
    AM_STAGE_ENCODE = 11,
    AM_STAGE_DRAIN = 12,
    AM_STAGE_EGL_DISPLAY = 13,
    AM_STAGE_GL_CONTEXT = 14,
    AM_STAGE_GL_RESOURCES = 15,
    AM_STAGE_CUDA_INTEROP = 16,
    AM_STAGE_UPLOAD = 17,
    AM_STAGE_FENCE = 18,
    AM_STAGE_IMPORT = 19,
    AM_STAGE_CONVERT = 20,
};

void am_set_status(struct am_nvcuda_status *status, uint32_t stage, int32_t native_code);

struct am_cuda_api {
    tcuInit *init;
    tcuDeviceGetCount *device_get_count;
    tcuDeviceGet *device_get;
    tcuDeviceGetName *device_get_name;
    tcuDeviceGetPCIBusId *device_get_pci_bus_id;
    tcuCtxCreate_v2 *ctx_create;
    tcuCtxDestroy_v2 *ctx_destroy;
    tcuCtxSynchronize *ctx_synchronize;
    tcuMemAllocPitch_v2 *mem_alloc_pitch;
    tcuMemFree_v2 *mem_free;
    tcuMemcpy2D_v2 *memcpy_2d;
    tcuGraphicsGLRegisterImage *graphics_gl_register_image;
    tcuGraphicsUnregisterResource *graphics_unregister;
    tcuGraphicsMapResources *graphics_map;
    tcuGraphicsUnmapResources *graphics_unmap;
    tcuGraphicsSubResourceGetMappedArray *graphics_mapped_array;
};

typedef NVENCSTATUS(NVENCAPI *am_nvenc_create_instance_fn)(NV_ENCODE_API_FUNCTION_LIST *list);
typedef NVENCSTATUS(NVENCAPI *am_nvenc_max_version_fn)(uint32_t *version);

/* Core EGL entry points resolved with dlsym; extensions with eglGetProcAddress. */
struct am_egl_api {
    int available;
    PFNEGLGETPROCADDRESSPROC get_proc_address;
    PFNEGLGETERRORPROC get_error;
    PFNEGLINITIALIZEPROC initialize;
    PFNEGLQUERYSTRINGPROC query_string;
    PFNEGLBINDAPIPROC bind_api;
    PFNEGLCHOOSECONFIGPROC choose_config;
    PFNEGLCREATECONTEXTPROC create_context;
    PFNEGLDESTROYCONTEXTPROC destroy_context;
    PFNEGLMAKECURRENTPROC make_current;
    PFNEGLQUERYDEVICESEXTPROC query_devices;
    PFNEGLQUERYDEVICEATTRIBEXTPROC query_device_attrib;
    PFNEGLQUERYDEVICESTRINGEXTPROC query_device_string;
    PFNEGLGETPLATFORMDISPLAYEXTPROC get_platform_display;
    PFNEGLQUERYDMABUFFORMATSEXTPROC query_dmabuf_formats;
    PFNEGLQUERYDMABUFMODIFIERSEXTPROC query_dmabuf_modifiers;
    PFNEGLCREATEIMAGEKHRPROC create_image;
    PFNEGLDESTROYIMAGEKHRPROC destroy_image;
    PFNEGLCREATESYNCKHRPROC create_sync;
    PFNEGLDESTROYSYNCKHRPROC destroy_sync;
    PFNEGLWAITSYNCKHRPROC wait_sync;
};

/* Opaque GBM entry points; signatures follow gbm.h and are cast at use. */
struct am_gbm_api {
    int available;
    void *(*create_device)(int fd);
    void (*destroy_device)(void *device);
};

/* Process-wide driver tables. Loaded once and never unloaded: a driver
 * library cannot be unloaded safely while any thread might still hold an
 * object it created, and keeping it loaded makes every pointer stable. */
struct am_driver {
    struct am_cuda_api cuda;
    am_nvenc_create_instance_fn nvenc_create_instance;
    am_nvenc_max_version_fn nvenc_max_version;
    struct am_egl_api egl;
    struct am_gbm_api gbm;
    void *gbm_library;
};

const struct am_driver *am_driver_load(struct am_nvcuda_status *status, int *result);

/* An initialized EGL display on one CUDA device. Displays are cached for the
 * process lifetime and never terminated, because eglTerminate would destroy
 * the contexts of every other session on the same GPU. */
struct am_egl_display {
    CUdevice cuda_device;
    EGLDisplay display;
    int uses_gbm;
    int dmabuf_import;
    int dmabuf_modifiers;
    int native_fence_sync;
    int wait_sync;
    /* EGL_KHR_no_config_context: offscreen contexts need no EGLConfig. */
    int no_config_context;
};

int am_egl_display_for_device(const struct am_driver *driver, CUdevice device,
                              const struct am_egl_display **display,
                              struct am_nvcuda_status *status);

struct am_gl_api {
    PFNGLGETERRORPROC get_error;
    PFNGLGENTEXTURESPROC gen_textures;
    PFNGLDELETETEXTURESPROC delete_textures;
    PFNGLBINDTEXTUREPROC bind_texture;
    PFNGLTEXPARAMETERIPROC tex_parameteri;
    PFNGLTEXIMAGE2DPROC tex_image_2d;
    PFNGLTEXSUBIMAGE2DPROC tex_sub_image_2d;
    PFNGLPIXELSTOREIPROC pixel_storei;
    PFNGLACTIVETEXTUREPROC active_texture;
    PFNGLGENFRAMEBUFFERSPROC gen_framebuffers;
    PFNGLDELETEFRAMEBUFFERSPROC delete_framebuffers;
    PFNGLBINDFRAMEBUFFERPROC bind_framebuffer;
    PFNGLFRAMEBUFFERTEXTURE2DPROC framebuffer_texture_2d;
    PFNGLCHECKFRAMEBUFFERSTATUSPROC check_framebuffer_status;
    PFNGLVIEWPORTPROC viewport;
    PFNGLCREATESHADERPROC create_shader;
    PFNGLDELETESHADERPROC delete_shader;
    PFNGLSHADERSOURCEPROC shader_source;
    PFNGLCOMPILESHADERPROC compile_shader;
    PFNGLGETSHADERIVPROC get_shaderiv;
    PFNGLCREATEPROGRAMPROC create_program;
    PFNGLDELETEPROGRAMPROC delete_program;
    PFNGLATTACHSHADERPROC attach_shader;
    PFNGLLINKPROGRAMPROC link_program;
    PFNGLGETPROGRAMIVPROC get_programiv;
    PFNGLUSEPROGRAMPROC use_program;
    PFNGLGETUNIFORMLOCATIONPROC get_uniform_location;
    PFNGLUNIFORM1IPROC uniform1i;
    PFNGLUNIFORM4FVPROC uniform4fv;
    PFNGLGENVERTEXARRAYSPROC gen_vertex_arrays;
    PFNGLDELETEVERTEXARRAYSPROC delete_vertex_arrays;
    PFNGLBINDVERTEXARRAYPROC bind_vertex_array;
    PFNGLDRAWARRAYSPROC draw_arrays;
    PFNGLFLUSHPROC flush;
    /* GL_OES_EGL_image; the desktop GL headers carry no typedef for it. */
    void(APIENTRY *egl_image_target_texture_2d)(GLenum target, void *image);
};

/* One conversion pass program and its uniforms. */
struct am_gl_pass {
    GLuint program;
    GLint source;
    GLint rows[2];
};

/* GPU RGB -> NV12 converter owned by one encoder. The EGL context is current
 * on the encoder worker thread from open until close. */
struct am_converter {
    const struct am_driver *driver;
    const struct am_egl_display *display;
    EGLContext context;
    /* Set only once every entry of `gl` was resolved. */
    int gl_loaded;
    struct am_gl_api gl;
    struct am_gl_pass luma_pass;
    struct am_gl_pass chroma_pass;
    GLuint vertex_array;
    GLuint upload_texture;
    GLuint luma_texture;
    GLuint chroma_texture;
    GLuint luma_framebuffer;
    GLuint chroma_framebuffer;
    CUgraphicsResource luma_resource;
    CUgraphicsResource chroma_resource;
    uint32_t width;
    uint32_t height;
    float luma_row[4];
    float cb_row[4];
    float cr_row[4];
};

int am_converter_open(struct am_converter *converter, const struct am_driver *driver,
                      CUdevice device, const struct am_nvcuda_settings *settings,
                      struct am_nvcuda_status *status);
void am_converter_close(struct am_converter *converter);
int am_converter_upload_bgra(struct am_converter *converter, const uint8_t *bgra,
                             struct am_nvcuda_status *status);
int am_converter_import_dmabuf(struct am_converter *converter, struct am_nvcuda_dmabuf *frame,
                               struct am_nvcuda_status *status);
int am_converter_copy_nv12(struct am_converter *converter, CUdeviceptr destination,
                           size_t pitch, struct am_nvcuda_status *status);
int am_converter_import_modifiers(const struct am_converter *converter, uint32_t fourcc,
                                  uint64_t *modifiers, uint32_t capacity,
                                  uint32_t *modifier_count, uint32_t *implicit);

#endif
