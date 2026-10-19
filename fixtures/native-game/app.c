/*
 * PenguDroid Invaders — a small but real game, running as an Android
 * NativeActivity with no ART.
 *
 * It renders on the host GPU through PenguDroid's Bionic EGL/GLES shim (each
 * eglSwapBuffers becomes a dma-buf presented by the daemon on a Wayland
 * surface) and reads the keyboard from stdin, so it is playable:
 *
 *   A / D  or  left / right   move the paddle
 *   Q                         quit
 *
 * Build: ./build.sh
 * Run:   pengudroid native --lib libmain.so
 */

#include <pthread.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <termios.h>
#include <unistd.h>

/* ------------------------------------------------------------------ */
/* android/native_activity.h (subset)                                  */
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

/* ------------------------------------------------------------------ */
/* EGL / GLES2                                                         */
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
/* Game                                                                */
/* ------------------------------------------------------------------ */

#define COLS 10
#define ROWS 5
#define MAX_VERTS 4096

typedef struct { float x, y; } Vec2;

static ANativeActivity* g_activity;
static ANativeWindow* g_window;
static volatile int g_have_window;
static volatile int g_stop;
static volatile int g_left, g_right;

static float g_paddle_x = 0.0f;
static float g_ball_x = 0.0f, g_ball_y = 0.0f;
static float g_ball_vx = 0.55f, g_ball_vy = 0.75f;
static int g_bricks[ROWS][COLS];
static int g_score;
static int g_lives = 3;
static int g_frames;

static const char* VS =
    "attribute vec2 a_pos;\n"
    "attribute vec3 a_color;\n"
    "varying vec3 v_color;\n"
    "void main() { gl_Position = vec4(a_pos, 0.0, 1.0); v_color = a_color; }\n";
static const char* FS =
    "precision mediump float;\n"
    "varying vec3 v_color;\n"
    "void main() { gl_FragColor = vec4(v_color, 1.0); }\n";

static float g_verts[MAX_VERTS * 5];
static int g_nverts;

static void push_quad(float x, float y, float w, float h, float r, float g, float b) {
    float x0 = x - w / 2, x1 = x + w / 2, y0 = y - h / 2, y1 = y + h / 2;
    float v[30] = {
        x0, y0, r, g, b, x1, y0, r, g, b, x1, y1, r, g, b,
        x0, y0, r, g, b, x1, y1, r, g, b, x0, y1, r, g, b,
    };
    if (g_nverts + 6 > MAX_VERTS) return;
    memcpy(&g_verts[g_nverts * 5], v, sizeof(v));
    g_nverts += 6;
}

static void reset_ball(void) {
    g_ball_x = 0.0f;
    g_ball_y = -0.55f;
    g_ball_vx = 0.55f;
    g_ball_vy = 0.75f;
}

static void reset_game(void) {
    for (int r = 0; r < ROWS; r++)
        for (int c = 0; c < COLS; c++) g_bricks[r][c] = 1;
    g_score = 0;
    g_lives = 3;
    g_paddle_x = 0.0f;
    reset_ball();
}

static void step(void) {
    /* Paddle. */
    if (g_left) g_paddle_x -= 0.045f;
    if (g_right) g_paddle_x += 0.045f;
    if (g_paddle_x < -0.85f) g_paddle_x = -0.85f;
    if (g_paddle_x > 0.85f) g_paddle_x = 0.85f;

    /* Ball. */
    g_ball_x += g_ball_vx * 0.016f;
    g_ball_y += g_ball_vy * 0.016f;
    if (g_ball_x < -0.97f) { g_ball_x = -0.97f; g_ball_vx = -g_ball_vx; }
    if (g_ball_x > 0.97f) { g_ball_x = 0.97f; g_ball_vx = -g_ball_vx; }
    if (g_ball_y > 0.97f) { g_ball_y = 0.97f; g_ball_vy = -g_ball_vy; }

    /* Paddle collision. */
    if (g_ball_y < -0.80f && g_ball_y > -0.92f && g_ball_vy < 0) {
        if (g_ball_x > g_paddle_x - 0.18f && g_ball_x < g_paddle_x + 0.18f) {
            g_ball_vy = -g_ball_vy;
            g_ball_vx += (g_ball_x - g_paddle_x) * 1.5f;
            if (g_ball_vx > 1.1f) g_ball_vx = 1.1f;
            if (g_ball_vx < -1.1f) g_ball_vx = -1.1f;
        }
    }

    /* Bricks. */
    for (int r = 0; r < ROWS; r++) {
        for (int c = 0; c < COLS; c++) {
            if (!g_bricks[r][c]) continue;
            float bx = -0.9f + c * 0.2f;
            float by = 0.85f - r * 0.13f;
            if (g_ball_x > bx - 0.09f && g_ball_x < bx + 0.09f &&
                g_ball_y > by - 0.05f && g_ball_y < by + 0.05f) {
                g_bricks[r][c] = 0;
                g_ball_vy = -g_ball_vy;
                g_score += 10;
                return;
            }
        }
    }

    /* Lost ball. */
    if (g_ball_y < -1.05f) {
        g_lives--;
        if (g_lives <= 0) {
            reset_game();
        } else {
            reset_ball();
        }
    }
}

static void build_scene(void) {
    g_nverts = 0;
    /* Background is the clear colour; draw the bricks. */
    for (int r = 0; r < ROWS; r++) {
        for (int c = 0; c < COLS; c++) {
            if (!g_bricks[r][c]) continue;
            float bx = -0.9f + c * 0.2f;
            float by = 0.85f - r * 0.13f;
            float t = (float)r / ROWS;
            push_quad(bx, by, 0.17f, 0.09f, 0.25f + t * 0.6f, 0.35f, 0.95f - t * 0.5f);
        }
    }
    /* Paddle and ball. */
    push_quad(g_paddle_x, -0.86f, 0.36f, 0.05f, 0.95f, 0.85f, 0.2f);
    push_quad(g_ball_x, g_ball_y, 0.045f, 0.045f, 1.0f, 1.0f, 1.0f);
    /* Lives as small bars in the corner. */
    for (int i = 0; i < g_lives; i++) {
        push_quad(-0.92f + i * 0.06f, 0.95f, 0.04f, 0.03f, 0.3f, 1.0f, 0.4f);
    }
}

static void* render_thread(void* arg) {
    (void)arg;
    while (!g_have_window && !g_stop) usleep(5000);
    ANativeWindow* window = g_window;
    if (!window) return NULL;

    EGLDisplay dpy = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    EGLint major = 0, minor = 0;
    if (!eglInitialize(dpy, &major, &minor)) {
        fprintf(stderr, "game: eglInitialize failed (0x%x)\n", eglGetError());
        return NULL;
    }
    eglBindAPI(EGL_OPENGL_ES_API);
    const EGLint cfg_attribs[] = {
        EGL_SURFACE_TYPE, EGL_WINDOW_BIT,
        EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8,
        EGL_NONE,
    };
    EGLConfig cfg = NULL;
    EGLint n = 0;
    if (!eglChooseConfig(dpy, cfg_attribs, &cfg, 1, &n) || n < 1) {
        fprintf(stderr, "game: no EGL config (0x%x)\n", eglGetError());
        return NULL;
    }
    const EGLint ctx_attribs[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, ctx_attribs);
    EGLSurface surf = eglCreateWindowSurface(dpy, cfg, window, NULL);
    if (!ctx || !surf || !eglMakeCurrent(dpy, surf, surf, ctx)) {
        fprintf(stderr, "game: context/surface failed (0x%x)\n", eglGetError());
        return NULL;
    }

    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &VS, NULL);
    glCompileShader(vs);
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &FS, NULL);
    glCompileShader(fs);
    GLuint prog = glCreateProgram();
    glAttachShader(prog, vs);
    glAttachShader(prog, fs);
    glLinkProgram(prog);
    GLint ok = 0;
    glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) {
        fprintf(stderr, "game: shader link failed\n");
        return NULL;
    }
    glUseProgram(prog);
    GLint a_pos = glGetAttribLocation(prog, "a_pos");
    GLint a_color = glGetAttribLocation(prog, "a_color");
    GLuint vbo = 0;
    glGenBuffers(1, &vbo);
    glBindBuffer(GL_ARRAY_BUFFER, vbo);

    int w = ANativeWindow_getWidth(window);
    int h = ANativeWindow_getHeight(window);
    if (w <= 0) w = 1280;
    if (h <= 0) h = 720;
    glViewport(0, 0, w, h);
    fprintf(stderr, "game: PenguDroid Invaders %dx%d — A/D or arrows to move, Q to quit\n", w, h);

    reset_game();
    while (!g_stop) {
        step();
        build_scene();
        glBufferData(GL_ARRAY_BUFFER, (long)g_nverts * 5 * sizeof(float), g_verts, GL_DYNAMIC_DRAW);
        glEnableVertexAttribArray((GLuint)a_pos);
        glVertexAttribPointer((GLuint)a_pos, 2, GL_FLOAT, GL_FALSE, 5 * sizeof(float), NULL);
        glEnableVertexAttribArray((GLuint)a_color);
        glVertexAttribPointer((GLuint)a_color, 3, GL_FLOAT, GL_FALSE, 5 * sizeof(float),
                              (const void*)(2 * sizeof(float)));
        glClearColor(0.03f, 0.04f, 0.09f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        glDrawArrays(GL_TRIANGLES, 0, g_nverts);
        eglSwapBuffers(dpy, surf);
        g_frames++;
        usleep(16000);
    }
    fprintf(stderr, "game: %d frames, score %d\n", g_frames, g_score);
    if (g_activity) ANativeActivity_finish(g_activity);
    return NULL;
}

/* Keyboard: the host inherits the terminal, so the game can read it directly. */
static void* input_thread(void* arg) {
    (void)arg;
    struct termios old, raw;
    int have_term = tcgetattr(0, &old) == 0;
    if (have_term) {
        raw = old;
        raw.c_lflag &= ~(ICANON | ECHO);
        raw.c_cc[VMIN] = 0;
        raw.c_cc[VTIME] = 0;
        tcsetattr(0, TCSANOW, &raw);
    }
    while (!g_stop) {
        unsigned char c = 0;
        if (read(0, &c, 1) == 1) {
            if (c == 'a' || c == 'A') g_left = 1;
            else if (c == 'd' || c == 'D') g_right = 1;
            else if (c == 'q' || c == 'Q') g_stop = 1;
            else if (c == 27) {
                /* Arrow keys: ESC [ C/D. */
                unsigned char seq[2] = {0, 0};
                if (read(0, &seq[0], 1) == 1 && seq[0] == '[' &&
                    read(0, &seq[1], 1) == 1) {
                    if (seq[1] == 'C') g_right = 1;
                    else if (seq[1] == 'D') g_left = 1;
                }
            }
        } else {
            g_left = 0;
            g_right = 0;
            usleep(8000);
        }
    }
    if (have_term) tcsetattr(0, TCSANOW, &old);
    return NULL;
}

static void on_native_window_created(ANativeActivity* a, ANativeWindow* w) {
    (void)a;
    g_window = w;
    g_have_window = 1;
}

static void on_native_window_destroyed(ANativeActivity* a, ANativeWindow* w) {
    (void)a;
    (void)w;
    g_stop = 1;
}

static void on_destroy(ANativeActivity* a) {
    (void)a;
    g_stop = 1;
}

void ANativeActivity_onCreate(ANativeActivity* activity, void* saved_state, size_t saved_size) {
    (void)saved_state;
    (void)saved_size;
    g_activity = activity;
    activity->callbacks->onNativeWindowCreated = on_native_window_created;
    activity->callbacks->onNativeWindowDestroyed = on_native_window_destroyed;
    activity->callbacks->onDestroy = on_destroy;

    pthread_t t;
    pthread_create(&t, NULL, render_thread, NULL);
    pthread_detach(t);
    pthread_create(&t, NULL, input_thread, NULL);
    pthread_detach(t);
}
