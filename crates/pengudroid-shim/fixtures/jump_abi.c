/* Independent LP64 ABI probe. See tests/jump_abi.py; no Android/ART runtime. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <pthread.h>
#include <setjmp.h>
#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

_Static_assert(sizeof(jmp_buf) == 200, "host glibc LP64 jump ABI");
_Static_assert(offsetof(struct __jmp_buf_tag, __mask_was_saved) == 64, "host flag");
_Static_assert(sizeof(long[11]) == 88 && sizeof(long[12]) == 96, "Bionic sizes");

#define CHECK(expr) do { if (!(expr)) { \
    fprintf(stderr, "%s:%d: %s (errno=%d)\n", __FILE__, __LINE__, #expr, errno); \
    exit(2); \
} } while (0)

static uint64_t get_mask(void) {
    uint64_t mask = 0;
    CHECK(syscall(SYS_rt_sigprocmask, 0, NULL, &mask, 8) == 0);
    return mask;
}
static void set_mask(uint64_t mask) {
    CHECK(syscall(SYS_rt_sigprocmask, SIG_SETMASK, &mask, NULL, 8) == 0);
}

typedef void (*jump_fn)(void *, int);
static jump_fn dynamic_jump;

/* Ordinary header-based calls, plus an unversioned dlsym caller, must all
 * retain glibc semantics. Padding at offset 68 is intentionally nonzero. */
static void host_case(int save_mask, int kind, int value) {
    struct {
        uint64_t before;
        sigjmp_buf env;
        uint64_t after;
    } storage;
    memset(&storage, 0xa5, sizeof(storage));
    const uint64_t guard = UINT64_C(0xa5a5a5a5a5a5a5a5);
    const uint64_t original = get_mask();
    const uint64_t baseline = UINT64_C(1) << (SIGUSR1 - 1);
    const uint64_t changed = UINT64_C(1) << (SIGUSR2 - 1);
    set_mask(baseline);
    volatile int visits = 0;
    int result = save_mask < 0 ? setjmp(storage.env) : sigsetjmp(storage.env, save_mask);
    if (result == 0) {
        ++visits;
        set_mask(changed);
        switch (kind) {
        case 0: siglongjmp(storage.env, value);
        case 1: longjmp(storage.env, value);
        case 2: _longjmp(storage.env, value);
        default: dynamic_jump(storage.env, value); __builtin_unreachable();
        }
    }
    const uint64_t observed = get_mask();
    set_mask(original);
    CHECK(visits == 1 && result == (value ? value : 1));
    CHECK(observed == (save_mask > 0 ? baseline : changed));
    CHECK(storage.before == guard && storage.after == guard);
}

static void host_tests(void) {
    const char *names[] = { "siglongjmp", "longjmp", "_longjmp" };
    for (int n = 0; n < 3; ++n) {
        dynamic_jump = (jump_fn)dlsym(RTLD_DEFAULT, names[n]);
        CHECK(dynamic_jump != NULL);
        for (int save = -1; save <= 1; ++save)
            for (int kind = 0; kind < 4; ++kind) {
                host_case(save, kind, 0);
                host_case(save, kind, -7);
            }
    }
    puts("ordinary glibc header and unversioned callers passed");
}

#ifndef PD_HOST_ONLY
/* Do not call a function pointer for the saving side: the compiler needs the
 * actual returns_twice declaration. Host sigsetjmp remains its header macro. */
extern int bionic_save(void *, int) __asm__("sigsetjmp") __attribute__((returns_twice));
extern void bionic_sigjump(void *, int) __asm__("siglongjmp") __attribute__((noreturn));
extern void bionic_jump(void *, int) __asm__("longjmp") __attribute__((noreturn));
extern void bionic__jump(void *, int) __asm__("_longjmp") __attribute__((noreturn));

struct guarded {
    unsigned char *allocation, *env;
    size_t page, size;
    int leading;
};
static struct guarded guarded_buffer(size_t size, int leading) {
    const size_t page = (size_t)sysconf(_SC_PAGESIZE);
    unsigned char *mapping = mmap(NULL, page * 3, PROT_NONE,
                                 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(mapping != MAP_FAILED);
    CHECK(mprotect(mapping + page, page, PROT_READ | PROT_WRITE) == 0);
    memset(mapping + page, 0xa5, page);
    return (struct guarded){ mapping, mapping + (leading ? page : page * 2 - size),
                             page, size, leading };
}
static void check_buffer(const struct guarded *buffer) {
    const unsigned char *canary = buffer->leading ? buffer->env + buffer->size
                                                 : buffer->env - 32;
    for (int i = 0; i < 32; ++i) CHECK(canary[i] == 0xa5);
    for (size_t i = 88; i < buffer->size; ++i) CHECK(buffer->env[i] == 0xa5);
}
static void release_buffer(struct guarded *buffer) {
    check_buffer(buffer);
    CHECK(munmap(buffer->allocation, buffer->page * 3) == 0);
}

__attribute__((noreturn, noinline))
static void nested_stack_jump(void *env, int value, int kind, unsigned depth) {
    volatile unsigned char pad[257];
    memset((void *)pad, 0x5a, sizeof(pad));
    __asm__ volatile("" : : "m"(pad) : "memory");
    if (depth) nested_stack_jump(env, value, kind, depth - 1);
    switch (kind) {
    case 0: bionic_sigjump(env, value);
    case 1: bionic_jump(env, value);
    default: bionic__jump(env, value);
    }
}

static void compact_case(size_t size, int leading, int save_mask, int kind, int value) {
    struct guarded buffer = guarded_buffer(size, leading);
    const uint64_t original = get_mask();
    /* Include glibc's reserved signals 32/33: a glibc sigset_t wrapper must
     * never see this compact mask (some paths copy all 128 bytes). */
    const uint64_t baseline = (UINT64_C(1) << 38) | (UINT64_C(3) << 31)
                            | (UINT64_C(1) << (SIGUSR1 - 1));
    const uint64_t changed = UINT64_C(1) << (SIGUSR2 - 1);
    set_mask(baseline);
    volatile int visits = 0;
    errno = EDOM;
    int result = bionic_save(buffer.env, save_mask);
    CHECK(errno == EDOM);
    if (result == 0) {
        ++visits;
        check_buffer(&buffer);
        set_mask(changed);
        nested_stack_jump(buffer.env, value, kind, 4);
    }
    const uint64_t observed = get_mask();
    set_mask(original);
    CHECK(visits == 1 && result == (value ? value : 1));
    CHECK(observed == (save_mask ? baseline : changed));
    release_buffer(&buffer);
}

/* Deliberately clobber EVERY callee-saved GPR between save and restore, even
 * rbp under -fno-omit-frame-pointer. The saved rsp must recover this frame too. */
extern int pd_jump_register_probe(void);
__asm__(
    ".pushsection .text\n"
    ".globl pd_jump_register_probe\n"
    ".type pd_jump_register_probe,@function\n"
    "pd_jump_register_probe:\n"
    "endbr64\n"
    "push %rbx\n push %rbp\n push %r12\n push %r13\n push %r14\n push %r15\n"
    "sub $104,%rsp\n"
    "movq $0x12345678,88(%rsp)\n"
    "mov $0x11223344,%ebx\n mov $0x22334455,%ebp\n"
    "mov $0x33445566,%r12d\n mov $0x44556677,%r13d\n"
    "mov $0x55667788,%r14d\n mov $0x66778899,%r15d\n"
    "mov %rsp,%rdi\n xor %esi,%esi\n call sigsetjmp@PLT\n"
    "test %eax,%eax\n jnz 1f\n"
    "xor %ebx,%ebx\n xor %ebp,%ebp\n xor %r12d,%r12d\n"
    "xor %r13d,%r13d\n xor %r14d,%r14d\n xor %r15d,%r15d\n"
    "mov %rsp,%rdi\n mov $73,%esi\n call siglongjmp@PLT\n ud2\n"
    "1: cmp $73,%eax\n jne 2f\n"
    "cmp $0x11223344,%rbx\n jne 2f\n cmp $0x22334455,%rbp\n jne 2f\n"
    "cmp $0x33445566,%r12\n jne 2f\n cmp $0x44556677,%r13\n jne 2f\n"
    "cmp $0x55667788,%r14\n jne 2f\n cmp $0x66778899,%r15\n jne 2f\n"
    "cmpq $0x12345678,88(%rsp)\n jne 2f\n xor %eax,%eax\n jmp 3f\n"
    "2: mov $1,%eax\n"
    "3: add $104,%rsp\n"
    "pop %r15\n pop %r14\n pop %r13\n pop %r12\n pop %rbp\n pop %rbx\n ret\n"
    ".size pd_jump_register_probe,.-pd_jump_register_probe\n"
    ".popsection\n");

static void inner_boundary(void *outer) {
    struct guarded inner = guarded_buffer(88, 0);
    int result = bionic_save(inner.env, 0);
    if (!result) nested_stack_jump(inner.env, 5, 0, 3);
    CHECK(result == 5);
    release_buffer(&inner);
    bionic_sigjump(outer, 9);
}
static void nested_boundaries(void) {
    struct guarded outer = guarded_buffer(88, 0);
    int result = bionic_save(outer.env, 0);
    if (!result) inner_boundary(outer.env);
    CHECK(result == 9);
    release_buffer(&outer);
}

static void *signal_env;
static void receive_signal(int signum) { bionic_sigjump(signal_env, signum); }
static void signal_case(int save_mask) {
    struct guarded buffer = guarded_buffer(88, 0);
    const uint64_t original = get_mask();
    struct sigaction action = { .sa_handler = receive_signal }, previous;
    CHECK(sigemptyset(&action.sa_mask) == 0);
    CHECK(sigaction(SIGUSR1, &action, &previous) == 0);
    set_mask(0);
    signal_env = buffer.env;
    int result = bionic_save(buffer.env, save_mask);
    if (!result) { CHECK(raise(SIGUSR1) == 0); CHECK(0); }
    CHECK(result == SIGUSR1);
    CHECK(get_mask() == (save_mask ? 0 : UINT64_C(1) << (SIGUSR1 - 1)));
    set_mask(original);
    CHECK(sigaction(SIGUSR1, &previous, NULL) == 0);
    release_buffer(&buffer);
}

static void *thread_probe(void *unused) {
    (void)unused;
    for (int i = 0; i < 32; ++i) compact_case(i & 1 ? 88 : 96, i & 1, i & 1, i % 3, i);
    return NULL;
}

static void deny_syscall(int number) {
    struct sock_filter filter[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, (unsigned)number, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EPERM),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };
    struct sock_fprog program = { .len = 4, .filter = filter };
    CHECK(prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == 0);
    CHECK(prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &program) == 0);
}
static void denied_status(void) {
    deny_syscall(SYS_arch_prctl);
    (void)bionic_save((void *)1, 0); /* Must reject CET uncertainty BEFORE writing. */
}
static void denied_mask_save(void) {
    long env[11];
    deny_syscall(SYS_rt_sigprocmask);
    (void)bionic_save(env, 1);
}
static void denied_mask_restore(void) {
    long env[11];
    if (!bionic_save(env, 1)) {
        deny_syscall(SYS_rt_sigprocmask);
        bionic_sigjump(env, 1);
    }
}
static void denied_status_restore(void) {
    long env[11];
    if (!bionic_save(env, 0)) {
        deny_syscall(SYS_arch_prctl);
        bionic_sigjump(env, 1);
    }
}
static void no_mask_syscall_when_unsaved(void) {
    long env[11];
    deny_syscall(SYS_rt_sigprocmask);
    int result = bionic_save(env, 0);
    if (!result) bionic_sigjump(env, 6);
    _exit(result == 6 ? 0 : 98);
}
static void corrupt_context(void) {
    uint64_t env[11];
    if (!bionic_save(env, 0)) { env[10] ^= 1; bionic_sigjump(env, 1); }
}
static void foreign_context(void) {
    uint64_t env[11] = {0};
    env[8] = UINT64_C(0xdeadbeef);
    bionic_sigjump(env, 1);
}
static void host_with_denied_status(void) {
    sigjmp_buf env;
    int result = sigsetjmp(env, 0);
    if (!result) {
        deny_syscall(SYS_arch_prctl);
        dynamic_jump(env, 4);
        __builtin_unreachable();
    }
    _exit(result == 4 ? 0 : 98);
}

/* Enabling SHSTK in libc's syscall() wrapper would return into a frame absent
 * from the newly enabled shadow stack. Keep the syscall in this very frame. */
__attribute__((always_inline)) static inline long enable_shstk(void) {
    long result;
    __asm__ volatile("syscall" : "=a"(result)
                     : "a"((long)SYS_arch_prctl), "D"(0x5001L), "S"(1L)
                     : "rcx", "r11", "memory");
    return result;
}
static void active_cet_save(void) {
    if (enable_shstk() != 0) _exit(77);
    (void)bionic_save((void *)1, 0);
}
static void active_cet_restore(void) {
    long env[11];
    if (!bionic_save(env, 0)) {
        if (enable_shstk() != 0) _exit(77);
        bionic_sigjump(env, 1);
    }
}

static void child_case(void (*probe)(void), int expected, const char *message, int optional) {
    int output[2];
    CHECK(pipe(output) == 0);
    pid_t child = fork();
    CHECK(child >= 0);
    if (!child) {
        close(output[0]);
        CHECK(dup2(output[1], STDERR_FILENO) == STDERR_FILENO);
        close(output[1]);
        probe();
        _exit(99);
    }
    close(output[1]);
    char diagnostic[1024] = {0};
    size_t length = 0;
    ssize_t n;
    while ((n = read(output[0], diagnostic + length, sizeof(diagnostic) - length - 1)) > 0)
        length += (size_t)n;
    close(output[0]);
    int status;
    CHECK(waitpid(child, &status, 0) == child);
    if (optional && WIFEXITED(status) && WEXITSTATUS(status) == 77) {
        puts("SKIP live CET enable: host kernel/CPU/policy does not permit SHSTK");
        return;
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != expected || !strstr(diagnostic, message)) {
        fprintf(stderr, "child status=%d expected=%d diagnostic=%s\n", status, expected, diagnostic);
        CHECK(0);
    }
}

static void compact_tests(void) {
    const int values[] = { 0, 37, -19 };
    const int masks[] = { 0, 1, 3 };
    for (int size = 88; size <= 96; size += 8)
        for (int leading = 0; leading < 2; ++leading)
            for (int mask = 0; mask < 3; ++mask)
                for (int kind = 0; kind < 3; ++kind)
                    for (int value = 0; value < 3; ++value)
                        compact_case((size_t)size, leading, masks[mask], kind, values[value]);
    CHECK(pd_jump_register_probe() == 0);
    nested_boundaries();
    signal_case(0);
    signal_case(1);
    pthread_t threads[4];
    for (int i = 0; i < 4; ++i) CHECK(pthread_create(&threads[i], NULL, thread_probe, NULL) == 0);
    for (int i = 0; i < 4; ++i) CHECK(pthread_join(threads[i], NULL) == 0);
    puts("exact 88/96-byte guard pages/canaries, returns-twice, registers, masks, nesting, signals and threads passed");

    child_case(denied_status, 127, "CET shadow stacks", 0);
    child_case(denied_status_restore, 127, "CET shadow stacks", 0);
    child_case(no_mask_syscall_when_unsaved, 0, "", 0);
    child_case(denied_mask_save, 127, "rt_sigprocmask failed", 0);
    child_case(denied_mask_restore, 127, "rt_sigprocmask failed", 0);
    child_case(corrupt_context, 127, "invalid compact jump context", 0);
    child_case(foreign_context, 127, "invalid compact jump context", 0);
    child_case(host_with_denied_status, 0, "", 0);
    child_case(active_cet_save, 127, "CET shadow stacks", 1);
    child_case(active_cet_restore, 127, "CET shadow stacks", 1);
    puts("fail-closed syscall/CET/context checks and independent host dispatch passed");
}
#endif

int main(void) {
    host_tests();
#ifndef PD_HOST_ONLY
    compact_tests();
#endif
    return 0;
}
