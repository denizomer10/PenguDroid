//! Compact nonlocal-jump boundary for the x86_64 Bionic translator.
//!
//! The pinned Bionic `06356e41c5ed7b12220b24c05ba4fdb873b126a7` declares
//! `jmp_buf[11]` (88 bytes) and `sigjmp_buf[12]` (96 bytes). Its private
//! `arch-x86_64/bionic/setjmp.S` uses words 0..7 for registers, 8 for a
//! cookie/flag, 9 for the 64-bit Linux signal mask, and 10 for a checksum.
//! We use those bounds, but our cookie/encoding is private to this shim: an
//! environment must be saved here and restored through one of our jump exports.
//! This does NOT implement Bionic's separately imported `setjmp`/`_setjmp`.
//!
//! glibc's `__sigsetjmp`, `setjmp`, and `_setjmp` are deliberately untouched.
//! Their buffers have a 32-bit mask-saved flag of 0 or 1 at offset 64; ours
//! have a distinct tag there. Genuine glibc environments are tail-dispatched
//! to `siglongjmp@GLIBC_2.2.5`, including glibc's unwinding and CET handling.
//! The named LIBC exports are essential: the versioned host reference must
//! not bind back to our dispatcher. As with the rest of this shim, unversioned
//! Bionic imports require LD_PRELOAD ordering, not a late dlopen after libc.
//!
//! Compact environments currently reject active CET shadow stacks, both at
//! save and restore. RDSSP checks the hardware state independently, so even a
//! seccomp-generated EINVAL cannot hide an active shadow stack. Other unknown
//! or denied ARCH_SHSTK_STATUS results fail closed; EINVAL accommodates kernels
//! without this API, but only after RDSSP has established that SHSTK is off.
//! Supporting CET here requires its own shadow-stack state/restore protocol,
//! not forwarding an undersized buffer to glibc (whose SSP can be at byte 88).
//! All compact-path operations are allocation-free raw syscalls, including
//! signal masks: passing our 8-byte mask to a glibc sigset_t API is not safe.
//!
//! build.rs assembles this into the cdylib ONLY, so linking the Rust rlib into
//! a host executable cannot accidentally interpose its nonlocal jumps.

pub const SYMBOLS: &[&str] = &["sigsetjmp", "siglongjmp", "longjmp", "_longjmp"];

pub const ASSEMBLY: &str = r#"
.section .text.pengudroid_jump,"ax",@progbits
.p2align 4
.globl sigsetjmp
.type sigsetjmp,@function
sigsetjmp:
    endbr64
    // Check before writing even the first byte of the caller's buffer.
    pushq %rdi
    pushq %rsi
    subq $8, %rsp
    call .Lpd_jump_check_shstk
    addq $8, %rsp
    popq %rsi
    popq %rdi

    movq %rbx, 0(%rdi)
    movq %rbp, 8(%rdi)
    movq %r12, 16(%rdi)
    movq %r13, 24(%rdi)
    movq %r14, 32(%rdi)
    movq %r15, 40(%rdi)
    leaq 8(%rsp), %rax
    movq %rax, 48(%rdi)
    movq (%rsp), %rax
    movq %rax, 56(%rdi)
    // glibc's per-thread pointer guard, not Bionic's libc_globals cookie.
    movq %fs:0x30, %rax
    .irp offset,0,8,16,24,32,40,48,56
        xorq %rax, \offset(%rdi)
    .endr
    movq $0x50444a00, 64(%rdi)
    movq $0, 72(%rdi)
    testl %esi, %esi
    jz .Lpd_jump_saved_mask
    orq $1, 64(%rdi)
    movq %rdi, %r8
    leaq 72(%rdi), %rdx
    xorl %edi, %edi
    xorl %esi, %esi
    movl $8, %r10d
    movl $14, %eax                 // rt_sigprocmask(0, NULL, mask, 8)
    syscall
    testq %rax, %rax
    js .Lpd_jump_mask_failure
    movq %r8, %rdi
.Lpd_jump_saved_mask:
    movq %fs:0x30, %rax
    .irp offset,0,8,16,24,32,40,48,56,64,72
        xorq \offset(%rdi), %rax
    .endr
    movq %rax, 80(%rdi)
    xorl %eax, %eax
    ret
.size sigsetjmp, .-sigsetjmp

// A real versioned reference, not RTLD_NEXT/dlsym in a signal handler.
.symver __pengudroid_host_siglongjmp, siglongjmp@GLIBC_2.2.5
.p2align 4
.globl siglongjmp
.globl longjmp
.globl _longjmp
.type siglongjmp,@function
.type longjmp,@function
.type _longjmp,@function
siglongjmp:
longjmp:
_longjmp:
    endbr64
    // Only inspect glibc's initialized 32-bit flag, never its padding.
    cmpl $1, 64(%rdi)
    jbe .Lpd_jump_host
    movl 64(%rdi), %eax
    andl $-2, %eax
    cmpl $0x50444a00, %eax
    jne .Lpd_jump_bad_context
    movq %fs:0x30, %rax
    .irp offset,0,8,16,24,32,40,48,56,64,72,80
        xorq \offset(%rdi), %rax
    .endr
    testq %rax, %rax
    jnz .Lpd_jump_bad_context

    pushq %rdi
    pushq %rsi
    subq $8, %rsp
    call .Lpd_jump_check_shstk
    addq $8, %rsp
    popq %rsi
    popq %rdi
    testb $1, 64(%rdi)
    jz .Lpd_jump_restore_registers
    movq %rdi, %r8
    movl %esi, %r9d
    leaq 72(%rdi), %rsi
    movl $2, %edi
    xorl %edx, %edx
    movl $8, %r10d
    movl $14, %eax                 // rt_sigprocmask(SIG_SETMASK, mask, NULL, 8)
    syscall
    testq %rax, %rax
    js .Lpd_jump_mask_failure
    movq %r8, %rdi
    movl %r9d, %esi
.Lpd_jump_restore_registers:
    movq %fs:0x30, %rcx
    movq 48(%rdi), %r8
    xorq %rcx, %r8
    movq 56(%rdi), %rdx
    xorq %rcx, %rdx
    movq 0(%rdi), %rbx
    xorq %rcx, %rbx
    movq 8(%rdi), %rbp
    xorq %rcx, %rbp
    movq 16(%rdi), %r12
    xorq %rcx, %r12
    movq 24(%rdi), %r13
    xorq %rcx, %r13
    movq 32(%rdi), %r14
    xorq %rcx, %r14
    movq 40(%rdi), %r15
    xorq %rcx, %r15
    movl %esi, %eax
    testl %eax, %eax
    jnz .Lpd_jump_nonzero
    movl $1, %eax
.Lpd_jump_nonzero:
    movq %r8, %rsp
    notrack jmp *%rdx
.Lpd_jump_host:
    jmp __pengudroid_host_siglongjmp@PLT
.Lpd_jump_bad_context:
    leaq .Lpd_jump_context_message(%rip), %rsi
    movl $.Lpd_jump_context_end-.Lpd_jump_context_message, %edx
    jmp .Lpd_jump_fatal
.size siglongjmp, .-siglongjmp
.size longjmp, .-longjmp
.size _longjmp, .-_longjmp

// No libc/TLS initialization or cached feature state: a later CET enable must
// also be rejected at restore. syscall only clobbers rax, rcx, and r11.
.Lpd_jump_check_shstk:
    // RDSSP is a reserved NOP on pre-CET CPUs and leaves its destination
    // unchanged when SHSTK is disabled. Initialize it before testing.
    xorl %eax, %eax
    rdsspq %rax
    testq %rax, %rax
    jnz .Lpd_jump_shstk_failure
    subq $8, %rsp
    movl $158, %eax                // arch_prctl(ARCH_SHSTK_STATUS, &status)
    movl $0x5005, %edi
    movq %rsp, %rsi
    syscall
    cmpq $-22, %rax                // EINVAL is safe only after the RDSSP check
    je .Lpd_jump_no_shstk
    testq %rax, %rax
    jne .Lpd_jump_shstk_failure
    testq $1, (%rsp)               // ARCH_SHSTK_SHSTK
    jnz .Lpd_jump_shstk_failure
.Lpd_jump_no_shstk:
    addq $8, %rsp
    ret
.Lpd_jump_shstk_failure:
    leaq .Lpd_jump_shstk_message(%rip), %rsi
    movl $.Lpd_jump_shstk_end-.Lpd_jump_shstk_message, %edx
    jmp .Lpd_jump_fatal
.Lpd_jump_mask_failure:
    leaq .Lpd_jump_mask_message(%rip), %rsi
    movl $.Lpd_jump_mask_end-.Lpd_jump_mask_message, %edx
.Lpd_jump_fatal:
    movl $2, %edi
    movl $1, %eax                  // write(stderr, message, length)
    syscall
    movl $127, %edi
    movl $231, %eax                // exit_group, even from a signal handler
    syscall
    ud2

.section .rodata.pengudroid_jump,"a",@progbits
.Lpd_jump_context_message:
    .ascii "pengudroid-shim: invalid compact jump context\n"
.Lpd_jump_context_end:
.Lpd_jump_shstk_message:
    .ascii "pengudroid-shim: compact Bionic jumps require CET shadow stacks off and a readable ARCH_SHSTK_STATUS\n"
.Lpd_jump_shstk_end:
.Lpd_jump_mask_message:
    .ascii "pengudroid-shim: compact jump rt_sigprocmask failed\n"
.Lpd_jump_mask_end:
.section .note.GNU-stack,"",@progbits
"#;
