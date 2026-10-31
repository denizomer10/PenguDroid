//! Read-only pathname adaptation at the GNU/Linux x86_64 LP64 syscall boundary.
//!
//! Digitalis has already translated guest syscall numbers, structures and
//! /proc/self/exe identity before calling the host's variadic `syscall` import.
//! Its newfstatat/statx calls bypass rootfs.rs's ordinary libc wrappers.
//!
//! Like pathshim.rs, this module only generates native sources for build.rs.
//! The assembly entry is linked into the cdylib ONLY and exported as syscall
//! under LIBC. It does not interpose versioned syscall@GLIBC_2.2.5 callers.
//! Every unselected call is a tail jump to that exact glibc version, with the
//! original stack and all arguments intact. In particular rt_sigreturn must
//! NEVER acquire a C/Rust wrapper frame, nor may clone/exec acquire new policy.
//!
//! Selected calls snapshot six integer words in assembly, not C va_arg or a
//! fixed-arity Rust imitation of a variadic function. The generated C router
//! changes only the designated pathname, then executes the same host syscall
//! directly. CString allocation/free is owned by the existing rootfs helpers;
//! errno is restored after both mapping and cleanup, on success and failure.
//!
//! A syscall accepts invalid user pointers. A bounded, page-split self
//! process_vm_readv prevents passing one to rootfs's CStr reader. Relative,
//! empty, NULL, unreadable and overlong paths retain the original kernel call,
//! including dirfd/AT_EMPTY_PATH semantics and error precedence. If self reads
//! are denied/unsupported, calls also remain unchanged: mapping then cannot be
//! provided. This is not an address-space sandbox, a symlink jail, an ARM
//! syscall-number converter, or an async-signal-safe replacement for rootfs.

// (host syscall number, zero-based pathname argument). Keep C and assembly in
// sync by generating both from this one allowlist. No open or mutation calls.
const PATH_CALLS: &[(u32, usize)] = &[
    (4, 0),   // stat
    (6, 0),   // lstat
    (21, 0),  // access
    (89, 0),  // readlink
    (137, 0), // statfs
    (262, 1), // newfstatat
    (267, 1), // readlinkat
    (269, 1), // faccessat
    (332, 1), // statx
    (439, 1), // faccessat2
];

#[allow(dead_code)]
pub const SYMBOLS: &[&str] = &["syscall"];

#[allow(dead_code)]
pub fn assembly() -> String {
    let mut out = String::from(
        ".section .text.pengudroid_syscall_paths,\"ax\",@progbits\n\
         .p2align 4\n.globl syscall\n.type syscall,@function\nsyscall:\n    .cfi_startproc\n    endbr64\n",
    );
    for (nr, _) in PATH_CALLS {
        out.push_str(&format!("    cmpq ${nr}, %rdi\n    je .Lpd_path_syscall\n"));
    }
    out.push_str(r#"
    jmp __pengudroid_glibc_syscall@PLT
.Lpd_path_syscall:
    // Entry: nr=rdi; a1..a5=rsi,rdx,rcx,r8,r9; a6=8(rsp).
    // 48-byte snapshot + 8-byte padding aligns the stack before the C call.
    subq $56, %rsp
    .cfi_def_cfa_offset 64
    movq %rsi, 0(%rsp)
    movq %rdx, 8(%rsp)
    movq %rcx, 16(%rsp)
    movq %r8, 24(%rsp)
    movq %r9, 32(%rsp)
    movq 64(%rsp), %rax
    movq %rax, 40(%rsp)
    movq %rsp, %rsi
    call __pengudroid_syscall_paths
    addq $56, %rsp
    .cfi_def_cfa_offset 8
    ret
    .cfi_endproc
.size syscall, .-syscall
.symver __pengudroid_glibc_syscall, syscall@GLIBC_2.2.5
.section .note.GNU-stack,"",@progbits
"#);
    out
}

const ROUTER_C: &str = r#"
#include <errno.h>
#include <stdint.h>
#include <string.h>
#include <sys/uio.h>

extern char *__pengudroid_map_path(const char *);
extern void __pengudroid_free_path(char *);

static long pd_raw_syscall(long nr, const unsigned long a[6]) {
    register unsigned long a4 __asm__("r10") = a[3];
    register unsigned long a5 __asm__("r8") = a[4];
    register unsigned long a6 __asm__("r9") = a[5];
    long result;
    __asm__ volatile("syscall" : "=a"(result)
        : "a"(nr), "D"(a[0]), "S"(a[1]), "d"(a[2]), "r"(a4), "r"(a5), "r"(a6)
        : "rcx", "r11", "memory", "cc");
    return result;
}

/* x86_64 Linux has 4-KiB base pages, including when a mapping uses huge pages.
 * Split remote iovecs at those boundaries so a NUL immediately before a guard
 * page is accepted, without trying to read the inaccessible following page.
 * Return 1 only for a complete absolute pathname; otherwise preserve the raw
 * kernel call (and its own EFAULT/ENAMETOOLONG/EINVAL/EBADF precedence).
 */
static int pd_absolute_path(const char *path, char out[4096]) {
    if (!path) return 0;
    const unsigned long zero[6] = {0};
    long pid = pd_raw_syscall(39, zero); /* getpid, not a cached pre-fork PID */
    if (pid <= 0) return 0;
    size_t used = 0;
    while (used < 4096) {
        uintptr_t address = (uintptr_t)path + used;
        if (address < (uintptr_t)path) return 0;
        size_t length = 4096 - (address & 4095);
        if (length > 4096 - used) length = 4096 - used;
        struct iovec local = {out + used, length};
        struct iovec remote = {(void *)address, length};
        unsigned long a[6] = {(unsigned long)pid, (unsigned long)&local, 1,
                             (unsigned long)&remote, 1, 0};
        long n = pd_raw_syscall(310, a); /* process_vm_readv(self, ...) */
        if (n <= 0 || (unsigned long)n > length) return 0;
        if (out[0] != '/') return 0;
        if (memchr(out + used, 0, (size_t)n)) return 1;
        used += (size_t)n;
    }
    return 0;
}

__attribute__((visibility("hidden")))
long __pengudroid_syscall_paths(long nr, const unsigned long saved[6]) {
    int *error = __errno_location();
    const int incoming_errno = *error;
    unsigned long a[6];
    for (int i = 0; i < 6; ++i) a[i] = saved[i];
    int path_index = -1;
    switch (nr) {
        /* PD_PATH_CASES */
    }
    char path[4096];
    char *mapped = 0;
    if (path_index >= 0 && pd_absolute_path((const char *)a[path_index], path)) {
        mapped = __pengudroid_map_path(path);
        if (mapped) a[path_index] = (unsigned long)mapped;
    }
    const long raw = pd_raw_syscall(nr, a);
    const int failed = (unsigned long)raw >= (unsigned long)-4095L;
    const int result_errno = failed ? (int)-raw : incoming_errno;
    __pengudroid_free_path(mapped);
    *error = result_errno;
    return failed ? -1 : raw;
}
"#;

#[allow(dead_code)]
pub fn source() -> String {
    let mut cases = String::new();
    for (nr, index) in PATH_CALLS {
        cases.push_str(&format!("case {nr}: path_index = {index}; break;\n"));
    }
    ROUTER_C.replace("/* PD_PATH_CASES */", &cases)
}
