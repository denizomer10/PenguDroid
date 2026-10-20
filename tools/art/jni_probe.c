/*
 * Minimal ART bring-up probe.
 *
 *   jni_probe <libart.so> [vm-option ...]
 *
 * dlopens libart.so and calls JNI_CreateJavaVM with the given options, so the
 * caller controls LD_LIBRARY_PATH / LD_PRELOAD entirely from the environment.
 * No jni.h dependency: the JNI invocation types are declared here.
 */
#include <dlfcn.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

/* The Bionic shim interposes fprintf/vfprintf, so diagnostics go through raw
 * write(2) with hand-built strings. */
static void say(const char *a, const char *b, const char *c) {
    char buf[1024];
    size_t n = 0;
    const char *parts[3] = {a, b, c};
    for (int i = 0; i < 3; i++) {
        for (const char *p = parts[i]; p && *p && n < sizeof(buf) - 1; p++) buf[n++] = *p;
    }
    buf[n++] = '\n';
    (void)!write(2, buf, n);
}

typedef int jint;
typedef unsigned char jboolean;

typedef struct JavaVMOption {
    char *optionString;
    void *extraInfo;
} JavaVMOption;

typedef struct JavaVMInitArgs {
    jint version;
    jint nOptions;
    JavaVMOption *options;
    jboolean ignoreUnrecognized;
} JavaVMInitArgs;

typedef jint (*CreateJavaVM_t)(void **pvm, void **penv, void *args);

int main(int argc, char **argv) {
    if (argc < 2) {
        say("usage: ", argv[0], " <libart.so> [vm-option ...]");
        return 2;
    }
    void *h = dlopen(argv[1], RTLD_NOW | RTLD_GLOBAL);
    if (!h) {
        say("dlopen failed: ", dlerror(), "");
        return 3;
    }
    CreateJavaVM_t create = (CreateJavaVM_t)dlsym(h, "JNI_CreateJavaVM");
    if (!create) {
        say("dlsym(JNI_CreateJavaVM) failed: ", dlerror(), "");
        return 4;
    }

    JavaVMOption opts[16];
    int n = 0;
    for (int i = 2; i < argc && n < 16; i++) {
        opts[n].optionString = argv[i];
        opts[n].extraInfo = 0;
        n++;
    }

    JavaVMInitArgs args;
    memset(&args, 0, sizeof(args));
    args.version = 0x00010006; /* JNI_VERSION_1_6 */
    args.nOptions = n;
    args.options = opts;
    args.ignoreUnrecognized = 0;

    void *vm = 0, *env = 0;
    jint r = create(&vm, &env, &args);
    char num[32];
    int i = 0, v = r;
    num[i++] = '0' + (v % 10);
    num[i] = 0;
    say("JNI_CreateJavaVM -> ", num, r == 0 ? " (ok)" : " (failed)");
    return r == 0 ? 0 : 1;
}
