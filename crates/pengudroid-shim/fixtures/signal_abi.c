/* Android 13 Bionic LP64 sigaction ABI. Kept independent of the Rust wrapper.
 * Layout source: platform/bionic libc/include/bits/signal_types.h, LP64 branch.
 */
#include <dlfcn.h>
#include <stdint.h>
#include <signal.h>
#include <stddef.h>
#include <stdio.h>
#include <errno.h>
#include <unistd.h>
#include <string.h>
#include <sys/wait.h>
#include <stdlib.h>
struct android_sigaction {
    int flags;
    void (*handler)(int);
    uint64_t mask;
    void (*restorer)(void);
};
_Static_assert(sizeof(struct android_sigaction) == 32, "LP64 sigaction size");
_Static_assert(offsetof(struct android_sigaction, handler) == 8, "handler offset");
static volatile sig_atomic_t handled;
static void receive_signal(int sig) { handled = sig; }
static int waitinfo_probe(void) {
    typedef int (*mask_fn)(int, const uint64_t*, uint64_t*);
    typedef int (*wait_fn)(const uint64_t*, siginfo_t*);
    typedef int (*timed_fn)(const uint64_t*, siginfo_t*, const struct timespec*);
    mask_fn mask = (mask_fn)dlsym(RTLD_DEFAULT, "pthread_sigmask64");
    timed_fn timed = (timed_fn)dlsym(RTLD_DEFAULT, "sigtimedwait64");
    if (!mask || !timed) return 10;
    struct { uint64_t bits; uint64_t guard; } set = { UINT64_C(1) << 38, UINT64_C(0xabcddcba) };
    uint64_t old = 0;
    if (mask(SIG_BLOCK, &set.bits, &old)) return 11;
    for (int i = 0; i < 2; ++i) {
        wait_fn waitinfo = (wait_fn)dlsym(RTLD_DEFAULT, i ? "sigwaitinfo64" : "sigwaitinfo");
        if (!waitinfo) return 12;
        union sigval value = { .sival_int = 1234 + i };
        if (sigqueue(getpid(), 39, value)) return 13;
        struct { siginfo_t info; uint64_t guard; } result;
        memset(&result, 0, sizeof(result)); result.guard = UINT64_C(0x12344321);
        if (waitinfo(&set.bits, &result.info) != 39 || result.info.si_signo != 39
            || result.info.si_pid != getpid() || result.info.si_value.sival_int != 1234 + i
            || result.guard != UINT64_C(0x12344321) || set.guard != UINT64_C(0xabcddcba)) return 14;
    }
    struct timespec zero = {0};
    errno = 0;
    if (timed(&set.bits, NULL, &zero) != -1 || errno != EAGAIN) return 15;
    zero.tv_nsec = 1000000000;
    errno = 0;
    if (timed(&set.bits, NULL, &zero) != -1 || errno != EINVAL) return 16;
    if (mask(SIG_SETMASK, &old, NULL)) return 17;
    puts("Android LP64 waitinfo payload and timed wait passed");
    return 0;
}
/* Canary reset is allowed only when no protected old frame will return.
 * Keep this helper unprotected and exit the forked child directly. */
__attribute__((no_stack_protector, noreturn)) static void guard_child(void) {
    void (*reset)(void) = (void (*)(void))dlsym(RTLD_DEFAULT, "android_reset_stack_guards");
    uintptr_t *global = (uintptr_t*)dlsym(RTLD_DEFAULT, "__stack_chk_guard");
    uintptr_t before, after;
    __asm__ volatile("mov %%fs:0x28, %0" : "=r"(before));
    if (!reset || !global || *global != before) _exit(21);
    int (*setprop)(const char*, const char*) = (void*)dlsym(RTLD_DEFAULT, "__system_property_set");
    int (*getprop)(const char*, char*) = (void*)dlsym(RTLD_DEFAULT, "__system_property_get");
    int (*reload)(void) = (void*)dlsym(RTLD_DEFAULT, "__system_properties_zygote_reload");
    char buffer[92] = {0};
    if (!setprop || !getprop || !reload) _exit(25);
    if (setprop("persist.pengu.compat_probe", "base")
        || setprop("ro.appcompat_override.persist.pengu.compat_probe", "override")) _exit(26);
    getprop("persist.pengu.compat_probe", buffer);
    if (strcmp(buffer, "base") || reload()) _exit(27);
    getprop("persist.pengu.compat_probe", buffer);
    if (strcmp(buffer, "override") || !getenv("BIONIC_APPCOMPAT_OVERRIDE")) _exit(28);
    reset();
    __asm__ volatile("mov %%fs:0x28, %0" : "=r"(after));
    if (after == before || !after || *global != after) _exit(22);
    _exit(0);
}
static int guard_probe(void) {
    pid_t child = fork();
    if (child < 0) return 23;
    if (!child) guard_child();
    int status;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status)) return 24;
    puts("Android LP64 child stack guard reset passed");
    return 0;
}
int main(void) {
    typedef int (*guest_sigaction)(int, const struct android_sigaction*, struct android_sigaction*);
    guest_sigaction install = (guest_sigaction)dlsym(RTLD_DEFAULT, "sigaction");
    if (!install) return 2;
    struct android_sigaction action = { .flags = 0, .handler = receive_signal, .mask = 0, .restorer = 0 };
    struct android_sigaction previous = {0};
    if (install(34, &action, &previous) != 0) return 3;
    if (raise(34) != 0) return 4;
    if (install(34, &previous, 0) != 0) return 5;
    printf("Android LP64 handler received signal %d\n", (int)handled);
    if (handled != 34) return 6;
    int result = waitinfo_probe();
    if (result) return result;
    return guard_probe();
}
