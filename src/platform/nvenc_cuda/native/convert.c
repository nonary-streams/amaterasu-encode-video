/*
 * GPU RGB -> NV12 conversion for the NVENC/CUDA encoder.
 *
 * A source RGB texture (an uploaded CPU BGRA frame, or a DMA-BUF imported as
 * an EGLImage) is drawn twice: once into an R8 luma texture at full size and
 * once into an RG8 chroma texture at half size. Both targets are registered
 * with CUDA once at open; each frame maps them and copies their arrays into
 * the encoder's pitched NV12 CUDA buffer.
 *
 * The colour matrix comes from Rust (color.rs), where it is checked against
 * the CPU reference converter. Chroma is the box average of each 2x2 block,
 * which is centre sited, exactly like the CPU reference.
 */
#include "internal.h"

#include <errno.h>
#include <poll.h>
#include <stdlib.h>
#include <string.h>

#define AM_FENCE_TIMEOUT_MS 500

static const char vertex_source[] =
    "#version 330 core\n"
    "void main() {\n"
    "    vec2 corner = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));\n"
    "    gl_Position = vec4(corner * 2.0 - 1.0, 0.0, 1.0);\n"
    "}\n";

/* Output row y reads source row y: GL texture row 0, framebuffer row 0 and
 * CUDA array row 0 are all the first row in memory, so no flip is needed. */
static const char luma_source[] =
    "#version 330 core\n"
    "uniform sampler2D source;\n"
    "uniform vec4 luma_row;\n"
    "layout(location = 0) out float luma;\n"
    "void main() {\n"
    "    vec3 rgb = texelFetch(source, ivec2(gl_FragCoord.xy), 0).rgb;\n"
    "    luma = dot(luma_row.rgb, rgb) + luma_row.a;\n"
    "}\n";

static const char chroma_source[] =
    "#version 330 core\n"
    "uniform sampler2D source;\n"
    "uniform vec4 cb_row;\n"
    "uniform vec4 cr_row;\n"
    "layout(location = 0) out vec2 chroma;\n"
    "void main() {\n"
    "    ivec2 origin = ivec2(gl_FragCoord.xy) * 2;\n"
    "    vec3 rgb = (texelFetch(source, origin, 0).rgb +\n"
    "                texelFetch(source, origin + ivec2(1, 0), 0).rgb +\n"
    "                texelFetch(source, origin + ivec2(0, 1), 0).rgb +\n"
    "                texelFetch(source, origin + ivec2(1, 1), 0).rgb) * 0.25;\n"
    "    chroma = vec2(dot(cb_row.rgb, rgb) + cb_row.a, dot(cr_row.rgb, rgb) + cr_row.a);\n"
    "}\n";

static int load_gl_symbol(const struct am_egl_api *egl, const char *name, void *destination) {
    __eglMustCastToProperFunctionPointerType symbol = egl->get_proc_address(name);
    if (symbol == NULL) {
        return 0;
    }
    memcpy(destination, &symbol, sizeof(symbol));
    return 1;
}

#define AM_GL(field, name) load_gl_symbol(egl, name, &gl->field)

static int load_gl(const struct am_egl_api *egl, struct am_gl_api *gl) {
    return AM_GL(get_error, "glGetError") && AM_GL(gen_textures, "glGenTextures") &&
           AM_GL(delete_textures, "glDeleteTextures") && AM_GL(bind_texture, "glBindTexture") &&
           AM_GL(tex_parameteri, "glTexParameteri") && AM_GL(tex_image_2d, "glTexImage2D") &&
           AM_GL(tex_sub_image_2d, "glTexSubImage2D") && AM_GL(pixel_storei, "glPixelStorei") &&
           AM_GL(active_texture, "glActiveTexture") &&
           AM_GL(gen_framebuffers, "glGenFramebuffers") &&
           AM_GL(delete_framebuffers, "glDeleteFramebuffers") &&
           AM_GL(bind_framebuffer, "glBindFramebuffer") &&
           AM_GL(framebuffer_texture_2d, "glFramebufferTexture2D") &&
           AM_GL(check_framebuffer_status, "glCheckFramebufferStatus") &&
           AM_GL(viewport, "glViewport") && AM_GL(create_shader, "glCreateShader") &&
           AM_GL(delete_shader, "glDeleteShader") && AM_GL(shader_source, "glShaderSource") &&
           AM_GL(compile_shader, "glCompileShader") && AM_GL(get_shaderiv, "glGetShaderiv") &&
           AM_GL(create_program, "glCreateProgram") && AM_GL(delete_program, "glDeleteProgram") &&
           AM_GL(attach_shader, "glAttachShader") && AM_GL(link_program, "glLinkProgram") &&
           AM_GL(get_programiv, "glGetProgramiv") && AM_GL(use_program, "glUseProgram") &&
           AM_GL(get_uniform_location, "glGetUniformLocation") &&
           AM_GL(uniform1i, "glUniform1i") && AM_GL(uniform4fv, "glUniform4fv") &&
           AM_GL(gen_vertex_arrays, "glGenVertexArrays") &&
           AM_GL(delete_vertex_arrays, "glDeleteVertexArrays") &&
           AM_GL(bind_vertex_array, "glBindVertexArray") && AM_GL(draw_arrays, "glDrawArrays") &&
           AM_GL(flush, "glFlush") &&
           AM_GL(egl_image_target_texture_2d, "glEGLImageTargetTexture2DOES");
}

/* Desktop OpenGL 3.3 core, surfaceless, made current on the calling thread. */
static int create_context(struct am_converter *converter, struct am_nvcuda_status *status) {
    const struct am_egl_api *egl = &converter->driver->egl;
    EGLDisplay display = converter->display->display;
    if (!egl->bind_api(EGL_OPENGL_API)) {
        am_set_status(status, AM_STAGE_GL_CONTEXT, (int32_t)egl->get_error());
        return AM_NVCUDA_CONVERTER_UNAVAILABLE;
    }
    /* The converter only renders into textures, so it needs no surface. NVIDIA's
     * device platform exposes no window-capable configs, and eglChooseConfig
     * defaults EGL_SURFACE_TYPE to EGL_WINDOW_BIT, so either create the context
     * without a config or ask for one with any surface type. */
    EGLConfig config = EGL_NO_CONFIG_KHR;
    if (!converter->display->no_config_context) {
        const EGLint config_attributes[] = {
            EGL_SURFACE_TYPE, EGL_DONT_CARE,
            EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
            EGL_NONE,
        };
        EGLint config_count = 0;
        if (!egl->choose_config(display, config_attributes, &config, 1, &config_count) ||
            config_count < 1) {
            am_set_status(status, AM_STAGE_GL_CONTEXT, (int32_t)egl->get_error());
            return AM_NVCUDA_CONVERTER_UNAVAILABLE;
        }
    }
    const EGLint context_attributes[] = {
        EGL_CONTEXT_MAJOR_VERSION, 3,
        EGL_CONTEXT_MINOR_VERSION, 3,
        EGL_CONTEXT_OPENGL_PROFILE_MASK, EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT,
        EGL_NONE,
    };
    converter->context = egl->create_context(display, config, EGL_NO_CONTEXT, context_attributes);
    if (converter->context == EGL_NO_CONTEXT) {
        am_set_status(status, AM_STAGE_GL_CONTEXT, (int32_t)egl->get_error());
        return AM_NVCUDA_CONVERTER_UNAVAILABLE;
    }
    if (!egl->make_current(display, EGL_NO_SURFACE, EGL_NO_SURFACE, converter->context)) {
        am_set_status(status, AM_STAGE_GL_CONTEXT, (int32_t)egl->get_error());
        return AM_NVCUDA_CONVERTER_UNAVAILABLE;
    }
    /* Core GL entry points come from eglGetProcAddress as well, which EGL
     * 1.5 and libglvnd provide for every client API function. */
    if (!load_gl(egl, &converter->gl)) {
        am_set_status(status, AM_STAGE_GL_CONTEXT, 0);
        return AM_NVCUDA_CONVERTER_UNAVAILABLE;
    }
    converter->gl_loaded = 1;
    return AM_NVCUDA_OK;
}

static GLuint compile_shader(const struct am_gl_api *gl, GLenum kind, const char *source) {
    GLuint shader = gl->create_shader(kind);
    if (shader == 0) {
        return 0;
    }
    gl->shader_source(shader, 1, &source, NULL);
    gl->compile_shader(shader);
    GLint compiled = GL_FALSE;
    gl->get_shaderiv(shader, GL_COMPILE_STATUS, &compiled);
    if (compiled != GL_TRUE) {
        gl->delete_shader(shader);
        return 0;
    }
    return shader;
}

static int build_pass(const struct am_gl_api *gl, const char *fragment_source,
                      const char *first_row, const char *second_row, struct am_gl_pass *pass) {
    GLuint vertex = compile_shader(gl, GL_VERTEX_SHADER, vertex_source);
    GLuint fragment = compile_shader(gl, GL_FRAGMENT_SHADER, fragment_source);
    GLuint program = vertex != 0 && fragment != 0 ? gl->create_program() : 0;
    GLint linked = GL_FALSE;
    if (program != 0) {
        gl->attach_shader(program, vertex);
        gl->attach_shader(program, fragment);
        gl->link_program(program);
        gl->get_programiv(program, GL_LINK_STATUS, &linked);
    }
    /* A linked program keeps its own copy of the shader code. */
    if (vertex != 0) {
        gl->delete_shader(vertex);
    }
    if (fragment != 0) {
        gl->delete_shader(fragment);
    }
    if (linked != GL_TRUE) {
        if (program != 0) {
            gl->delete_program(program);
        }
        return 0;
    }
    pass->program = program;
    pass->source = gl->get_uniform_location(program, "source");
    pass->rows[0] = gl->get_uniform_location(program, first_row);
    pass->rows[1] = second_row != NULL ? gl->get_uniform_location(program, second_row) : -1;
    return pass->source >= 0 && pass->rows[0] >= 0 && (second_row == NULL || pass->rows[1] >= 0);
}

static void set_sampling(const struct am_gl_api *gl) {
    gl->tex_parameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    gl->tex_parameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    gl->tex_parameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    gl->tex_parameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
}

static GLuint allocate_texture(const struct am_gl_api *gl, GLint internal_format, GLenum format,
                               uint32_t width, uint32_t height) {
    GLuint texture = 0;
    gl->gen_textures(1, &texture);
    gl->bind_texture(GL_TEXTURE_2D, texture);
    set_sampling(gl);
    gl->tex_image_2d(GL_TEXTURE_2D, 0, internal_format, (GLsizei)width, (GLsizei)height, 0,
                     format, GL_UNSIGNED_BYTE, NULL);
    gl->bind_texture(GL_TEXTURE_2D, 0);
    return texture;
}

static GLuint attach_framebuffer(const struct am_gl_api *gl, GLuint texture) {
    GLuint framebuffer = 0;
    gl->gen_framebuffers(1, &framebuffer);
    gl->bind_framebuffer(GL_FRAMEBUFFER, framebuffer);
    gl->framebuffer_texture_2d(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, texture, 0);
    GLenum complete = gl->check_framebuffer_status(GL_FRAMEBUFFER);
    gl->bind_framebuffer(GL_FRAMEBUFFER, 0);
    if (complete != GL_FRAMEBUFFER_COMPLETE) {
        gl->delete_framebuffers(1, &framebuffer);
        return 0;
    }
    return framebuffer;
}

static int create_gl_resources(struct am_converter *converter, struct am_nvcuda_status *status) {
    const struct am_gl_api *gl = &converter->gl;
    if (!build_pass(gl, luma_source, "luma_row", NULL, &converter->luma_pass) ||
        !build_pass(gl, chroma_source, "cb_row", "cr_row", &converter->chroma_pass)) {
        am_set_status(status, AM_STAGE_GL_RESOURCES, (int32_t)gl->get_error());
        return AM_NVCUDA_CONVERTER_UNAVAILABLE;
    }
    gl->gen_vertex_arrays(1, &converter->vertex_array);
    converter->upload_texture =
        allocate_texture(gl, GL_RGBA8, GL_BGRA, converter->width, converter->height);
    converter->luma_texture =
        allocate_texture(gl, GL_R8, GL_RED, converter->width, converter->height);
    converter->chroma_texture =
        allocate_texture(gl, GL_RG8, GL_RG, converter->width / 2, converter->height / 2);
    converter->luma_framebuffer = attach_framebuffer(gl, converter->luma_texture);
    converter->chroma_framebuffer = attach_framebuffer(gl, converter->chroma_texture);
    GLenum error = gl->get_error();
    if (converter->vertex_array == 0 || converter->luma_framebuffer == 0 ||
        converter->chroma_framebuffer == 0 || error != GL_NO_ERROR) {
        am_set_status(status, AM_STAGE_GL_RESOURCES, (int32_t)error);
        return AM_NVCUDA_CONVERTER_UNAVAILABLE;
    }
    return AM_NVCUDA_OK;
}

/* Registered once; the textures are never reallocated while registered. */
static int register_with_cuda(struct am_converter *converter, struct am_nvcuda_status *status) {
    const struct am_cuda_api *cuda = &converter->driver->cuda;
    CUresult result = cuda->graphics_gl_register_image(
        &converter->luma_resource, converter->luma_texture, GL_TEXTURE_2D,
        CU_GRAPHICS_REGISTER_FLAGS_READ_ONLY);
    if (result == CUDA_SUCCESS) {
        result = cuda->graphics_gl_register_image(&converter->chroma_resource,
                                                  converter->chroma_texture, GL_TEXTURE_2D,
                                                  CU_GRAPHICS_REGISTER_FLAGS_READ_ONLY);
    }
    if (result != CUDA_SUCCESS) {
        am_set_status(status, AM_STAGE_CUDA_INTEROP, (int32_t)result);
        return AM_NVCUDA_CONVERTER_UNAVAILABLE;
    }
    return AM_NVCUDA_OK;
}

int am_converter_open(struct am_converter *converter, const struct am_driver *driver,
                      CUdevice device, const struct am_nvcuda_settings *settings,
                      struct am_nvcuda_status *status) {
    memset(converter, 0, sizeof(*converter));
    converter->driver = driver;
    converter->width = settings->width;
    converter->height = settings->height;
    memcpy(converter->luma_row, settings->luma_row, sizeof(converter->luma_row));
    memcpy(converter->cb_row, settings->cb_row, sizeof(converter->cb_row));
    memcpy(converter->cr_row, settings->cr_row, sizeof(converter->cr_row));
    int result = am_egl_display_for_device(driver, device, &converter->display, status);
    if (result == AM_NVCUDA_OK) {
        result = create_context(converter, status);
    }
    if (result == AM_NVCUDA_OK) {
        result = create_gl_resources(converter, status);
    }
    if (result == AM_NVCUDA_OK) {
        result = register_with_cuda(converter, status);
    }
    return result;
}

/* Safe on a partially opened converter. CUDA registrations are released
 * before the textures they reference, while the context is still current. */
void am_converter_close(struct am_converter *converter) {
    if (converter->driver == NULL) {
        return;
    }
    const struct am_cuda_api *cuda = &converter->driver->cuda;
    if (converter->chroma_resource != NULL) {
        cuda->graphics_unregister(converter->chroma_resource);
    }
    if (converter->luma_resource != NULL) {
        cuda->graphics_unregister(converter->luma_resource);
    }
    if (converter->gl_loaded) {
        const struct am_gl_api *gl = &converter->gl;
        GLuint framebuffers[] = {converter->luma_framebuffer, converter->chroma_framebuffer};
        GLuint textures[] = {converter->upload_texture, converter->luma_texture,
                             converter->chroma_texture};
        gl->delete_framebuffers(2, framebuffers);
        gl->delete_textures(3, textures);
        gl->delete_vertex_arrays(1, &converter->vertex_array);
        gl->delete_program(converter->luma_pass.program);
        gl->delete_program(converter->chroma_pass.program);
    }
    if (converter->context != EGL_NO_CONTEXT) {
        const struct am_egl_api *egl = &converter->driver->egl;
        EGLDisplay display = converter->display->display;
        egl->make_current(display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
        egl->destroy_context(display, converter->context);
    }
    memset(converter, 0, sizeof(*converter));
}

static void draw_pass(const struct am_converter *converter, const struct am_gl_pass *pass,
                      GLuint framebuffer, uint32_t width, uint32_t height,
                      const float *first_row, const float *second_row, GLuint source) {
    const struct am_gl_api *gl = &converter->gl;
    gl->bind_framebuffer(GL_FRAMEBUFFER, framebuffer);
    gl->viewport(0, 0, (GLsizei)width, (GLsizei)height);
    gl->use_program(pass->program);
    gl->active_texture(GL_TEXTURE0);
    gl->bind_texture(GL_TEXTURE_2D, source);
    gl->uniform1i(pass->source, 0);
    gl->uniform4fv(pass->rows[0], 1, first_row);
    if (second_row != NULL) {
        gl->uniform4fv(pass->rows[1], 1, second_row);
    }
    gl->bind_vertex_array(converter->vertex_array);
    gl->draw_arrays(GL_TRIANGLES, 0, 3);
}

static int convert_texture(struct am_converter *converter, GLuint source,
                           struct am_nvcuda_status *status) {
    const struct am_gl_api *gl = &converter->gl;
    draw_pass(converter, &converter->luma_pass, converter->luma_framebuffer, converter->width,
              converter->height, converter->luma_row, NULL, source);
    draw_pass(converter, &converter->chroma_pass, converter->chroma_framebuffer,
              converter->width / 2, converter->height / 2, converter->cb_row, converter->cr_row,
              source);
    gl->bind_texture(GL_TEXTURE_2D, 0);
    gl->bind_framebuffer(GL_FRAMEBUFFER, 0);
    gl->flush();
    GLenum error = gl->get_error();
    if (error != GL_NO_ERROR) {
        am_set_status(status, AM_STAGE_CONVERT, (int32_t)error);
        return AM_NVCUDA_NATIVE_FAILURE;
    }
    return AM_NVCUDA_OK;
}

int am_converter_upload_bgra(struct am_converter *converter, const uint8_t *bgra,
                             struct am_nvcuda_status *status) {
    const struct am_gl_api *gl = &converter->gl;
    gl->bind_texture(GL_TEXTURE_2D, converter->upload_texture);
    gl->pixel_storei(GL_UNPACK_ALIGNMENT, 4);
    gl->pixel_storei(GL_UNPACK_ROW_LENGTH, 0);
    gl->tex_sub_image_2d(GL_TEXTURE_2D, 0, 0, 0, (GLsizei)converter->width,
                         (GLsizei)converter->height, GL_BGRA, GL_UNSIGNED_BYTE, bgra);
    gl->bind_texture(GL_TEXTURE_2D, 0);
    GLenum error = gl->get_error();
    if (error != GL_NO_ERROR) {
        am_set_status(status, AM_STAGE_UPLOAD, (int32_t)error);
        return AM_NVCUDA_NATIVE_FAILURE;
    }
    return convert_texture(converter, converter->upload_texture, status);
}

/* Bounded CPU wait for a sync_file; used when EGL cannot wait on the GPU. */
static int poll_fence(int fence_fd, struct am_nvcuda_status *status) {
    struct pollfd descriptor = {.fd = fence_fd, .events = POLLIN, .revents = 0};
    int ready;
    do {
        ready = poll(&descriptor, 1, AM_FENCE_TIMEOUT_MS);
    } while (ready < 0 && errno == EINTR);
    if (ready == 0) {
        am_set_status(status, AM_STAGE_FENCE, 0);
        return AM_NVCUDA_FENCE_TIMEOUT;
    }
    if (ready < 0 || (descriptor.revents & (POLLERR | POLLNVAL)) != 0) {
        am_set_status(status, AM_STAGE_FENCE, ready < 0 ? errno : descriptor.revents);
        return AM_NVCUDA_IMPORT_FAILED;
    }
    return AM_NVCUDA_OK;
}

/* Queue a GPU-side wait on the current context. On success EGL owns the fd,
 * which is then cleared in the frame so the Rust caller does not close it. */
static int queue_gpu_fence_wait(const struct am_converter *converter, int32_t *fence_fd) {
    const struct am_egl_api *egl = &converter->driver->egl;
    EGLDisplay display = converter->display->display;
    const EGLint attributes[] = {EGL_SYNC_NATIVE_FENCE_FD_ANDROID, *fence_fd, EGL_NONE};
    EGLSyncKHR sync = egl->create_sync(display, EGL_SYNC_NATIVE_FENCE_ANDROID, attributes);
    if (sync == EGL_NO_SYNC_KHR) {
        return 0;
    }
    *fence_fd = -1;
    EGLint waited = egl->wait_sync(display, sync, 0);
    /* A destroyed sync stays alive until the queued wait no longer needs it. */
    egl->destroy_sync(display, sync);
    return waited == EGL_TRUE;
}

static int wait_for_acquire_fences(const struct am_converter *converter,
                                   struct am_nvcuda_dmabuf *frame,
                                   struct am_nvcuda_status *status) {
    int gpu_wait = converter->display->native_fence_sync && converter->display->wait_sync;
    for (uint32_t plane = 0; plane < frame->plane_count; ++plane) {
        if (frame->fence_fds[plane] < 0) {
            continue;
        }
        if (gpu_wait && queue_gpu_fence_wait(converter, &frame->fence_fds[plane])) {
            continue;
        }
        if (frame->fence_fds[plane] < 0) {
            /* EGL took the fence but could not queue the wait. */
            am_set_status(status, AM_STAGE_FENCE,
                          (int32_t)converter->driver->egl.get_error());
            return AM_NVCUDA_IMPORT_FAILED;
        }
        int result = poll_fence(frame->fence_fds[plane], status);
        if (result != AM_NVCUDA_OK) {
            return result;
        }
    }
    return AM_NVCUDA_OK;
}

static const EGLint plane_attributes[AM_NVCUDA_MAX_PLANES][5] = {
    {EGL_DMA_BUF_PLANE0_FD_EXT, EGL_DMA_BUF_PLANE0_OFFSET_EXT, EGL_DMA_BUF_PLANE0_PITCH_EXT,
     EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT, EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT},
    {EGL_DMA_BUF_PLANE1_FD_EXT, EGL_DMA_BUF_PLANE1_OFFSET_EXT, EGL_DMA_BUF_PLANE1_PITCH_EXT,
     EGL_DMA_BUF_PLANE1_MODIFIER_LO_EXT, EGL_DMA_BUF_PLANE1_MODIFIER_HI_EXT},
    {EGL_DMA_BUF_PLANE2_FD_EXT, EGL_DMA_BUF_PLANE2_OFFSET_EXT, EGL_DMA_BUF_PLANE2_PITCH_EXT,
     EGL_DMA_BUF_PLANE2_MODIFIER_LO_EXT, EGL_DMA_BUF_PLANE2_MODIFIER_HI_EXT},
    {EGL_DMA_BUF_PLANE3_FD_EXT, EGL_DMA_BUF_PLANE3_OFFSET_EXT, EGL_DMA_BUF_PLANE3_PITCH_EXT,
     EGL_DMA_BUF_PLANE3_MODIFIER_LO_EXT, EGL_DMA_BUF_PLANE3_MODIFIER_HI_EXT},
};

#define AM_DRM_FORMAT_MOD_INVALID UINT64_C(0x00ffffffffffffff)

/* EGL never takes ownership of plane fds; it references the buffers itself. */
static EGLImageKHR create_dmabuf_image(const struct am_converter *converter,
                                       const struct am_nvcuda_dmabuf *frame) {
    EGLint attributes[6 + AM_NVCUDA_MAX_PLANES * 10 + 1];
    size_t count = 0;
    attributes[count++] = EGL_WIDTH;
    attributes[count++] = (EGLint)frame->width;
    attributes[count++] = EGL_HEIGHT;
    attributes[count++] = (EGLint)frame->height;
    attributes[count++] = EGL_LINUX_DRM_FOURCC_EXT;
    attributes[count++] = (EGLint)frame->fourcc;
    int explicit_modifier = frame->modifier != AM_DRM_FORMAT_MOD_INVALID;
    for (uint32_t plane = 0; plane < frame->plane_count; ++plane) {
        attributes[count++] = plane_attributes[plane][0];
        attributes[count++] = frame->plane_fds[plane];
        attributes[count++] = plane_attributes[plane][1];
        attributes[count++] = (EGLint)frame->offsets[plane];
        attributes[count++] = plane_attributes[plane][2];
        attributes[count++] = (EGLint)frame->pitches[plane];
        if (explicit_modifier) {
            attributes[count++] = plane_attributes[plane][3];
            attributes[count++] = (EGLint)(frame->modifier & 0xffffffffu);
            attributes[count++] = plane_attributes[plane][4];
            attributes[count++] = (EGLint)(frame->modifier >> 32);
        }
    }
    attributes[count] = EGL_NONE;
    return converter->driver->egl.create_image(converter->display->display, EGL_NO_CONTEXT,
                                               EGL_LINUX_DMA_BUF_EXT, NULL, attributes);
}

static int validate_dmabuf(const struct am_converter *converter,
                           const struct am_nvcuda_dmabuf *frame) {
    if (frame->width != converter->width || frame->height != converter->height ||
        frame->plane_count == 0 || frame->plane_count > AM_NVCUDA_MAX_PLANES) {
        return 0;
    }
    if (frame->modifier != AM_DRM_FORMAT_MOD_INVALID && !converter->display->dmabuf_modifiers) {
        return 0;
    }
    for (uint32_t plane = 0; plane < frame->plane_count; ++plane) {
        if (frame->plane_fds[plane] < 0 || frame->pitches[plane] == 0 ||
            frame->offsets[plane] > INT32_MAX || frame->pitches[plane] > INT32_MAX) {
            return 0;
        }
    }
    return 1;
}

int am_converter_import_dmabuf(struct am_converter *converter, struct am_nvcuda_dmabuf *frame,
                               struct am_nvcuda_status *status) {
    if (!converter->display->dmabuf_import) {
        am_set_status(status, AM_STAGE_IMPORT, 0);
        return AM_NVCUDA_UNSUPPORTED;
    }
    if (!validate_dmabuf(converter, frame)) {
        am_set_status(status, AM_STAGE_IMPORT, 0);
        return AM_NVCUDA_INVALID_ARGUMENT;
    }
    int result = wait_for_acquire_fences(converter, frame, status);
    if (result != AM_NVCUDA_OK) {
        return result;
    }
    const struct am_egl_api *egl = &converter->driver->egl;
    EGLImageKHR image = create_dmabuf_image(converter, frame);
    if (image == EGL_NO_IMAGE_KHR) {
        am_set_status(status, AM_STAGE_IMPORT, (int32_t)egl->get_error());
        return AM_NVCUDA_IMPORT_FAILED;
    }
    /* A fresh texture per frame: deleting it drops the last GL reference to
     * the producer's buffer once the conversion has been consumed. */
    const struct am_gl_api *gl = &converter->gl;
    GLuint source = 0;
    gl->gen_textures(1, &source);
    gl->bind_texture(GL_TEXTURE_2D, source);
    set_sampling(gl);
    gl->egl_image_target_texture_2d(GL_TEXTURE_2D, image);
    gl->bind_texture(GL_TEXTURE_2D, 0);
    GLenum error = gl->get_error();
    if (error != GL_NO_ERROR) {
        am_set_status(status, AM_STAGE_IMPORT, (int32_t)error);
        result = AM_NVCUDA_IMPORT_FAILED;
    } else {
        result = convert_texture(converter, source, status);
    }
    gl->delete_textures(1, &source);
    egl->destroy_image(converter->display->display, image);
    return result;
}

static int copy_plane(const struct am_cuda_api *cuda, CUarray source, CUdeviceptr destination,
                      size_t pitch, size_t width_bytes, size_t rows) {
    CUDA_MEMCPY2D copy;
    memset(&copy, 0, sizeof(copy));
    copy.srcMemoryType = CU_MEMORYTYPE_ARRAY;
    copy.srcArray = source;
    copy.dstMemoryType = CU_MEMORYTYPE_DEVICE;
    copy.dstDevice = destination;
    copy.dstPitch = pitch;
    copy.WidthInBytes = width_bytes;
    copy.Height = rows;
    return cuda->memcpy_2d(&copy) == CUDA_SUCCESS;
}

/* Mapping orders the GL draws before the CUDA copies; the context
 * synchronization completes the copies before NVENC or the caller proceed. */
int am_converter_copy_nv12(struct am_converter *converter, CUdeviceptr destination,
                           size_t pitch, struct am_nvcuda_status *status) {
    const struct am_cuda_api *cuda = &converter->driver->cuda;
    CUgraphicsResource resources[] = {converter->luma_resource, converter->chroma_resource};
    CUresult result = cuda->graphics_map(2, resources, NULL);
    if (result != CUDA_SUCCESS) {
        am_set_status(status, AM_STAGE_CUDA_INTEROP, (int32_t)result);
        return AM_NVCUDA_NATIVE_FAILURE;
    }
    CUarray luma = NULL;
    CUarray chroma = NULL;
    int copied =
        cuda->graphics_mapped_array(&luma, converter->luma_resource, 0, 0) == CUDA_SUCCESS &&
        cuda->graphics_mapped_array(&chroma, converter->chroma_resource, 0, 0) ==
            CUDA_SUCCESS &&
        copy_plane(cuda, luma, destination, pitch, converter->width, converter->height) &&
        copy_plane(cuda, chroma, destination + pitch * converter->height, pitch,
                   converter->width, converter->height / 2);
    CUresult unmapped = cuda->graphics_unmap(2, resources, NULL);
    CUresult synchronized = cuda->ctx_synchronize();
    if (!copied || unmapped != CUDA_SUCCESS || synchronized != CUDA_SUCCESS) {
        am_set_status(status, AM_STAGE_CUDA_INTEROP,
                      (int32_t)(unmapped != CUDA_SUCCESS ? unmapped : synchronized));
        return AM_NVCUDA_NATIVE_FAILURE;
    }
    return AM_NVCUDA_OK;
}

static int format_listed(const struct am_egl_api *egl, EGLDisplay display, uint32_t fourcc) {
    EGLint count = 0;
    if (!egl->query_dmabuf_formats(display, 0, NULL, &count) || count <= 0 || count > 4096) {
        return 0;
    }
    EGLint *formats = calloc((size_t)count, sizeof(*formats));
    if (formats == NULL) {
        return 0;
    }
    int listed = 0;
    if (egl->query_dmabuf_formats(display, count, formats, &count)) {
        for (EGLint index = 0; index < count; ++index) {
            listed |= (uint32_t)formats[index] == fourcc;
        }
    }
    free(formats);
    return listed;
}

/* Lists only modifiers that may be bound to GL_TEXTURE_2D; external-only
 * modifiers would need GL_TEXTURE_EXTERNAL_OES, which this converter lacks. */
int am_converter_import_modifiers(const struct am_converter *converter, uint32_t fourcc,
                                  uint64_t *modifiers, uint32_t capacity,
                                  uint32_t *modifier_count, uint32_t *implicit) {
    *modifier_count = 0;
    *implicit = 0;
    const struct am_egl_display *display = converter->display;
    if (!display->dmabuf_import) {
        return AM_NVCUDA_OK;
    }
    if (!display->dmabuf_modifiers) {
        /* Without the query the format list is unknown; implicit layouts are
         * attempted and a failed import fails that frame. */
        *implicit = 1;
        return AM_NVCUDA_OK;
    }
    const struct am_egl_api *egl = &converter->driver->egl;
    *implicit = (uint32_t)format_listed(egl, display->display, fourcc);
    EGLint count = 0;
    if (!egl->query_dmabuf_modifiers(display->display, (EGLint)fourcc, 0, NULL, NULL, &count) ||
        count <= 0) {
        return AM_NVCUDA_OK;
    }
    if (count > 1024) {
        count = 1024;
    }
    EGLuint64KHR *listed = calloc((size_t)count, sizeof(*listed));
    EGLBoolean *external_only = calloc((size_t)count, sizeof(*external_only));
    if (listed == NULL || external_only == NULL) {
        free(listed);
        free(external_only);
        return AM_NVCUDA_ALLOCATION_FAILED;
    }
    if (egl->query_dmabuf_modifiers(display->display, (EGLint)fourcc, count, listed,
                                    external_only, &count)) {
        for (EGLint index = 0; index < count && *modifier_count < capacity; ++index) {
            if (!external_only[index]) {
                modifiers[(*modifier_count)++] = listed[index];
            }
        }
    }
    free(listed);
    free(external_only);
    return AM_NVCUDA_OK;
}
