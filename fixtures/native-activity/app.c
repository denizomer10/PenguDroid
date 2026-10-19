/*
 * A minimal NativeActivity app: it renders a spinning triangle with EGL/GLES
 * and swaps frames through PenguDroid's Bionic EGL shim.
 *
 * It is deliberately self-contained (no NDK headers): the struct layouts and
 * the EGL/GLES entry points are declared here, exactly as an NDK app would see
 * them. Every symbol is resolved at load time from the preloaded shim, so the
 * library itself links against nothing.
 *
 * Build: ./build.sh   (produces libmain.so)
 * Run:   pengudroid native --lib libmain.so
 */

#include <pthread.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

/* ------------------------------------------------------------------ */
/* android/native_activity.h (the subset this app uses)                */
/* ------------------------------------------------------------------ */

typedef struct ANativeActivity ANativeActivity;
typedef struct ANativeWindow ANativeWindow;
typedef struct AAssetManager AAssetManager;
typedef struct ARect { int32_t left, top, right, bottom; } ARect;

typedef struct ANativeActivityCallbacks {
    void (*onStart)(ANativeActivity*);
    void (*onResume)(ANativeActivity*);
    void* (*onSaveInstanceState)(ANativeActivity*, size_t*);
    void (*onPause)(ANativeActivity*);
    void (*onStop)(ANativeActivity*);
    void (*onDestroy)(ANativeActivity*);
    void (*onWindowFocusChanged)(ANativeActivity*, int);
    void (*onNativeWindowCreated)(ANativeActivity*, ANativeWindow*);
    void (*onNativeWindowResized)(ANativeActivity*, ANativeWindow*);
    void (*onNativeWindowRedrawNeeded)(ANativeActivity*, ANativeWindow*);
    void (*onNativeWindowDestroyed)(ANativeActivity*, ANativeWindow*);
    void (*onInputQueueCreated)(ANativeActivity*, void*);
    void (*onInputQueueDestroyed)(ANativeActivity*, void*);
    void (*onContentRectChanged)(ANativeActivity*, const ARect*);
    void (*onConfigurationChanged)(ANativeActivity*);
    void (*onLowMemory)(ANativeActivity*);
} ANativeActivityCallbacks;

struct ANativeActivity {
    ANativeActivityCallbacks* callbacks;
    void* vm;
    void* env;
    void* clazz;
    const char* internalDataPath;
    const char* externalDataPath;
    int32_t sdkVersion;
    void* instance;
    AAssetManager* assetManager;
    const char* obbPath;
};

extern void ANativeActivity_finish(ANativeActivity*);
extern int32_t ANativeWindow_getWidth(ANativeWindow*);
extern int32_t ANativeWindow_getHeight(ANativeWindow*);
extern void* AAssetManager_open(AAssetManager*, const char*, int);
extern int64_t AAsset_getLength(void*);
extern const void* AAsset_getBuffer(void*);
extern void AAsset_close(void*);

/* ------------------------------------------------------------------ */
/* EGL / GLES2 (declared, resolved from the preloaded shim)            */
/* ------------------------------------------------------------------ */

typedef void* EGLDisplay;
typedef void* EGLConfig;
typedef void* EGLContext;
typedef void* EGLSurface;
typedef int EGLint;
typedef unsigned int EGLBoolean;

#define EGL_DEFAULT_DISPLAY ((void*)0)
#define EGL_NO_CONTEXT ((void*)0)
#define EGL_NO_SURFACE ((void*)0)
#define EGL_TRUE 1
#define EGL_FALSE 0
#define EGL_OPENGL_ES_API 0x30A0
#define EGL_SURFACE_TYPE 0x3033
#define EGL_WINDOW_BIT 0x0004
#define EGL_RENDERABLE_TYPE 0x3040
#define EGL_OPENGL_ES2_BIT 0x0004
#define EGL_RED_SIZE 0x3024
#define EGL_GREEN_SIZE 0x3023
#define EGL_BLUE_SIZE 0x3022
#define EGL_ALPHA_SIZE 0x3021
#define EGL_DEPTH_SIZE 0x3025
#define EGL_NONE 0x3038
#define EGL_CONTEXT_CLIENT_VERSION 0x3098

extern EGLDisplay eglGetDisplay(void*);
extern EGLBoolean eglInitialize(EGLDisplay, EGLint*, EGLint*);
extern EGLBoolean eglBindAPI(unsigned int);
extern EGLBoolean eglChooseConfig(EGLDisplay, const EGLint*, EGLConfig*, EGLint, EGLint*);
extern EGLContext eglCreateContext(EGLDisplay, EGLConfig, EGLContext, const EGLint*);
extern EGLSurface eglCreateWindowSurface(EGLDisplay, EGLConfig, void*, const EGLint*);
extern EGLBoolean eglMakeCurrent(EGLDisplay, EGLSurface, EGLSurface, EGLContext);
extern EGLBoolean eglSwapBuffers(EGLDisplay, EGLSurface);
extern EGLint eglGetError(void);

typedef unsigned int GLuint;
typedef int GLint;
typedef unsigned int GLenum;
typedef float GLfloat;

#define GL_COLOR_BUFFER_BIT 0x4000
#define GL_VERTEX_SHADER 0x8B31
#define GL_FRAGMENT_SHADER 0x8B30
#define GL_COMPILE_STATUS 0x8B81
#define GL_LINK_STATUS 0x8B82
#define GL_TRIANGLES 0x0004
#define GL_FLOAT 0x1406
#define GL_FALSE 0
#define GL_ARRAY_BUFFER 0x8892
#define GL_DYNAMIC_DRAW 0x88E8

extern void glViewport(GLint, GLint, GLint, GLint);
extern void glClearColor(GLfloat, GLfloat, GLfloat, GLfloat);
extern void glClear(GLenum);
extern GLuint glCreateShader(GLenum);
extern void glShaderSource(GLuint, GLint, const char* const*, const GLint*);
extern void glCompileShader(GLuint);
extern void glGetShaderiv(GLuint, GLenum, GLint*);
extern GLuint glCreateProgram(void);
extern void glAttachShader(GLuint, GLuint);
extern void glLinkProgram(GLuint);
extern void glGetProgramiv(GLuint, GLenum, GLint*);
extern void glUseProgram(GLuint);
extern GLint glGetAttribLocation(GLuint, const char*);
extern void glGenBuffers(GLint, GLuint*);
extern void glBindBuffer(GLenum, GLuint);
extern void glBufferData(GLenum, long, const void*, GLenum);
extern void glVertexAttribPointer(GLuint, GLint, GLenum, unsigned char, GLint, const void*);
extern void glEnableVertexAttribArray(GLuint);
extern void glDrawArrays(GLenum, GLint, GLint);

/* ------------------------------------------------------------------ */
/* The app                                                             */
/* ------------------------------------------------------------------ */

static ANativeActivity* g_activity;
static ANativeWindow* g_window;
static pthread_mutex_t g_lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t g_cond = PTHREAD_COND_INITIALIZER;
static int g_have_window;
static int g_stop;
static int g_frames;

static const char* VERTEX_SRC =
    "attribute vec2 a_pos;\n"
    "void main() { gl_Position = vec4(a_pos, 0.0, 1.0); }\n";

static const char* FRAGMENT_SRC =
    "precision mediump float;\n"
    "void main() { gl_FragColor = vec4(1.0, 0.55, 0.1, 1.0); }\n";

static GLuint compile(GLenum type, const char* src) {
    GLuint sh = glCreateShader(type);
    glShaderSource(sh, 1, &src, NULL);
    glCompileShader(sh);
    GLint ok = 0;
    glGetShaderiv(sh, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        fprintf(stderr, "native-app: shader compile failed (type %u)\n", type);
    }
    return sh;
}

static void* render_thread(void* arg) {
    (void)arg;
    pthread_mutex_lock(&g_lock);
    while (!g_have_window && !g_stop) {
        pthread_cond_wait(&g_cond, &g_lock);
    }
    ANativeWindow* window = g_window;
    pthread_mutex_unlock(&g_lock);
    if (!window) {
        return NULL;
    }

    EGLDisplay dpy = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    EGLint major = 0, minor = 0;
    if (!eglInitialize(dpy, &major, &minor)) {
        fprintf(stderr, "native-app: eglInitialize failed (0x%x)\n", eglGetError());
        return NULL;
    }
    eglBindAPI(EGL_OPENGL_ES_API);

    const EGLint cfg_attribs[] = {
        EGL_SURFACE_TYPE, EGL_WINDOW_BIT,
        EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8,
        EGL_DEPTH_SIZE, 0,
        EGL_NONE,
    };
    EGLConfig cfg = NULL;
    EGLint n = 0;
    if (!eglChooseConfig(dpy, cfg_attribs, &cfg, 1, &n) || n < 1) {
        fprintf(stderr, "native-app: eglChooseConfig found no config (0x%x)\n", eglGetError());
        return NULL;
    }

    const EGLint ctx_attribs[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, ctx_attribs);
    if (!ctx) {
        fprintf(stderr, "native-app: eglCreateContext failed (0x%x)\n", eglGetError());
        return NULL;
    }
    EGLSurface surf = eglCreateWindowSurface(dpy, cfg, window, NULL);
    if (!surf) {
        fprintf(stderr, "native-app: eglCreateWindowSurface failed (0x%x)\n", eglGetError());
        return NULL;
    }
    if (!eglMakeCurrent(dpy, surf, surf, ctx)) {
        fprintf(stderr, "native-app: eglMakeCurrent failed (0x%x)\n", eglGetError());
        return NULL;
    }

    GLuint vs = compile(GL_VERTEX_SHADER, VERTEX_SRC);
    GLuint fs = compile(GL_FRAGMENT_SHADER, FRAGMENT_SRC);
    GLuint prog = glCreateProgram();
    glAttachShader(prog, vs);
    glAttachShader(prog, fs);
    glLinkProgram(prog);
    GLint linked = 0;
    glGetProgramiv(prog, GL_LINK_STATUS, &linked);
    if (!linked) {
        fprintf(stderr, "native-app: program link failed\n");
        return NULL;
    }
    glUseProgram(prog);
    GLint pos = glGetAttribLocation(prog, "a_pos");

    GLuint vbo = 0;
    glGenBuffers(1, &vbo);
    glBindBuffer(GL_ARRAY_BUFFER, vbo);
    glEnableVertexAttribArray((GLuint)pos);
    glVertexAttribPointer((GLuint)pos, 2, GL_FLOAT, GL_FALSE, 0, NULL);

    int w = ANativeWindow_getWidth(window);
    int h = ANativeWindow_getHeight(window);
    if (w <= 0) w = 1280;
    if (h <= 0) h = 720;
    glViewport(0, 0, w, h);
    fprintf(stderr, "native-app: rendering %dx%d\n", w, h);

    float angle = 0.0f;
    while (!g_stop) {
        /* Rotate a triangle on the CPU so no uniform plumbing is needed. */
        float x = angle * 0.0174533f;
        if (x > 3.14159f) x -= 6.28318f;
        float sn = x - x * x * x / 6.0f + x * x * x * x * x / 120.0f;
        float cs = 1.0f - x * x / 2.0f + x * x * x * x / 24.0f;

        const float base[6] = { -0.6f, -0.5f, 0.6f, -0.5f, 0.0f, 0.6f };
        float verts[6];
        for (int i = 0; i < 3; i++) {
            float vx = base[i * 2], vy = base[i * 2 + 1];
            verts[i * 2] = vx * cs - vy * sn;
            verts[i * 2 + 1] = vx * sn + vy * cs;
        }
        glBufferData(GL_ARRAY_BUFFER, sizeof(verts), verts, GL_DYNAMIC_DRAW);

        float t = (float)(g_frames % 120) / 120.0f;
        glClearColor(0.05f + t * 0.15f, 0.06f, 0.12f + t * 0.2f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        glDrawArrays(GL_TRIANGLES, 0, 3);
        eglSwapBuffers(dpy, surf);

        g_frames++;
        angle += 3.0f;
        if (angle > 360.0f) angle -= 360.0f;
        if (g_frames >= 90) {
            break;
        }
        usleep(16000);
    }
    fprintf(stderr, "native-app: rendered %d frames\n", g_frames);
    /* Ask the host to end the app once enough frames have been presented. */
    if (g_activity) {
        ANativeActivity_finish(g_activity);
    }
    return NULL;
}

static void on_native_window_created(ANativeActivity* activity, ANativeWindow* window) {
    (void)activity;
    pthread_mutex_lock(&g_lock);
    g_window = window;
    g_have_window = 1;
    pthread_cond_signal(&g_cond);
    pthread_mutex_unlock(&g_lock);
}

static void on_native_window_destroyed(ANativeActivity* activity, ANativeWindow* window) {
    (void)activity;
    (void)window;
    g_stop = 1;
}

static void on_destroy(ANativeActivity* activity) {
    (void)activity;
    g_stop = 1;
}

void ANativeActivity_onCreate(ANativeActivity* activity, void* saved_state, size_t saved_size) {
    (void)saved_state;
    (void)saved_size;
    g_activity = activity;

    /* Install our callbacks (the host calls them). */
    activity->callbacks->onNativeWindowCreated = on_native_window_created;
    activity->callbacks->onNativeWindowDestroyed = on_native_window_destroyed;
    activity->callbacks->onDestroy = on_destroy;

    /* Prove APK assets are reachable when the app was launched from an APK. */
    if (activity->assetManager) {
        void* asset = AAssetManager_open(activity->assetManager, "assets/hello.txt", 0);
        if (asset) {
            int64_t len = AAsset_getLength(asset);
            const char* data = (const char*)AAsset_getBuffer(asset);
            fprintf(stderr, "native-app: asset assets/hello.txt = %lld bytes: %.*s\n",
                    (long long)len, (int)(len > 64 ? 64 : len), data ? data : "");
            AAsset_close(asset);
        } else {
            fprintf(stderr, "native-app: no assets/hello.txt in the APK\n");
        }
    }

    /* Return immediately: the host calls onNativeWindowCreated next, which is
     * what unblocks the render thread. */
    pthread_t thread;
    pthread_create(&thread, NULL, render_thread, NULL);
    pthread_detach(thread);
}
