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

/* Provided by rootfs.rs. */
extern char *__pengudroid_map_path(const char *path);
extern void __pengudroid_free_path(char *p);

extern long syscall(long number, ...);

#if defined(__x86_64__)
# define PD_SYS_OPENAT 257
# define PD_AT_FDCWD (-100)
#elif defined(__aarch64__)
# define PD_SYS_OPENAT 56
# define PD_AT_FDCWD (-100)
#elif defined(__i386__)
# define PD_SYS_OPENAT 295
# define PD_AT_FDCWD (-100)
#elif defined(__arm__)
# define PD_SYS_OPENAT 322
# define PD_AT_FDCWD (-100)
#else
# error "pengudroid path shim: unsupported architecture"
#endif

/* O_CREAT is 0100 on every Linux ABI. */
#define PD_O_CREAT 0100

static int pd_openat(int dirfd, const char *path, int flags, __pd_mode_t mode) {
    char *mapped = __pengudroid_map_path(path);
    const char *p = mapped != 0 ? mapped : path;
    long r = syscall(PD_SYS_OPENAT, dirfd, p, flags, mode);
    __pengudroid_free_path(mapped);
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
"#;

/// Every symbol this translation unit defines.
#[allow(dead_code)]
pub const C_SYMBOLS: &[&str] = &["open", "open64", "openat", "openat64"];

/// The translation unit `build.rs` compiles.
#[allow(dead_code)]
pub fn source() -> String {
    PATHSHIM_C.to_string()
}
