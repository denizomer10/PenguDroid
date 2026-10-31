//! Host-only adaptation of Digitalis's Bionic function-wrapper code pool.
//!
//! `runtime_primitives/code_pool.{h,cc}` already selects the anonymous factory
//! for !__BIONIC__. The ELF factory cannot run through a dlopen shim which
//! ignores ANDROID_DLEXT_RESERVED_ADDRESS: its RX alias must land in low 2 GiB.
//! Delegate allocation to the loaded translator, preserving its dual memfd
//! aliases, fd ownership, low-address policy, and fatal Create error handling.
//!
//! The ELF factory ignores small requests and always returns 512 KiB (see its
//! header and exec_region/sections.ld). Check that limit BEFORE delegating and
//! request the entire capacity, not the anonymous factory's 4 MiB default or
//! the possibly much smaller caller request. CodePool's compiled-in capacity
//! and Detach/reset behavior are otherwise unchanged.
//!
//! ABI: the pinned base/exec_region.h has a nontrivial move constructor. Under
//! x86_64 SysV/Itanium C++ ABI Create(size_t) takes a hidden output pointer in
//! RDI, size in RSI, and returns that pointer in RAX. The output is opaque here;
//! returning a Rust C-layout aggregate would NOT express this C++ contract.
//! fixtures/bridge_exec.cpp checks this with real GCC/Clang C++ constructors.
//!
//! This does not emulate ELF loader identity, segment layout, dladdr/dlsym, or
//! unwind metadata. Anonymous bases are host-page-aligned, not necessarily the
//! ELF script's 16 KiB alignment; CodePool only requires 64-byte code alignment.
//! The inspected ExecRegion/CodePool paths do not consume those ELF properties;
//! this is not a general replacement for reserved-address
//! dlopen. The translator must be RTLD_GLOBAL after this preloaded shim and
//! keep these factory calls preemptible. Missing delegation fails closed.

use core::ffi::{c_char, c_int, c_void};

const ELF_REGION_CAPACITY: usize = 512 * 1024;
const ANONYMOUS_CREATE: &[u8] = b"_ZN8berberis26ExecRegionAnonymousFactory6CreateEm\0";
const RTLD_NEXT: *mut c_void = -1isize as *mut c_void;

type AnonymousCreate = unsafe extern "C" fn(*mut c_void, usize) -> *mut c_void;

#[link(name = "dl")]
extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

extern "C" {
    fn write(fd: c_int, data: *const c_void, size: usize) -> isize;
    fn _exit(status: c_int) -> !;
}

unsafe fn fail(message: &[u8]) -> ! {
    let _ = write(2, message.as_ptr().cast(), message.len());
    _exit(127)
}

/// Lowered hidden-sret ABI, NOT a C aggregate return. Only enabled by lib.rs
/// for Linux GNU x86_64 LP64, the compiler/ABI combination verified by probes.
#[export_name = "_ZN8berberis26ExecRegionElfBackedFactory6CreateEm"]
pub unsafe extern "C" fn create(output: *mut c_void, requested_size: usize) -> *mut c_void {
    if output.is_null() {
        fail(b"pengudroid-shim: bridge ExecRegion Create has a null hidden result pointer\n");
    }
    if requested_size > ELF_REGION_CAPACITY {
        fail(b"pengudroid-shim: bridge ELF ExecRegion request exceeds its fixed 512 KiB capacity\n");
    }
    // Resolve at the call, not at shim initialization: the translator is loaded
    // later. Do not fall back to the ELF implementation or an empty region.
    let address = dlsym(RTLD_NEXT, ANONYMOUS_CREATE.as_ptr().cast());
    if address.is_null() {
        fail(b"pengudroid-shim: missing RTLD_NEXT ExecRegionAnonymousFactory::Create; translator must be RTLD_GLOBAL after the shim\n");
    }
    let anonymous_create: AnonymousCreate = core::mem::transmute(address);
    let returned_output = anonymous_create(output, ELF_REGION_CAPACITY);
    if returned_output != output {
        fail(b"pengudroid-shim: bridge ExecRegion anonymous delegate violated the x86_64 hidden-sret ABI\n");
    }
    output
}
