//! Minimal C variadic support for the Bionic log entry points.
//!
//! Rust cannot *define* C-variadic functions (`...`) on stable, but it can call
//! variadic C functions. So the variadic entry points are written as thin C
//! shims — the compiler collects the arguments — which `vsnprintf` the format
//! string (a `va_list` is opaque to stable Rust) and hand the finished line to
//! Rust for emission. Bionic's `printf` expansion is therefore real rather than
//! a literal echo.
//!
//! NOTE: `build.rs` includes this file and calls `source()`. Keep it
//! self-contained (no crate imports, no `#[cfg(test)]` code). It intentionally
//! avoids libc headers: `vsnprintf` is declared here and resolved from the host
//! C library, and `NULL` is spelled `0`.

const VLOG_C: &str = r#"
typedef __builtin_va_list __pd_va_list;
extern int vsnprintf(char *buf, __SIZE_TYPE__ size, const char *fmt, __pd_va_list ap);

extern void __pengudroid_log_rendered(int prio, const char *tag, const char *rendered);
extern void __pengudroid_assert_message(const char *cond, const char *tag, const char *rendered)
    __attribute__((noreturn));

/*
 * `__android_log_print(prio, tag, fmt, ...)`
 *
 * Formats into a stack buffer; when the result does not fit, formats again into
 * memory the shim's own allocator (`__pengudroid_alloc`) provides, so the shim
 * works whether it is preloaded into a glibc or a musl process.
 */
int __android_log_print(int prio, const char *tag, const char *fmt, ...) {
    char stack[1024];
    if (fmt == 0) {
        return -1;
    }

    __pd_va_list ap;
    __builtin_va_start(ap, fmt);
    int n = vsnprintf(stack, sizeof(stack), fmt, ap);
    __builtin_va_end(ap);
    if (n < 0) {
        /* Malformed format: log the literal string, as a lenient logger would. */
        __pengudroid_log_rendered(prio, tag, fmt);
        return -1;
    }
    if ((__SIZE_TYPE__)n >= sizeof(stack)) {
        extern char *__pengudroid_alloc(__SIZE_TYPE__);
        extern void __pengudroid_free(char *);
        char *heap = __pengudroid_alloc((__SIZE_TYPE__)n + 1);
        if (heap == 0) {
            return -1;
        }
        __builtin_va_start(ap, fmt);
        vsnprintf(heap, (__SIZE_TYPE__)n + 1, fmt, ap);
        __builtin_va_end(ap);
        __pengudroid_log_rendered(prio, tag, heap);
        __pengudroid_free(heap);
        return n;
    }
    __pengudroid_log_rendered(prio, tag, stack);
    return n;
}

/* `__android_log_assert(cond, tag, fmt, ...)` — noreturn. */
void __android_log_assert(const char *cond, const char *tag, const char *fmt, ...) {
    char rendered[1024];
    rendered[0] = '\0';
    if (fmt != 0) {
        __pd_va_list ap;
        __builtin_va_start(ap, fmt);
        vsnprintf(rendered, sizeof(rendered), fmt, ap);
        __builtin_va_end(ap);
    }
    __pengudroid_assert_message(cond, tag, rendered);
}

/*
 * `__android_log_buf_print(buf_id, prio, tag, fmt, ...)`
 *
 * The `buf_id` selects one of Bionic's log buffers (main, radio, ...).
 * PenguDroid has a single sink, so it is ignored and the line is rendered like
 * `__android_log_print`.
 */
int __android_log_buf_print(int buf_id, int prio, const char *tag, const char *fmt, ...) {
    (void)buf_id;
    char stack[1024];
    if (fmt == 0) {
        return -1;
    }
    __pd_va_list ap;
    __builtin_va_start(ap, fmt);
    int n = vsnprintf(stack, sizeof(stack), fmt, ap);
    __builtin_va_end(ap);
    if (n < 0) {
        __pengudroid_log_rendered(prio, tag, fmt);
        return -1;
    }
    if ((__SIZE_TYPE__)n >= sizeof(stack)) {
        extern char *__pengudroid_alloc(__SIZE_TYPE__);
        extern void __pengudroid_free(char *);
        char *heap = __pengudroid_alloc((__SIZE_TYPE__)n + 1);
        if (heap == 0) {
            return -1;
        }
        __builtin_va_start(ap, fmt);
        vsnprintf(heap, (__SIZE_TYPE__)n + 1, fmt, ap);
        __builtin_va_end(ap);
        __pengudroid_log_rendered(prio, tag, heap);
        __pengudroid_free(heap);
        return n;
    }
    __pengudroid_log_rendered(prio, tag, stack);
    return n;
}

/*
 * `__android_log_bswrite(buf_id, tag, fmt, ...)`
 *
 * The "binary string write" variant; like `__android_log_buf_print` the buffer
 * id is ignored and the line is rendered to the single sink.
 */
int __android_log_bswrite(int buf_id, const char *tag, const char *fmt, ...) {
    (void)buf_id;
    char stack[1024];
    if (fmt == 0) {
        return -1;
    }
    __pd_va_list ap;
    __builtin_va_start(ap, fmt);
    int n = vsnprintf(stack, sizeof(stack), fmt, ap);
    __builtin_va_end(ap);
    if (n < 0) {
        __pengudroid_log_rendered(4, tag, fmt);
        return -1;
    }
    if ((__SIZE_TYPE__)n >= sizeof(stack)) {
        extern char *__pengudroid_alloc(__SIZE_TYPE__);
        extern void __pengudroid_free(char *);
        char *heap = __pengudroid_alloc((__SIZE_TYPE__)n + 1);
        if (heap == 0) {
            return -1;
        }
        __builtin_va_start(ap, fmt);
        vsnprintf(heap, (__SIZE_TYPE__)n + 1, fmt, ap);
        __builtin_va_end(ap);
        __pengudroid_log_rendered(4, tag, heap);
        __pengudroid_free(heap);
        return n;
    }
    __pengudroid_log_rendered(4, tag, stack);
    return n;
}

/*
 * Bionic `_FORTIFY_SOURCE` wrappers that are variadic.
 *
 * Bionic exports `__snprintf_chk`, `__printf_chk`, ... as ordinary symbols; a
 * caller compiled with `_FORTIFY_SOURCE` references them by name. They behave
 * like the underlying `printf` family, aborting only when the caller's claimed
 * buffer size is smaller than the requested length. PenguDroid forwards to the
 * host C library and skips the abort (a guest that overflows its own buffer is
 * the guest's bug, and aborting would take the whole host process down).
 */
typedef struct __pd_FILE __pd_FILE;
extern int vsprintf(char *buf, const char *fmt, __pd_va_list ap);
extern int vfprintf(__pd_FILE *stream, const char *fmt, __pd_va_list ap);
extern __pd_FILE *stdout;

int __snprintf_chk(char *buf, __SIZE_TYPE__ len, int flags, __SIZE_TYPE__ buf_size,
                   const char *fmt, ...) {
    (void)flags; (void)buf_size;
    __pd_va_list ap;
    __builtin_va_start(ap, fmt);
    int n = vsnprintf(buf, len, fmt, ap);
    __builtin_va_end(ap);
    return n;
}

int __vsnprintf_chk(char *buf, __SIZE_TYPE__ len, int flags, __SIZE_TYPE__ buf_size,
                    const char *fmt, __pd_va_list ap) {
    (void)flags; (void)buf_size;
    return vsnprintf(buf, len, fmt, ap);
}

int __sprintf_chk(char *buf, int flags, __SIZE_TYPE__ buf_size, const char *fmt, ...) {
    (void)flags; (void)buf_size;
    __pd_va_list ap;
    __builtin_va_start(ap, fmt);
    int n = vsprintf(buf, fmt, ap);
    __builtin_va_end(ap);
    return n;
}

int __vsprintf_chk(char *buf, int flags, __SIZE_TYPE__ buf_size, const char *fmt,
                   __pd_va_list ap) {
    (void)flags; (void)buf_size;
    return vsprintf(buf, fmt, ap);
}

int __printf_chk(int flags, const char *fmt, ...) {
    (void)flags;
    __pd_va_list ap;
    __builtin_va_start(ap, fmt);
    int n = vfprintf(stdout, fmt, ap);
    __builtin_va_end(ap);
    return n;
}

int __fprintf_chk(__pd_FILE *stream, int flags, const char *fmt, ...) {
    (void)flags;
    __pd_va_list ap;
    __builtin_va_start(ap, fmt);
    int n = vfprintf(stream, fmt, ap);
    __builtin_va_end(ap);
    return n;
}

int __vfprintf_chk(__pd_FILE *stream, int flags, const char *fmt, __pd_va_list ap) {
    (void)flags;
    return vfprintf(stream, fmt, ap);
}
"#;

/// Every symbol the C translation unit defines.
///
/// `build.rs` forces each one into the cdylib: the object is archived, nothing
/// in the Rust half references these symbols, and rustc's cdylib export list
/// does not know about them — without this list they would be dropped and the
/// shim would silently fail to export them.
#[allow(dead_code)]
pub const C_SYMBOLS: &[&str] = &[
    "__android_log_print",
    "__android_log_assert",
    "__android_log_buf_print",
    "__android_log_bswrite",
    "__snprintf_chk",
    "__vsnprintf_chk",
    "__sprintf_chk",
    "__vsprintf_chk",
    "__printf_chk",
    "__fprintf_chk",
    "__vfprintf_chk",
];

/// The translation unit `build.rs` compiles.
#[allow(dead_code)]
pub fn source() -> String {
    VLOG_C.to_string()
}
