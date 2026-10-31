//! Opt-in, guest-facing Bionic bootstrap TLS for Digitalis on GNU x86_64.
//!
//! This is NOT a pthread_self interposer or a replacement host TCB. Only the
//! hash-validated staged Digitalis adapter calls this versioned helper. The
//! pinned Bionic layout is proven by build-nativebridge.py's x86_64/AArch64
//! compiler probes (Bionic 06356e41c5ed7b12220b24c05ba4fdb873b126a7).
//!
//! As in Bionic's __allocate_temp_bionic_tls, anonymous zero-initialized TLS
//! includes real libgen buffer storage. Thread identity and stack attributes
//! come from native glibc APIs, not from interpreting glibc pthread bytes.
//! The original host pthread handle remains separate and is never rewritten.
//!
//! Allocations intentionally survive until process exit: Rust TLS destructors
//! can precede the translator's pthread-key guest destructors. This costs one
//! approximately 16 KiB mapping per participating host thread. No unmap or
//! pthread registry/destructor emulation is claimed. Guest pthread handles
//! MUST NOT be passed directly to glibc join/detach/etc.; proxy adaptation is
//! separate work. Bootstrap bionic_tcb remains null until ConfigStaticTls binds
//! the actual guest TCB. MTE/guest pthread enumeration are not implemented here.

use core::ffi::{c_int, c_void};
use core::mem::{size_of, MaybeUninit};
use core::ptr;
use std::cell::Cell;

pub const LAYOUT_COOKIE: u64 = 0xf953e6ea42974914;
const PTHREAD_SIZE: usize = 832;
const PTHREAD_TID: usize = 16;
const PTHREAD_CACHED_PID: usize = 20;
const PTHREAD_ATTR: usize = 24;
const PTHREAD_JOIN_STATE: usize = 80;
const PTHREAD_STACK_TOP: usize = 144;
const PTHREAD_STACK_BOTTOM: usize = 152;
const PTHREAD_BIONIC_TLS: usize = 768;
const PTHREAD_BIONIC_TCB: usize = 784;
const BIONIC_TLS_SIZE: usize = 4016;
const TLS_LIBGEN_BUFFERS: usize = 2088;
const LIBGEN_SIZE: usize = 8192;
const GUEST_TCB_NEGATIVE_BYTES: usize = 24;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BridgeTlsView {
    pub guest_pthread: *mut c_void,
    pub bionic_tls: *mut c_void,
    pub host_pthread: usize,
    pub tid: i32,
    pub pid: i32,
    pub guest_state: *mut c_void,
}
const _: () = assert!(size_of::<BridgeTlsView>() == 40);

#[repr(C, align(8))]
struct Bytes<const N: usize>([u8; N]);

#[repr(C)]
struct State {
    host_pthread: usize,
    guest_tp: *mut c_void,
    guest_state: *mut c_void,
    pthread: Bytes<PTHREAD_SIZE>,
    tls: Bytes<BIONIC_TLS_SIZE>,
    libgen: Bytes<LIBGEN_SIZE>,
}

thread_local! {
    // No destructor: see the lifetime contract above. The busy sentinel makes
    // accidental signal re-entry fail closed rather than publish partial TLS.
    static STATE: Cell<*mut State> = const { Cell::new(ptr::null_mut()) };
}
const BUSY: *mut State = 1usize as *mut State;

unsafe fn store<T: Copy>(base: *mut u8, offset: usize, value: T) {
    ptr::write(base.add(offset).cast::<T>(), value);
}

// libc's Rust bindings omit this POSIX getter on this GNU target. Keep its
// native declaration here; do not substitute a setter or inspect attr bytes.
extern "C" {
    fn pthread_attr_getdetachstate(attr: *const libc::pthread_attr_t, state: *mut c_int) -> c_int;
}

struct HostAttributes(libc::pthread_attr_t);
impl Drop for HostAttributes {
    fn drop(&mut self) {
        unsafe { libc::pthread_attr_destroy(&mut self.0) };
    }
}

unsafe fn initialize() -> Result<*mut State, c_int> {
    let host = libc::pthread_self();
    let mut attr = MaybeUninit::<libc::pthread_attr_t>::uninit();
    let status = libc::pthread_getattr_np(host, attr.as_mut_ptr());
    if status != 0 {
        return Err(status);
    }
    let attr = HostAttributes(attr.assume_init());
    let (mut stack, mut size, mut guard) = (ptr::null_mut(), 0usize, 0usize);
    let (mut detached, mut inherit, mut policy) = (0, 0, 0);
    let mut sched = MaybeUninit::<libc::sched_param>::zeroed();
    for status in [
        libc::pthread_attr_getstack(&attr.0, &mut stack, &mut size),
        libc::pthread_attr_getguardsize(&attr.0, &mut guard),
        pthread_attr_getdetachstate(&attr.0, &mut detached),
        libc::pthread_attr_getinheritsched(&attr.0, &mut inherit),
        libc::pthread_attr_getschedpolicy(&attr.0, &mut policy),
        libc::pthread_attr_getschedparam(&attr.0, sched.as_mut_ptr()),
    ] {
        if status != 0 {
            return Err(status);
        }
    }
    let top = (stack as usize).checked_add(size).ok_or(libc::EOVERFLOW)?;
    let allocation = libc::mmap(
        ptr::null_mut(), size_of::<State>(), libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0,
    );
    if allocation == libc::MAP_FAILED {
        return Err(libc::ENOMEM);
    }
    let state = allocation.cast::<State>();
    (*state).host_pthread = host as usize;
    let pthread = ptr::addr_of_mut!((*state).pthread.0).cast::<u8>();
    let tls = ptr::addr_of_mut!((*state).tls.0).cast::<u8>();
    let libgen = ptr::addr_of_mut!((*state).libgen.0).cast::<u8>();
    store(pthread, PTHREAD_BIONIC_TLS, tls);
    store(tls, TLS_LIBGEN_BUFFERS, libgen);
    // Bionic attr flag values are from pthread_internal.h, not glibc enums.
    let flags: u32 = (if detached == libc::PTHREAD_CREATE_DETACHED { 1 } else { 0 })
        | (if inherit == libc::PTHREAD_INHERIT_SCHED { 4 } else { 8 });
    store(pthread, PTHREAD_ATTR, flags);
    store(pthread, PTHREAD_ATTR + 8, stack);
    store(pthread, PTHREAD_ATTR + 16, size);
    store(pthread, PTHREAD_ATTR + 24, guard);
    store(pthread, PTHREAD_ATTR + 32, policy);
    store(pthread, PTHREAD_ATTR + 36, sched.assume_init().sched_priority);
    store(pthread, PTHREAD_JOIN_STATE, if detached == libc::PTHREAD_CREATE_DETACHED { 3i32 } else { 0 });
    store(pthread, PTHREAD_STACK_TOP, top);
    store(pthread, PTHREAD_STACK_BOTTOM, stack as usize);
    Ok(state)
}

unsafe fn get_state() -> Result<*mut State, c_int> {
    STATE.with(|slot| {
        let state = slot.get();
        if state == BUSY {
            return Err(libc::EBUSY);
        }
        if !state.is_null() {
            return Ok(state);
        }
        slot.set(BUSY);
        let initialized = initialize();
        slot.set(initialized.as_ref().copied().unwrap_or(ptr::null_mut()));
        initialized
    })
}

unsafe fn enabled() -> bool {
    let value = libc::getenv(b"PENGUDROID_BRIDGE_TLS\0".as_ptr().cast());
    !value.is_null() && *value == b'1' as libc::c_char && *value.add(1) == 0
}

/// Versioned, opt-in boundary used only by the staged Digitalis adapter.
///
/// `out` must be writable for `view_size` bytes. `guest_tp`, if non-null, is
/// the TP within the adapter-validated ARM64 static-TLS allocation; it is never
/// a host TP. Null TP is a query and does not unbind an existing guest TCB.
/// Return a POSIX error number, not a fabricated pointer, on any mismatch.
#[no_mangle]
pub unsafe extern "C" fn pengudroid_bridge_tls_v1(
    layout_cookie: u64,
    view_size: usize,
    guest_tp: *mut c_void,
    guest_state: *mut c_void,
    out: *mut BridgeTlsView,
) -> c_int {
    if layout_cookie != LAYOUT_COOKIE || view_size != size_of::<BridgeTlsView>() || out.is_null() {
        return libc::EINVAL;
    }
    if !enabled() {
        return libc::ENOTSUP;
    }
    if (!guest_tp.is_null() && ((guest_tp as usize) < GUEST_TCB_NEGATIVE_BYTES
        || (guest_tp as usize) & 7 != 0)) || (guest_tp.is_null() && !guest_state.is_null()) {
        return libc::EINVAL;
    }
    let state = match get_state() {
        Ok(state) => state,
        Err(error) => return error,
    };
    let host = libc::pthread_self() as usize;
    if host != (*state).host_pthread || guest_tp as usize == host {
        return libc::EINVAL;
    }
    // Raw identity queries neither reinterpret nor modify glibc's TCB. Refresh
    // after fork as well; do NOT call set_tid_address on synthetic storage.
    let tid = libc::syscall(libc::SYS_gettid) as i32;
    let pid = libc::syscall(libc::SYS_getpid) as i32;
    if tid <= 0 || pid <= 0 {
        return libc::ESRCH;
    }
    let pthread = ptr::addr_of_mut!((*state).pthread.0).cast::<u8>();
    store(pthread, PTHREAD_TID, tid);
    store(pthread, PTHREAD_CACHED_PID, (pid as u32) & 0x7fff_ffff);
    if !guest_tp.is_null() {
        (*state).guest_tp = guest_tp;
        (*state).guest_state = guest_state;
        store(pthread, PTHREAD_BIONIC_TCB,
              (guest_tp as *mut u8).sub(GUEST_TCB_NEGATIVE_BYTES));
    }
    ptr::write(out, BridgeTlsView {
        guest_pthread: pthread.cast(), bionic_tls: ptr::addr_of_mut!((*state).tls.0).cast(),
        host_pthread: host, tid, pid, guest_state: (*state).guest_state,
    });
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe fn view(tp: *mut c_void, storage: *mut c_void) -> BridgeTlsView {
        let mut out = MaybeUninit::<BridgeTlsView>::uninit();
        assert_eq!(pengudroid_bridge_tls_v1(LAYOUT_COOKIE, 40, tp, storage, out.as_mut_ptr()), 0);
        out.assume_init()
    }

    #[test]
    fn opt_in_identity_layout_and_thread_isolation() {
        // Run this focused module with --test-threads=1: the opt-in is process-wide.
        unsafe {
            std::env::remove_var("PENGUDROID_BRIDGE_TLS");
            let mut out = MaybeUninit::<BridgeTlsView>::uninit();
            assert_eq!(pengudroid_bridge_tls_v1(LAYOUT_COOKIE, 40, ptr::null_mut(), ptr::null_mut(), out.as_mut_ptr()), libc::ENOTSUP);
            std::env::set_var("PENGUDROID_BRIDGE_TLS", "1");
            assert_eq!(pengudroid_bridge_tls_v1(0, 40, ptr::null_mut(), ptr::null_mut(), out.as_mut_ptr()), libc::EINVAL);
            let host = libc::pthread_self() as usize;
            let first = view(ptr::null_mut(), ptr::null_mut());
            assert_eq!(first.host_pthread, host);
            assert_ne!(first.guest_pthread as usize, host);
            assert_eq!(first.tid, libc::syscall(libc::SYS_gettid) as i32);
            assert_eq!(first.pid, libc::getpid());
            let pthread = first.guest_pthread.cast::<u8>();
            assert_eq!(ptr::read(pthread.add(PTHREAD_BIONIC_TLS).cast::<*mut c_void>()), first.bionic_tls);
            assert!(!ptr::read(first.bionic_tls.cast::<u8>().add(TLS_LIBGEN_BUFFERS).cast::<*mut c_void>()).is_null());
            assert_eq!(*first.bionic_tls.cast::<u8>().add(0xfa9), 0);
            let mut tcb = [0usize; 11];
            let tp = tcb.as_mut_ptr().add(3).cast();
            let storage = 0x1234usize as *mut c_void;
            let bound = view(tp, storage);
            assert_eq!(bound.guest_pthread, first.guest_pthread);
            assert_eq!(bound.guest_state, storage);
            assert_eq!(ptr::read(pthread.add(PTHREAD_BIONIC_TCB).cast::<*mut usize>()), tcb.as_mut_ptr());
            assert!(tcb.iter().all(|&slot| slot == 0), "helper must not write guest or host TCB slots");
            assert_eq!(libc::pthread_self() as usize, host);
            let child = std::thread::spawn(|| {
                let other = view(ptr::null_mut(), ptr::null_mut());
                assert_eq!(other.host_pthread, libc::pthread_self() as usize);
                (other.guest_pthread as usize, other.tid)
            }).join().unwrap();
            assert_ne!(child.0, first.guest_pthread as usize);
            assert_ne!(child.1, first.tid);
            // Retained storage remains readable after host TLS destruction.
            assert_eq!(ptr::read((child.0 + PTHREAD_TID) as *const i32), child.1);
            std::env::remove_var("PENGUDROID_BRIDGE_TLS");
        }
    }
}
