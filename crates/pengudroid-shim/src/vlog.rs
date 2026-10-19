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
"#;

/// The translation unit `build.rs` compiles.
#[allow(dead_code)]
pub fn source() -> String {
    VLOG_C.to_string()
}
