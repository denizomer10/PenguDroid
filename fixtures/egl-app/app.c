/*
 * A minimal "Android app" that talks EGL the way a native app would, but loads
 * PenguDroid's Bionic-compatible libEGL instead of the system one.
 *
 * It creates a context and a window surface, renders a few frames, and calls
 * `eglSwapBuffers`. The shim exports each frame's FBO as a dma-buf and hands it
 * to the daemon, so a successful run proves the whole chain:
 *
 *     app EGL/GL -> host GPU -> dma-buf -> daemon -> Wayland
 *
 * Everything is resolved with dlsym so the shim can be pointed at via
 * PENGUDROID_LIBEGL (no link-time dependency, no headers).
 */
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>

typedef void *EGLDisplay;
typedef void *EGLConfig;
typedef void *EGLContext;
typedef void *EGLSurface;
typedef int EGLBoolean;
typedef int EGLint;
typedef unsigned int EGLenum;
typedef void (*GLProc)(void);

#define EGL_DEFAULT_DISPLAY ((void *)0)
#define EGL_NO_CONTEXT ((void *)0)
#define EGL_OPENGL_ES_API 0x30A0
#define EGL_SURFACE_TYPE 0x3033
#define EGL_PBUFFER_BIT 0x0001
#define EGL_RENDERABLE_TYPE 0x3040
#define EGL_OPENGL_ES2_BIT 0x0004
#define EGL_RED_SIZE 0x3024
#define EGL_GREEN_SIZE 0x3023
#define EGL_BLUE_SIZE 0x3022
#define EGL_ALPHA_SIZE 0x3021
#define EGL_NONE 0x3038
#define EGL_CONTEXT_CLIENT_VERSION 0x3098
#define GL_COLOR_BUFFER_BIT 0x4000

static void *sym(void *h, const char *name) {
    void *p = dlsym(h, name);
    if (!p) {
        fprintf(stderr, "egl-app: missing symbol %s\n", name);
        exit(2);
    }
    return p;
}

int main(void) {
    const char *lib = getenv("PENGUDROID_LIBEGL");
    if (!lib) {
        lib = "target/release/libpengudroid_gleshim.so";
    }
    void *h = dlopen(lib, RTLD_NOW | RTLD_LOCAL);
    if (!h) {
        fprintf(stderr, "egl-app: dlopen(%s): %s\n", lib, dlerror());
        return 2;
    }

    EGLDisplay (*getDisplay)(void *) = sym(h, "eglGetDisplay");
    EGLBoolean (*initialize)(EGLDisplay, EGLint *, EGLint *) = sym(h, "eglInitialize");
    EGLBoolean (*bindAPI)(EGLenum) = sym(h, "eglBindAPI");
    EGLBoolean (*chooseConfig)(EGLDisplay, const EGLint *, EGLConfig *, EGLint, EGLint *) =
        sym(h, "eglChooseConfig");
    EGLContext (*createContext)(EGLDisplay, EGLConfig, EGLContext, const EGLint *) =
        sym(h, "eglCreateContext");
    EGLSurface (*createWindowSurface)(EGLDisplay, EGLConfig, void *, const EGLint *) =
        sym(h, "eglCreateWindowSurface");
    EGLBoolean (*makeCurrent)(EGLDisplay, EGLSurface, EGLSurface, EGLContext) =
        sym(h, "eglMakeCurrent");
    EGLBoolean (*swapBuffers)(EGLDisplay, EGLSurface) = sym(h, "eglSwapBuffers");
    GLProc (*getProcAddress)(const char *) = sym(h, "eglGetProcAddress");
    EGLint (*getError)(void) = sym(h, "eglGetError");

    EGLDisplay dpy = getDisplay(EGL_DEFAULT_DISPLAY);
    EGLint major = 0, minor = 0;
    if (!initialize(dpy, &major, &minor)) {
        fprintf(stderr, "egl-app: eglInitialize failed (0x%x)\n", getError());
        return 1;
    }
    printf("egl-app: EGL %d.%d via %s\n", major, minor, lib);

    EGLint cfg_attribs[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
        EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8,
        EGL_NONE,
    };
    EGLConfig cfg = 0;
    EGLint n = 0;
    if (!chooseConfig(dpy, cfg_attribs, &cfg, 1, &n) || n < 1) {
        fprintf(stderr, "egl-app: eglChooseConfig failed (0x%x)\n", getError());
        return 1;
    }
    bindAPI(EGL_OPENGL_ES_API);

    EGLint ctx_attribs[] = {EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE};
    EGLContext ctx = createContext(dpy, cfg, EGL_NO_CONTEXT, ctx_attribs);
    if (!ctx) {
        fprintf(stderr, "egl-app: eglCreateContext failed (0x%x)\n", getError());
        return 1;
    }
    /* The "native window" is opaque to us; the shim takes geometry from env. */
    EGLSurface surf = createWindowSurface(dpy, cfg, (void *)0, (const EGLint *)0);
    if (!surf) {
        fprintf(stderr, "egl-app: eglCreateWindowSurface failed (0x%x)\n", getError());
        return 1;
    }
    if (!makeCurrent(dpy, surf, surf, ctx)) {
        fprintf(stderr, "egl-app: eglMakeCurrent failed (0x%x)\n", getError());
        return 1;
    }

    void (*clearColor)(float, float, float, float) =
        (void (*)(float, float, float, float))getProcAddress("glClearColor");
    void (*clear)(unsigned int) = (void (*)(unsigned int))getProcAddress("glClear");
    void (*finish)(void) = (void (*)(void))getProcAddress("glFinish");
    if (!clearColor || !clear || !finish) {
        fprintf(stderr, "egl-app: gl entry points unavailable\n");
        return 1;
    }

    for (int i = 0; i < 5; i++) {
        float t = (float)i / 4.0f;
        clearColor(0.1f, 0.2f + 0.3f * t, 0.9f, 1.0f);
        clear(GL_COLOR_BUFFER_BIT);
        finish();
        if (!swapBuffers(dpy, surf)) {
            fprintf(stderr, "egl-app: eglSwapBuffers failed at frame %d (0x%x)\n", i, getError());
            return 1;
        }
    }

    printf("egl-app: 5 frames swapped (GPU -> dma-buf -> Wayland)\n");
    return 0;
}
