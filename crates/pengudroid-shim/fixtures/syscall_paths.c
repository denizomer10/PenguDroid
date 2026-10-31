/* GNU x86_64 syscall ABI probe. No Android runtime or production shim needed. */
#define _GNU_SOURCE
#include <dirent.h>
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/vfs.h>
#include <sys/wait.h>
#include <unistd.h>

#define CHECK(x) do { if (!(x)) { fprintf(stderr, "%s:%d: %s errno=%d\n", __FILE__, __LINE__, #x, errno); exit(2); } } while (0)
extern long host_syscall(long, ...);
__asm__(".symver host_syscall, syscall@GLIBC_2.2.5");
#ifndef PD_HOST_ONLY
extern size_t __pd_probe_counts(int which);
static const char *arm_path = "/system/bin/arm64/app_process64";
static const char *generic_path = "/system/lib64/pd_syscall_paths.so";
static const char *link_path = "/system/lib64/pd_syscall_paths.link";

static long six(long nr, const unsigned long a[6]) {
    return syscall(nr, a[0], a[1], a[2], a[3], a[4], a[5]);
}
static void check_stat_path(const char *path, long size) {
    struct { struct stat stat; uint64_t guard; } result;
    memset(&result, 0xa5, sizeof(result));
    errno = E2BIG;
    long r = syscall(SYS_newfstatat, AT_FDCWD, path, &result.stat, 0);
    if (size < 0) CHECK(r == -1 && errno == ENOENT);
    else CHECK(r == 0 && errno == E2BIG && result.stat.st_size == size);
    CHECK(result.guard == UINT64_C(0xa5a5a5a5a5a5a5a5));
    struct { struct statx stat; uint64_t guard; } x;
    memset(&x, 0xa5, sizeof(x));
    errno = E2BIG;
    r = syscall(SYS_statx, AT_FDCWD, path, 0, STATX_TYPE | STATX_SIZE, &x.stat);
    if (size < 0) CHECK(r == -1 && errno == ENOENT);
    else CHECK(r == 0 && errno == E2BIG && x.stat.stx_size == (uint64_t)size
               && (x.stat.stx_mask & STATX_SIZE));
    CHECK(x.guard == UINT64_C(0xa5a5a5a5a5a5a5a5));
}

static void read_only_families(long generic_size) {
    struct stat st;
    struct statx sx;
    struct statfs fs;
    if (generic_size < 0) {
        errno = 0;
        CHECK(syscall(SYS_stat, generic_path, &st) == -1 && errno == ENOENT);
        return;
    }
    CHECK(syscall(SYS_stat, generic_path, &st) == 0 && st.st_size == generic_size);
    CHECK(syscall(SYS_lstat, link_path, &st) == 0 && S_ISLNK(st.st_mode));
    CHECK(syscall(SYS_newfstatat, AT_FDCWD, link_path, &st, AT_SYMLINK_NOFOLLOW) == 0 && S_ISLNK(st.st_mode));
    CHECK(syscall(SYS_statx, AT_FDCWD, link_path, AT_SYMLINK_NOFOLLOW, STATX_TYPE, &sx) == 0 && S_ISLNK(sx.stx_mode));
    CHECK(syscall(SYS_statfs, generic_path, &fs) == 0 && fs.f_bsize > 0);
    CHECK(syscall(SYS_access, generic_path, R_OK) == 0);
    CHECK(syscall(SYS_faccessat, AT_FDCWD, generic_path, R_OK) == 0);
    CHECK(syscall(SYS_faccessat2, AT_FDCWD, generic_path, R_OK, AT_EACCESS) == 0);
    errno = 0;
    CHECK(syscall(SYS_access, generic_path, X_OK) == -1 && errno == EACCES);
    const char target[] = "pd_syscall_paths.so";
    char buffer[64]; memset(buffer, 0xa5, sizeof(buffer));
    CHECK(syscall(SYS_readlink, link_path, buffer, 3) == 3 && memcmp(buffer, target, 3) == 0);
    CHECK((unsigned char)buffer[3] == 0xa5);
    CHECK(syscall(SYS_readlinkat, AT_FDCWD, link_path, buffer, sizeof(buffer)) == (long)strlen(target));
    CHECK(memcmp(buffer, target, strlen(target)) == 0 && (unsigned char)buffer[strlen(target)] == 0xa5);
}

static void relative_and_errors(void) {
    char directory[] = "relative-XXXXXX";
    CHECK(mkdtemp(directory) != NULL);
    int dirfd = open(directory, O_RDONLY | O_DIRECTORY);
    CHECK(dirfd >= 0);
    int fd = openat(dirfd, "file", O_CREAT | O_RDWR | O_EXCL, 0600);
    CHECK(fd >= 0 && write(fd, "relative", 8) == 8);
    struct stat st;
    struct statx sx;
    CHECK(syscall(SYS_newfstatat, dirfd, "file", &st, 0) == 0 && st.st_size == 8);
    CHECK(syscall(SYS_statx, dirfd, "file", 0, STATX_SIZE, &sx) == 0 && sx.stx_size == 8);
    CHECK(syscall(SYS_faccessat2, dirfd, "file", F_OK, 0) == 0);
    CHECK(syscall(SYS_newfstatat, fd, "", &st, AT_EMPTY_PATH) == 0 && st.st_size == 8);
    CHECK(syscall(SYS_statx, fd, "", AT_EMPTY_PATH, STATX_SIZE, &sx) == 0 && sx.stx_size == 8);
    long baseline = host_syscall(SYS_newfstatat, fd, NULL, &st, AT_EMPTY_PATH);
    int baseline_errno = errno;
    CHECK(syscall(SYS_newfstatat, fd, NULL, &st, AT_EMPTY_PATH) == baseline);
    if (baseline < 0) CHECK(errno == baseline_errno);
    CHECK(syscall(SYS_newfstatat, -1, "file", &st, 0) == -1 && errno == EBADF);
    CHECK(syscall(SYS_statx, -1, "file", 0, STATX_SIZE, &sx) == -1 && errno == EBADF);
    CHECK(syscall(SYS_newfstatat, AT_FDCWD, "/proc/self/exe", (void *)1, 0) == -1 && errno == EFAULT);
    for (int bad_flags = 0; bad_flags < 2; ++bad_flags) {
        const int flags = bad_flags ? -1 : 0;
        baseline = host_syscall(SYS_newfstatat, AT_FDCWD, (void *)1, &st, flags);
        baseline_errno = errno;
        CHECK(syscall(SYS_newfstatat, AT_FDCWD, (void *)1, &st, flags) == baseline && errno == baseline_errno);
        baseline = host_syscall(SYS_statx, AT_FDCWD, (void *)1, flags, STATX_SIZE, &sx);
        baseline_errno = errno;
        CHECK(syscall(SYS_statx, AT_FDCWD, (void *)1, flags, STATX_SIZE, &sx) == baseline && errno == baseline_errno);
    }
    char long_path[8192]; memset(long_path, 'x', sizeof(long_path)); long_path[0] = '/'; long_path[8191] = 0;
    baseline = host_syscall(SYS_newfstatat, AT_FDCWD, long_path, &st, 0); baseline_errno = errno;
    CHECK(syscall(SYS_newfstatat, AT_FDCWD, long_path, &st, 0) == baseline && errno == baseline_errno);
    const size_t page = (size_t)sysconf(_SC_PAGESIZE);
    char *mapping = mmap(NULL, page * 2, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(mapping != MAP_FAILED && mprotect(mapping + page, page, PROT_NONE) == 0);
    const char path[] = "/proc/self/exe";
    char *at_guard = mapping + page - sizeof(path); memcpy(at_guard, path, sizeof(path));
    CHECK(syscall(SYS_newfstatat, AT_FDCWD, at_guard, &st, 0) == 0);
    CHECK(munmap(mapping, page * 2) == 0);
    CHECK(unlinkat(dirfd, "file", 0) == 0 && close(fd) == 0 && close(dirfd) == 0 && rmdir(directory) == 0);
    puts("relative dirfd, empty/NULL paths, guarded strings, EFAULT/error precedence and overlong paths passed");
}

/* Fail with EILSEQ if even an UNUSED high half / sixth argument changed. */
static void filter_arguments(long nr, const unsigned long expected[6], int path_index) {
    struct sock_filter f[64]; size_t n = 0;
    f[n++] = (struct sock_filter)BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr));
    f[n++] = (struct sock_filter)BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, (unsigned)nr, 1, 0);
    f[n++] = (struct sock_filter)BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW);
    for (int arg = 0; arg < 6; ++arg) if (arg != path_index) {
        for (int half = 0; half < 2; ++half) {
            unsigned offset = (unsigned)offsetof(struct seccomp_data, args) + (unsigned)arg * 8 + (unsigned)half * 4;
            f[n++] = (struct sock_filter)BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offset);
            f[n++] = (struct sock_filter)BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, (unsigned)(expected[arg] >> (half * 32)), 1, 0);
            f[n++] = (struct sock_filter)BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EILSEQ);
        }
    }
    f[n++] = (struct sock_filter)BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EDQUOT);
    struct sock_fprog program = {.len = (unsigned short)n, .filter = f};
    CHECK(prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == 0);
    CHECK(prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &program) == 0);
}
static void arg_child(long nr) {
    struct stat st; struct statx sx;
    unsigned long a[6] = {0x11112222fffffff7UL, (unsigned long)arm_path,
        0x2222333300000100UL, 0x3333444400000200UL, (unsigned long)&sx, 0xdeadbeefbad0cafeUL};
    int path_index = 1;
    if (nr == SYS_newfstatat) { a[2] = (unsigned long)&st; a[3] = 0x3333444400000100UL; a[4] = 0xcafef00dfeedbad0UL; }
    if (nr == SYS_stat) { path_index = 0; a[0] = (unsigned long)arm_path; a[1] = (unsigned long)&st; }
    filter_arguments(nr, a, path_index);
    CHECK(six(nr, a) == -1 && errno == EDQUOT);
}
static void denied_copy_child(void) {
    unsigned long ignored[6] = {0};
    filter_arguments(SYS_process_vm_readv, ignored, -1); /* Denies self-copy, all other calls allowed. */
    struct stat st;
    errno = E2BIG;
    CHECK(syscall(SYS_newfstatat, AT_FDCWD, "/proc/self/exe", &st, 0) == 0 && errno == E2BIG);
}
static void child_test(int which) {
    pid_t child = fork(); CHECK(child >= 0);
    if (!child) {
        if (which == 3) denied_copy_child();
        else arg_child(which == 0 ? SYS_newfstatat : which == 1 ? SYS_statx : SYS_stat);
        _exit(0);
    }
    int status; CHECK(waitpid(child, &status, 0) == child);
    CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 0);
}

static volatile sig_atomic_t received;
static void signal_handler(int signal) { received = signal; }
extern void pd_syscall_restorer(void);
__asm__(".text\n.globl pd_syscall_restorer\n.type pd_syscall_restorer,@function\n"
        "pd_syscall_restorer:\nendbr64\nmov $15,%edi\njmp syscall@PLT\n"
        ".size pd_syscall_restorer,.-pd_syscall_restorer\n");
static void sigreturn_test(void) {
    struct kernel_action { void (*handler)(int); unsigned long flags; void (*restorer)(void); uint64_t mask; };
    struct kernel_action action = {signal_handler, 0x04000000, pd_syscall_restorer, 0}, old;
    CHECK(host_syscall(SYS_rt_sigaction, SIGUSR1, &action, &old, 8) == 0);
    const size_t before = __pd_probe_counts(0);
    volatile uint64_t guard = UINT64_C(0x1234567887654321);
    CHECK(raise(SIGUSR1) == 0 && received == SIGUSR1);
    CHECK(guard == UINT64_C(0x1234567887654321) && __pd_probe_counts(0) == before);
    CHECK(host_syscall(SYS_rt_sigaction, SIGUSR1, &old, NULL, 8) == 0);
    puts("real rt_sigreturn via syscall@LIBC restorer tail path passed");
}

static void passthrough(long arm_size) {
    CHECK(syscall(SYS_getpid) == getpid());
    CHECK(syscall(-1L) == -1 && errno == ENOSYS);
    int fd = memfd_create("syscall-paths", MFD_CLOEXEC); CHECK(fd >= 0);
    const long page = sysconf(_SC_PAGESIZE); CHECK(ftruncate(fd, page * 2) == 0);
    CHECK(pwrite(fd, "offset-six", 10, page) == 10);
    char *p = (void *)syscall(SYS_mmap, NULL, page, PROT_READ, MAP_PRIVATE, fd, page);
    CHECK(p != MAP_FAILED && memcmp(p, "offset-six", 10) == 0);
    CHECK(syscall(SYS_munmap, p, page) == 0 && close(fd) == 0);
    char cwd[4096]; CHECK(syscall(79L, cwd, sizeof(cwd)) > 0); /* Host getcwd, NOT ARM64 newfstatat. */
    errno = 0; CHECK(syscall(SYS_openat, AT_FDCWD, generic_path, O_RDONLY, 0) == -1 && errno == ENOENT);
    errno = 0; CHECK(syscall(SYS_unlinkat, AT_FDCWD, arm_path, 0) == -1 && errno == ENOENT);
    check_stat_path(arm_path, arm_size); /* Excluded mutation must not delete mapped file. */
    puts("zero/six-argument passthrough, host syscall numbers, and excluded open/mutation calls passed");
}
#endif

static void host_paths(void) {
    struct stat st, raw;
    CHECK(stat("/proc/self/exe", &st) == 0);
    CHECK(syscall(SYS_newfstatat, AT_FDCWD, "/proc/self/exe", &raw, 0) == 0);
    CHECK(raw.st_ino == st.st_ino && raw.st_dev == st.st_dev && raw.st_size == st.st_size);
    CHECK(host_syscall(SYS_newfstatat, AT_FDCWD, "/proc/self/exe", &raw, 0) == 0 && raw.st_ino == st.st_ino);
    char a[4096], b[4096];
    long n = syscall(SYS_readlink, "/proc/self/exe", a, sizeof(a));
    CHECK(n > 0 && host_syscall(SYS_readlink, "/proc/self/exe", b, sizeof(b)) == n && memcmp(a, b, (size_t)n) == 0);
    puts("host stat/libc/versioned syscall and /proc/self/exe behavior passed");
}
int main(int argc, char **argv) {
    host_paths();
#ifndef PD_HOST_ONLY
    CHECK(argc == 4);
    const long arm_size = strtol(argv[1], NULL, 10), generic_size = strtol(argv[2], NULL, 10);
    check_stat_path(arm_path, arm_size);
    check_stat_path(generic_path, generic_size);
    check_stat_path("/system/bin/arm64/only-in-primary", strtol(argv[3], NULL, 10));
    read_only_families(generic_size);
    relative_and_errors();
    for (int i = 0; i < 4; ++i) child_test(i);
    puts("kernel seccomp verified all six argument words/high halves; denied pathname-copy fallback passed");
    sigreturn_test();
    passthrough(arm_size);
    if (arm_size >= 0) for (int i = 0; i < 256; ++i) check_stat_path(arm_path, arm_size);
    CHECK(__pd_probe_counts(0) == __pd_probe_counts(1));
    puts("all pathname cases and allocation/free balance passed (cleanup intentionally clobbers errno)");
#else
    (void)argc; (void)argv;
#endif
    return 0;
}
