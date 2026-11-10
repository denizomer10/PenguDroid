/*
 * ARM64 guest EGL probe: the thing an ARM-only game does at startup.
 *
 * Unlike app.c (which dlopens the *host* Bionic-compatible libEGL), this object
 * links against the platform EGL/GLES2 headers and libraries, exactly like a
 * translated app's libmain.so. Under the translator those calls land in the
 * NativeBridgeSupport stub `system/lib64/arm64/libEGL.so`, are intercepted into
 * `libberberis_proxy_libEGL.so` on the host, and reach the PenguDroid GL shim
 * through the host forwarder module.
 *
 * The return value encodes the first stage that failed, so one run says how far
 * the guest got:
 *
 *   100  the whole path worked (initialize, config, context, surface, swap)
 *     1  eglGetDisplay returned no display
 *     2  eglInitialize failed
 *     3  eglChooseConfig failed
 *     4  eglCreateContext failed
 *     5  eglCreateWindowSurface failed
 *     6  eglMakeCurrent failed
 *     7  gl entry points unavailable from eglGetProcAddress
 *     8  eglSwapBuffers failed
 */
#include <EGL/egl.h>
#include <GLES2/gl2.h>
#include <android/log.h>

#define TAG "pd-egl-probe"
#define LOGI(...) __android_log_print(ANDROID_LOG_INFO, TAG, __VA_ARGS__)

#define PROBE_OK 100

__attribute__((visibility("default"))) int pengudroid_egl_probe(void) {
    EGLDisplay dpy = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (dpy == EGL_NO_DISPLAY) {
        LOGI("eglGetDisplay -> NO_DISPLAY (err 0x%x)", eglGetError());
        return 1;
    }
    LOGI("eglGetDisplay -> %p", (void *)dpy);

    EGLint major = 0, minor = 0;
    if (!eglInitialize(dpy, &major, &minor)) {
        LOGI("eglInitialize failed (err 0x%x)", eglGetError());
        return 2;
    }
    LOGI("eglInitialize -> EGL %d.%d", major, minor);

    const EGLint cfg_attribs[] = {
        EGL_SURFACE_TYPE, EGL_WINDOW_BIT,
        EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8,
        EGL_NONE,
    };
    EGLConfig cfg = NULL;
    EGLint count = 0;
    if (!eglChooseConfig(dpy, cfg_attribs, &cfg, 1, &count) || count < 1) {
        LOGI("eglChooseConfig failed (err 0x%x)", eglGetError());
        return 3;
    }
    LOGI("eglChooseConfig -> %d config(s)", count);

    eglBindAPI(EGL_OPENGL_ES_API);
    const EGLint ctx_attribs[] = {EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE};
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, ctx_attribs);
    if (ctx == EGL_NO_CONTEXT) {
        LOGI("eglCreateContext failed (err 0x%x)", eglGetError());
        return 4;
    }
    LOGI("eglCreateContext -> %p", (void *)ctx);

    /* The shim takes the surface geometry from PENGUDROID_SURFACE_W/H and
     * ignores the native window, so a null window is the honest call here. */
    EGLSurface surf = eglCreateWindowSurface(dpy, cfg, (EGLNativeWindowType)0, NULL);
    if (surf == EGL_NO_SURFACE) {
        LOGI("eglCreateWindowSurface failed (err 0x%x)", eglGetError());
        return 5;
    }
    LOGI("eglCreateWindowSurface -> %p", (void *)surf);

    if (!eglMakeCurrent(dpy, surf, surf, ctx)) {
        LOGI("eglMakeCurrent failed (err 0x%x)", eglGetError());
        return 6;
    }
    LOGI("eglMakeCurrent -> ok, GL_VERSION=%s", (const char *)glGetString(GL_VERSION));

    for (int frame = 0; frame < 3; ++frame) {
        glClearColor(0.1f, 0.4f, 0.9f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        glFinish();
        if (!eglSwapBuffers(dpy, surf)) {
            LOGI("eglSwapBuffers failed at frame %d (err 0x%x)", frame, eglGetError());
            return 8;
        }
    }
    LOGI("guest EGL/GL path complete: 3 frames swapped");
    return PROBE_OK;
}
