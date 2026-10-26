/* Android 13 Bionic LP64 sigaction ABI. Kept independent of the Rust wrapper.
 * Layout source: platform/bionic libc/include/bits/signal_types.h, LP64 branch.
 */
#include <dlfcn.h>
#include <stdint.h>
#include <signal.h>
#include <stddef.h>
#include <stdio.h>
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
    return handled == 34 ? 0 : 6;
}
