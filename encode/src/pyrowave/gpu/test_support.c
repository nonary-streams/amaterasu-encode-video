/* GPU fixtures only: render known colors into exportable scanout-style memory.
 * No CPU pixel upload or mapping is used to create these source frames. */
#define _GNU_SOURCE
#include "adapter.h"
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl3.h>
#include <GLES2/gl2ext.h>
#include <gbm.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <unistd.h>

struct am_pyrowave_test_image {
    int fd;
    struct gbm_device *gbm;
    struct gbm_bo *buffer;
    EGLDisplay display;
    EGLContext context;
    EGLSurface surface;
    EGLImageKHR image;
    GLuint texture, framebuffer;
    struct am_pyrowave_gpu_frame frame;
};
void am_pyrowave_test_image_destroy(struct am_pyrowave_test_image *fixture) {
    if (!fixture) return;
    for (unsigned i=0;i<4;i++) {
        if (fixture->frame.fds[i]>=0) close(fixture->frame.fds[i]);
        if (fixture->frame.fences[i]>=0) close(fixture->frame.fences[i]);
    }
    if (fixture->display!=EGL_NO_DISPLAY) {
        if (fixture->context!=EGL_NO_CONTEXT && fixture->surface!=EGL_NO_SURFACE) {
            eglMakeCurrent(fixture->display,fixture->surface,fixture->surface,fixture->context);
            glFinish();
            if (fixture->framebuffer) glDeleteFramebuffers(1,&fixture->framebuffer);
            if (fixture->texture) glDeleteTextures(1,&fixture->texture);
        }
        PFNEGLDESTROYIMAGEKHRPROC destroy=(PFNEGLDESTROYIMAGEKHRPROC)eglGetProcAddress("eglDestroyImageKHR");
        if (destroy && fixture->image!=EGL_NO_IMAGE_KHR) destroy(fixture->display,fixture->image);
        eglMakeCurrent(fixture->display,EGL_NO_SURFACE,EGL_NO_SURFACE,EGL_NO_CONTEXT);
        if (fixture->context!=EGL_NO_CONTEXT) eglDestroyContext(fixture->display,fixture->context);
        if (fixture->surface!=EGL_NO_SURFACE) eglDestroySurface(fixture->display,fixture->surface);
        eglTerminate(fixture->display);
    }
    if (fixture->buffer) gbm_bo_destroy(fixture->buffer);
    if (fixture->gbm) gbm_device_destroy(fixture->gbm);
    if (fixture->fd>=0) close(fixture->fd);
    free(fixture);
}
struct am_pyrowave_test_image *am_pyrowave_test_image_create(const struct am_pyrowave_gpu_identity *identity,uint32_t width,uint32_t height,uint32_t fourcc,uint64_t modifier,const float *color,uint32_t hdr,struct am_pyrowave_gpu_frame *frame) {
    if (!identity || !frame || !color || !width || !height) return NULL;
    struct am_pyrowave_test_image *fixture=calloc(1,sizeof(*fixture));
    if (!fixture) return NULL;
    fixture->fd=-1;
    for (unsigned i=0;i<4;i++) {fixture->frame.fds[i]=-1;fixture->frame.fences[i]=-1;}
    for (unsigned index=128;index<192;index++) {
        char path[64];snprintf(path,sizeof(path),"/dev/dri/renderD%u",index);
        struct stat node;
        if (stat(path,&node)==0 && major(node.st_rdev)==identity->render_major && minor(node.st_rdev)==identity->render_minor) {fixture->fd=open(path,O_RDWR|O_CLOEXEC);break;}
    }
    if (fixture->fd<0 || !(fixture->gbm=gbm_create_device(fixture->fd))) goto fail;
    PFNEGLGETPLATFORMDISPLAYEXTPROC get_display=(PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress("eglGetPlatformDisplayEXT");
    if (!get_display) goto fail;
    fixture->display=get_display(EGL_PLATFORM_GBM_KHR,fixture->gbm,NULL);
    if (fixture->display==EGL_NO_DISPLAY || !eglInitialize(fixture->display,NULL,NULL) || !eglBindAPI(EGL_OPENGL_ES_API)) goto fail;
    EGLint config_attributes[]={EGL_SURFACE_TYPE,EGL_PBUFFER_BIT,EGL_RENDERABLE_TYPE,EGL_OPENGL_ES3_BIT,EGL_RED_SIZE,8,EGL_GREEN_SIZE,8,EGL_BLUE_SIZE,8,EGL_NONE};
    EGLConfig config;EGLint count=0;
    if (!eglChooseConfig(fixture->display,config_attributes,&config,1,&count) || count!=1) goto fail;
    EGLint context_attributes[]={EGL_CONTEXT_CLIENT_VERSION,3,EGL_NONE};
    EGLint surface_attributes[]={EGL_WIDTH,1,EGL_HEIGHT,1,EGL_NONE};
    fixture->context=eglCreateContext(fixture->display,config,EGL_NO_CONTEXT,context_attributes);
    fixture->surface=eglCreatePbufferSurface(fixture->display,config,surface_attributes);
    if (fixture->context==EGL_NO_CONTEXT || fixture->surface==EGL_NO_SURFACE || !eglMakeCurrent(fixture->display,fixture->surface,fixture->surface,fixture->context)) goto fail;
    fixture->buffer=gbm_bo_create_with_modifiers2(fixture->gbm,width,height,fourcc,&modifier,1,GBM_BO_USE_RENDERING);
    if (!fixture->buffer) goto fail;
    fixture->frame.width=width;fixture->frame.height=height;fixture->frame.fourcc=fourcc;fixture->frame.modifier=gbm_bo_get_modifier(fixture->buffer);fixture->frame.hdr=hdr;
    int planes=gbm_bo_get_plane_count(fixture->buffer);
    if (planes<1 || planes>4 || fixture->frame.modifier!=modifier) goto fail;
    fixture->frame.plane_count=(uint32_t)planes;
    EGLint attributes[64];size_t used=0;
#define ATTRIBUTE(key,value) do {attributes[used++]=(key);attributes[used++]=(EGLint)(value);} while (0)
    ATTRIBUTE(EGL_WIDTH,width);ATTRIBUTE(EGL_HEIGHT,height);ATTRIBUTE(EGL_LINUX_DRM_FOURCC_EXT,fourcc);
    const EGLint keys[4][5]={
        {EGL_DMA_BUF_PLANE0_FD_EXT,EGL_DMA_BUF_PLANE0_OFFSET_EXT,EGL_DMA_BUF_PLANE0_PITCH_EXT,EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT},
        {EGL_DMA_BUF_PLANE1_FD_EXT,EGL_DMA_BUF_PLANE1_OFFSET_EXT,EGL_DMA_BUF_PLANE1_PITCH_EXT,EGL_DMA_BUF_PLANE1_MODIFIER_LO_EXT,EGL_DMA_BUF_PLANE1_MODIFIER_HI_EXT},
        {EGL_DMA_BUF_PLANE2_FD_EXT,EGL_DMA_BUF_PLANE2_OFFSET_EXT,EGL_DMA_BUF_PLANE2_PITCH_EXT,EGL_DMA_BUF_PLANE2_MODIFIER_LO_EXT,EGL_DMA_BUF_PLANE2_MODIFIER_HI_EXT},
        {EGL_DMA_BUF_PLANE3_FD_EXT,EGL_DMA_BUF_PLANE3_OFFSET_EXT,EGL_DMA_BUF_PLANE3_PITCH_EXT,EGL_DMA_BUF_PLANE3_MODIFIER_LO_EXT,EGL_DMA_BUF_PLANE3_MODIFIER_HI_EXT}};
    for (int i=0;i<planes;i++) {
        fixture->frame.fds[i]=gbm_bo_get_fd_for_plane(fixture->buffer,i);
        fixture->frame.offsets[i]=gbm_bo_get_offset(fixture->buffer,i);
        fixture->frame.pitches[i]=gbm_bo_get_stride_for_plane(fixture->buffer,i);
        if (fixture->frame.fds[i]<0) goto fail;
        ATTRIBUTE(keys[i][0],fixture->frame.fds[i]);ATTRIBUTE(keys[i][1],fixture->frame.offsets[i]);ATTRIBUTE(keys[i][2],fixture->frame.pitches[i]);
        ATTRIBUTE(keys[i][3],(uint32_t)modifier);ATTRIBUTE(keys[i][4],modifier>>32);
    }
    attributes[used++]=EGL_NONE;
    PFNEGLCREATEIMAGEKHRPROC create_image=(PFNEGLCREATEIMAGEKHRPROC)eglGetProcAddress("eglCreateImageKHR");
    PFNGLEGLIMAGETARGETTEXTURE2DOESPROC bind_image=(PFNGLEGLIMAGETARGETTEXTURE2DOESPROC)eglGetProcAddress("glEGLImageTargetTexture2DOES");
    if (!create_image || !bind_image) goto fail;
    fixture->image=create_image(fixture->display,EGL_NO_CONTEXT,EGL_LINUX_DMA_BUF_EXT,NULL,attributes);
    if (fixture->image==EGL_NO_IMAGE_KHR) goto fail;
    glGenTextures(1,&fixture->texture);glBindTexture(GL_TEXTURE_2D,fixture->texture);bind_image(GL_TEXTURE_2D,fixture->image);
    glGenFramebuffers(1,&fixture->framebuffer);glBindFramebuffer(GL_FRAMEBUFFER,fixture->framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER,GL_COLOR_ATTACHMENT0,GL_TEXTURE_2D,fixture->texture,0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER)!=GL_FRAMEBUFFER_COMPLETE) goto fail;
    glViewport(0,0,(GLsizei)width,(GLsizei)height);glClearColor(color[0],color[1],color[2],1.0f);glClear(GL_COLOR_BUFFER_BIT);
    if (glGetError()!=GL_NO_ERROR) goto fail;
    PFNEGLCREATESYNCKHRPROC create_sync=(PFNEGLCREATESYNCKHRPROC)eglGetProcAddress("eglCreateSyncKHR");
    PFNEGLDUPNATIVEFENCEFDANDROIDPROC duplicate_fence=(PFNEGLDUPNATIVEFENCEFDANDROIDPROC)eglGetProcAddress("eglDupNativeFenceFDANDROID");
    PFNEGLDESTROYSYNCKHRPROC destroy_sync=(PFNEGLDESTROYSYNCKHRPROC)eglGetProcAddress("eglDestroySyncKHR");
    if (!create_sync || !duplicate_fence || !destroy_sync) goto fail;
    EGLint sync_attributes[]={EGL_SYNC_NATIVE_FENCE_FD_ANDROID,EGL_NO_NATIVE_FENCE_FD_ANDROID,EGL_NONE};
    EGLSyncKHR sync=create_sync(fixture->display,EGL_SYNC_NATIVE_FENCE_ANDROID,sync_attributes);
    if (sync==EGL_NO_SYNC_KHR) goto fail;
    glFlush();
    int fence=duplicate_fence(fixture->display,sync);
    destroy_sync(fixture->display,sync);
    if (fence<0) goto fail;
    for (int i=0;i<planes;i++) {fixture->frame.fences[i]=fcntl(fence,F_DUPFD_CLOEXEC,0);if(fixture->frame.fences[i]<0){close(fence);goto fail;}}
    close(fence);
    *frame=fixture->frame;
    return fixture;
fail:
    am_pyrowave_test_image_destroy(fixture);
    return NULL;
}
