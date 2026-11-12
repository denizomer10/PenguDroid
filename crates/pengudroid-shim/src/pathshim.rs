//! C shims for the *variadic* file entry points.
//!
//! `open` and `openat` take an optional `mode_t` that is only present when
//! `O_CREAT` (or `O_TMPFILE`) is set.  Stable Rust cannot define a C-variadic
//! function, so these four are written in C: they collect the optional mode and
//! issue the syscall directly.
//!
//! The syscall is used rather than a `dlsym(RTLD_NEXT)` call on purpose.  The
//! dynamic linker itself opens files, and a first call that resolved `open`
//! through `dlsym` could recurse before the cached pointer is set.  A direct
//! `openat(2)` has no such cycle, and the path has already been rewritten by
//! [`__pengudroid_map_path`](crate::rootfs).
//!
//! NOTE: `build.rs` includes this file and calls `source()`.  Keep it
//! self-contained (no crate imports).
//!
//! # Safety
//! The generated code is ordinary C; it is only compiled, never called from
//! Rust directly.

const PATHSHIM_C: &str = r#"
typedef __builtin_va_list __pd_va_list;
typedef unsigned int __pd_mode_t;

/* Provided by rootfs.rs and binder.rs. */
extern char *__pengudroid_map_path(const char *path);
extern void __pengudroid_free_path(char *p);
extern void __pengudroid_note_open(const char *path, int err);
extern int *__errno_location(void);
extern int __pengudroid_binder_open(const char *path);
extern long __pengudroid_binder_ioctl(int fd, unsigned long request, void *arg);
extern void __pengudroid_binder_mapping(int fd, void *addr, unsigned long len);
extern int __pengudroid_binder_mmap_prot(int fd, int prot);

#define PD_IOCTL_NOT_HANDLED (-4096L)

/* `_IOWR(0xAA, 0x3F, struct uffdio_api)`: the userfaultfd API handshake.  The
 * kernel answers with the features and ioctls it supports; ART then picks its
 * GC strategy from that answer. */
#define PD_UFFDIO_API 0xC018AA3F
/* Linux 6.8 added UFFDIO_MOVE, and 6.13 UFFD_FEATURE_MOVE advertises it.  ART's
 * mark-compact takes a separate "move" path when it sees that bit, and on this
 * host that path faults right after compaction.  Hide it so ART uses the
 * long-standing UFFDIO_COPY/POISON path. */
#define PD_UFFD_FEATURE_MOVE (1ULL << 16)
#define PD_UFFDIO_MOVE_BIT (1ULL << 5)

extern long syscall(long number, ...);
extern char *getenv(const char *name);

struct pd_uffdio_api {
    unsigned long long api;
    unsigned long long features;
    unsigned long long ioctls;
};

#if defined(__x86_64__)
# define PD_SYS_OPENAT 257
# define PD_SYS_IOCTL 16
# define PD_SYS_MMAP 9
# define PD_AT_FDCWD (-100)
#elif defined(__aarch64__)
# define PD_SYS_OPENAT 56
# define PD_SYS_IOCTL 29
# define PD_SYS_MMAP 222
# define PD_AT_FDCWD (-100)
#elif defined(__i386__)
# define PD_SYS_OPENAT 295
# define PD_SYS_IOCTL 54
# define PD_SYS_MMAP 90
# define PD_AT_FDCWD (-100)
#elif defined(__arm__)
# define PD_SYS_OPENAT 322
# define PD_SYS_IOCTL 54
# define PD_SYS_MMAP 192
# define PD_AT_FDCWD (-100)
#else
# error "pengudroid path shim: unsupported architecture"
#endif

/* O_CREAT is 0100 on every Linux ABI. */
#define PD_O_CREAT 0100

static int pd_openat(int dirfd, const char *path, int flags, __pd_mode_t mode) {
    /* Binder devices are emulated in-process; libbinder must never touch the
     * real (usually absent) driver. */
    int bfd = __pengudroid_binder_open(path);
    if (bfd >= 0) {
        return bfd;
    }
    char *mapped = __pengudroid_map_path(path);
    const char *p = mapped != 0 ? mapped : path;
    long r = syscall(PD_SYS_OPENAT, dirfd, p, flags, mode);
    if (r < 0) {
        __pengudroid_note_open(p, *__errno_location());
    }
    __pengudroid_free_path(mapped);
    return (int)r;
}

/* `ioctl(fd, request, ...)`
 *
 * Android's libbinder drives the kernel binder driver through ioctl; the
 * emulated device answers the control requests and refuses transactions for
 * now. Everything else is forwarded to the kernel with a raw syscall, which
 * also avoids any chance of recursing through dlsym.
 */
int ioctl(int fd, unsigned long request, ...) {
    __pd_va_list ap;
    __builtin_va_start(ap, request);
    void *arg = __builtin_va_arg(ap, void *);
    __builtin_va_end(ap);
    long handled = __pengudroid_binder_ioctl(fd, request, arg);
    if (handled != PD_IOCTL_NOT_HANDLED) {
        return (int)handled;
    }
    long r = syscall(PD_SYS_IOCTL, fd, request, arg);
    /* Report no UFFDIO_MOVE support (see the comment on PD_UFFD_FEATURE_MOVE).
     * PENGUDROID_UFFD_ALLOW_MOVE=1 keeps the kernel's real answer so the two
     * paths can be compared. */
    if (r == 0 && request == PD_UFFDIO_API && arg != 0 &&
        getenv("PENGUDROID_UFFD_ALLOW_MOVE") == 0) {
        struct pd_uffdio_api *api = (struct pd_uffdio_api *)arg;
        api->features &= ~PD_UFFD_FEATURE_MOVE;
        api->ioctls &= ~PD_UFFDIO_MOVE_BIT;
    }
    return (int)r;
}

static __pd_mode_t pd_mode_from_va(int flags, __pd_va_list *ap) {
    __pd_mode_t mode = 0;
    if ((flags & PD_O_CREAT) != 0) {
        mode = __builtin_va_arg(*ap, __pd_mode_t);
    }
    return mode;
}

int open(const char *path, int flags, ...) {
    __pd_va_list ap;
    __builtin_va_start(ap, flags);
    __pd_mode_t mode = pd_mode_from_va(flags, &ap);
    __builtin_va_end(ap);
    return pd_openat(PD_AT_FDCWD, path, flags, mode);
}

int open64(const char *path, int flags, ...) {
    __pd_va_list ap;
    __builtin_va_start(ap, flags);
    __pd_mode_t mode = pd_mode_from_va(flags, &ap);
    __builtin_va_end(ap);
    return pd_openat(PD_AT_FDCWD, path, flags, mode);
}

int openat(int dirfd, const char *path, int flags, ...) {
    __pd_va_list ap;
    __builtin_va_start(ap, flags);
    __pd_mode_t mode = pd_mode_from_va(flags, &ap);
    __builtin_va_end(ap);
    return pd_openat(dirfd, path, flags, mode);
}

int openat64(int dirfd, const char *path, int flags, ...) {
    __pd_va_list ap;
    __builtin_va_start(ap, flags);
    __pd_mode_t mode = pd_mode_from_va(flags, &ap);
    __builtin_va_end(ap);
    return pd_openat(dirfd, path, flags, mode);
}

/* `mmap(addr, len, prot, flags, fd, off)`
 *
 * Only the emulated binder device is interesting: libbinder maps the transfer
 * area and replies must be written inside that same mapping. Everything else
 * is a plain syscall. */
void *mmap(void *addr, __SIZE_TYPE__ len, int prot, int flags, int fd, long off) {
    /* The emulated device keeps its transfer area in a memfd. libbinder maps it
     * read-only, but replies have to be written into it, so ask for write
     * permission on the device fd only. */
    if (fd >= 0) {
        prot = __pengudroid_binder_mmap_prot(fd, prot);
    }
    long r = syscall(PD_SYS_MMAP, addr, len, prot, flags, fd, off);
    if (fd >= 0) {
        __pengudroid_binder_mapping(fd, (void *)r, (unsigned long)len);
    }
    return (void *)r;
}

void *mmap64(void *addr, __SIZE_TYPE__ len, int prot, int flags, int fd, long off) {
    return mmap(addr, len, prot, flags, fd, off);
}
"#;

/// Every symbol this translation unit defines.
#[allow(dead_code)]
pub const C_SYMBOLS: &[&str] = &[
    "open",
    "open64",
    "openat",
    "openat64",
    "ioctl",
    "mmap",
    "mmap64",
];

/// The translation unit `build.rs` compiles.
#[allow(dead_code)]
pub fn source() -> String {
    PATHSHIM_C.to_string()
}
