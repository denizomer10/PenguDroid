//! Optional hardening for guest processes: `--isolate`.
//!
//! PenguDroid deliberately does not run a container, so a guest is an ordinary
//! process with the user's privileges. That is fine for apps you trust, but it
//! is not Android's process isolation. `--isolate` narrows the gap without
//! reintroducing a container:
//!
//! * `PR_SET_NO_NEW_PRIVS`, so setuid binaries cannot elevate;
//! * a seccomp filter that refuses the syscalls an app never needs and that are
//!   the usual escape hatches: `ptrace`, `process_vm_*`, `mount`/`umount2`,
//!   `pivot_root`, `chroot`, `unshare`/`setns`, module loading, `bpf`,
//!   `perf_event_open`, `keyctl`, `reboot`, `kexec_*`, `swapon`, `acct`,
//!   `open_by_handle_at`, `syslog`, `iopl`/`ioperm`, clock/hostname changes.
//!
//! This is a hardening filter, not a sandbox: it does not restrict the
//! filesystem or the network. A production design would add a private mount
//! view; see `docs/LIMITATIONS.md` §8.

use std::ffi::c_void;

/// Syscalls refused under `--isolate`, with the reason shown in diagnostics.
const DENIED: &[(&str, libc::c_long)] = &[
    ("ptrace", libc::SYS_ptrace),
    ("process_vm_readv", libc::SYS_process_vm_readv),
    ("process_vm_writev", libc::SYS_process_vm_writev),
    ("mount", libc::SYS_mount),
    ("umount2", libc::SYS_umount2),
    ("pivot_root", libc::SYS_pivot_root),
    ("chroot", libc::SYS_chroot),
    ("unshare", libc::SYS_unshare),
    ("setns", libc::SYS_setns),
    ("init_module", libc::SYS_init_module),
    ("finit_module", libc::SYS_finit_module),
    ("delete_module", libc::SYS_delete_module),
    ("kexec_load", libc::SYS_kexec_load),
    ("reboot", libc::SYS_reboot),
    ("swapon", libc::SYS_swapon),
    ("swapoff", libc::SYS_swapoff),
    ("acct", libc::SYS_acct),
    ("open_by_handle_at", libc::SYS_open_by_handle_at),
    ("name_to_handle_at", libc::SYS_name_to_handle_at),
    ("syslog", libc::SYS_syslog),
    ("iopl", libc::SYS_iopl),
    ("ioperm", libc::SYS_ioperm),
    ("settimeofday", libc::SYS_settimeofday),
    ("clock_settime", libc::SYS_clock_settime),
    ("adjtimex", libc::SYS_adjtimex),
    ("sethostname", libc::SYS_sethostname),
    ("setdomainname", libc::SYS_setdomainname),
    ("bpf", libc::SYS_bpf),
    ("perf_event_open", libc::SYS_perf_event_open),
    ("userfaultfd", libc::SYS_userfaultfd),
    ("keyctl", libc::SYS_keyctl),
    ("add_key", libc::SYS_add_key),
    ("request_key", libc::SYS_request_key),
];

// Classic BPF encoding (linux/bpf_common.h, linux/filter.h).
const BPF_LD_W_ABS: u16 = 0x20;
const BPF_JMP_JEQ_K: u16 = 0x15;
const BPF_RET_K: u16 = 0x06;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
const SECCOMP_MODE_FILTER: libc::c_ulong = 2;
const PR_SET_NO_NEW_PRIVS: libc::c_int = 38;
const PR_SET_SECCOMP: libc::c_int = 22;

#[repr(C)]
struct SockFilter {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

#[repr(C)]
struct SockFprog {
    len: u16,
    filter: *const SockFilter,
}

/// Build the BPF program: `if nr == denied { return EPERM } ... return ALLOW`.
fn program() -> Vec<SockFilter> {
    let mut prog = Vec::with_capacity(DENIED.len() * 2 + 2);
    // Load seccomp_data.nr (offset 0).
    prog.push(SockFilter { code: BPF_LD_W_ABS, jt: 0, jf: 0, k: 0 });
    for (_, nr) in DENIED {
        // If equal, fall through to the ERRNO return; otherwise skip it.
        prog.push(SockFilter {
            code: BPF_JMP_JEQ_K,
            jt: 0,
            jf: 1,
            k: *nr as u32,
        });
        prog.push(SockFilter {
            code: BPF_RET_K,
            jt: 0,
            jf: 0,
            k: SECCOMP_RET_ERRNO | libc::EPERM as u32,
        });
    }
    prog.push(SockFilter { code: BPF_RET_K, jt: 0, jf: 0, k: SECCOMP_RET_ALLOW });
    prog
}

/// Apply the hardening filter to the current process.
///
/// Returns `Ok(())` when the filter is active. A kernel or sandbox that refuses
/// seccomp is reported as an error so the caller can decide whether to continue.
pub fn apply() -> Result<(), String> {
    // SAFETY: `prctl` with a plain integer argument.
    let rc = unsafe { libc::prctl(PR_SET_NO_NEW_PRIVS, 1 as libc::c_ulong, 0, 0, 0) };
    if rc != 0 {
        return Err(format!(
            "PR_SET_NO_NEW_PRIVS failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    let prog = program();
    let fprog = SockFprog { len: prog.len() as u16, filter: prog.as_ptr() };
    // SAFETY: `fprog` points at `prog`, which outlives the call.
    let rc = unsafe {
        libc::prctl(
            PR_SET_SECCOMP,
            SECCOMP_MODE_FILTER,
            &fprog as *const SockFprog as *const c_void as libc::c_ulong,
            0,
            0,
        )
    };
    if rc != 0 {
        return Err(format!("PR_SET_SECCOMP failed: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

/// The syscalls `--isolate` refuses (for diagnostics and tests).
pub fn denied_syscalls() -> &'static [(&'static str, libc::c_long)] {
    DENIED
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_shape_is_well_formed() {
        let prog = program();
        assert_eq!(prog.len(), DENIED.len() * 2 + 2);
        assert_eq!(prog[0].code, BPF_LD_W_ABS);
        assert_eq!(prog.last().unwrap().k, SECCOMP_RET_ALLOW);
    }

    /// Apply the filter in a forked child and check that a denied syscall fails
    /// with EPERM while an allowed one still works.
    #[test]
    fn filter_denies_ptrace_and_allows_getpid() {
        // SAFETY: fork() in a test; the child only makes syscalls and exits.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            // Child.
            let code = if apply().is_err() {
                // The sandbox may forbid seccomp; skip rather than fail.
                42
            } else {
                // SAFETY: PTRACE_TRACEME is a plain ptrace request.
                let rc = unsafe { libc::ptrace(libc::PTRACE_TRACEME, 0, 0, 0) };
                let denied = rc == -1
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
                // SAFETY: getpid is always allowed.
                let alive = unsafe { libc::getpid() } > 0;
                if denied && alive {
                    0
                } else {
                    1
                }
            };
            // SAFETY: `_exit` is async-signal-safe and does not run destructors.
            unsafe { libc::_exit(code) };
        }
        let mut status = 0;
        // SAFETY: waiting for the child we just forked.
        unsafe { libc::waitpid(pid, &mut status, 0) };
        let code = (status >> 8) & 0xff;
        assert!(
            code == 0 || code == 42,
            "child exited {code} (expected 0, or 42 when seccomp is unavailable)"
        );
    }
}
