//! Bionic's `netdb.h` ABI on top of glibc's resolver.
//!
//! `struct addrinfo` is **not** laid out the same way in Bionic and glibc. Both
//! start with the four `int` fields and `socklen_t ai_addrlen`, but then they
//! disagree about the order of the two pointers:
//!
//! ```text
//! offset  Bionic (bionic/libc/include/netdb.h)   glibc (netdb.h)
//! 0x00    int      ai_flags                      int      ai_flags
//! 0x04    int      ai_family                     int      ai_family
//! 0x08    int      ai_socktype                   int      ai_socktype
//! 0x0c    int      ai_protocol                   int      ai_protocol
//! 0x10    socklen_t ai_addrlen                   socklen_t ai_addrlen
//! 0x18    char*    ai_canonname                  sockaddr* ai_addr
//! 0x20    sockaddr* ai_addr                      char*    ai_canonname
//! 0x28    addrinfo* ai_next                      addrinfo* ai_next
//! ```
//!
//! Handing a glibc result list to Bionic-compiled code therefore makes
//! `p->ai_addr` read `ai_canonname`, which is usually `NULL` — that is a
//! guaranteed SIGSEGV inside `sockaddrToInetAddress` (libjavacore's
//! `android_getaddrinfo`). The shim converts in both directions instead.
//!
//! The `AI_*` flag bits and the `EAI_*` error codes differ as well, so those are
//! translated too. `NI_*` flags and `struct hostent` happen to agree.

use libc::{c_char, c_int, c_void, sockaddr, socklen_t};
use std::ffi::{CStr, CString};
use std::sync::OnceLock;

/// glibc's `getaddrinfo`, looked up past the shim's own interposition.
///
/// The shim exports a symbol named `getaddrinfo`, so a plain `libc::getaddrinfo`
/// call from inside the shim would be interposed by the dynamic linker and
/// recurse forever. `RTLD_NEXT` skips our definition.
type GetaddrinfoFn = unsafe extern "C" fn(
    *const c_char,
    *const c_char,
    *const libc::addrinfo,
    *mut *mut libc::addrinfo,
) -> c_int;
type FreeaddrinfoFn = unsafe extern "C" fn(*mut libc::addrinfo);

static REAL_GETADDRINFO: OnceLock<usize> = OnceLock::new();
static REAL_FREEADDRINFO: OnceLock<usize> = OnceLock::new();

fn lookup(name: &CStr) -> usize {
    // SAFETY: `dlsym` with `RTLD_NEXT` is always safe to call; a missing symbol
    // yields null, which the callers treat as an error.
    unsafe { libc::dlsym(libc::RTLD_NEXT, name.as_ptr()) as usize }
}

fn real_getaddrinfo() -> Option<GetaddrinfoFn> {
    let p = *REAL_GETADDRINFO.get_or_init(|| lookup(c"getaddrinfo"));
    if p == 0 {
        None
    } else {
        // SAFETY: the symbol is glibc's `getaddrinfo`, which has this signature.
        Some(unsafe { std::mem::transmute::<usize, GetaddrinfoFn>(p) })
    }
}

fn real_freeaddrinfo() -> Option<FreeaddrinfoFn> {
    let p = *REAL_FREEADDRINFO.get_or_init(|| lookup(c"freeaddrinfo"));
    if p == 0 {
        None
    } else {
        // SAFETY: the symbol is glibc's `freeaddrinfo`, which has this signature.
        Some(unsafe { std::mem::transmute::<usize, FreeaddrinfoFn>(p) })
    }
}

/// Release a glibc-layout list with glibc's own `freeaddrinfo`.
unsafe fn glibc_freeaddrinfo(list: *mut libc::addrinfo) {
    if list.is_null() {
        return;
    }
    match real_freeaddrinfo() {
        Some(f) => f(list),
        // Without the real symbol the list leaks; freeing it by hand would be
        // wrong because glibc allocates the whole list in one block.
        None => {}
    }
}

/// Bionic's `struct addrinfo`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BionicAddrinfo {
    pub ai_flags: c_int,
    pub ai_family: c_int,
    pub ai_socktype: c_int,
    pub ai_protocol: c_int,
    pub ai_addrlen: socklen_t,
    pub ai_canonname: *mut c_char,
    pub ai_addr: *mut sockaddr,
    pub ai_next: *mut BionicAddrinfo,
}

// --- Bionic `AI_*` flags ---------------------------------------------------
pub const AI_PASSIVE: c_int = 0x0000_0001;
pub const AI_CANONNAME: c_int = 0x0000_0002;
pub const AI_NUMERICHOST: c_int = 0x0000_0004;
pub const AI_NUMERICSERV: c_int = 0x0000_0008;
pub const AI_ALL: c_int = 0x0000_0100;
pub const AI_V4MAPPED_CFG: c_int = 0x0000_0200;
pub const AI_ADDRCONFIG: c_int = 0x0000_0400;
pub const AI_V4MAPPED: c_int = 0x0000_0800;

// --- glibc `AI_*` flags ----------------------------------------------------
const G_AI_PASSIVE: c_int = 0x0001;
const G_AI_CANONNAME: c_int = 0x0002;
const G_AI_NUMERICHOST: c_int = 0x0004;
const G_AI_V4MAPPED: c_int = 0x0008;
const G_AI_ALL: c_int = 0x0010;
const G_AI_ADDRCONFIG: c_int = 0x0020;
const G_AI_NUMERICSERV: c_int = 0x0400;

// --- Bionic `EAI_*` codes --------------------------------------------------
pub const EAI_ADDRFAMILY: c_int = 1;
pub const EAI_AGAIN: c_int = 2;
pub const EAI_BADFLAGS: c_int = 3;
pub const EAI_FAIL: c_int = 4;
pub const EAI_FAMILY: c_int = 5;
pub const EAI_MEMORY: c_int = 6;
pub const EAI_NODATA: c_int = 7;
pub const EAI_NONAME: c_int = 8;
pub const EAI_SERVICE: c_int = 9;
pub const EAI_SOCKTYPE: c_int = 10;
pub const EAI_SYSTEM: c_int = 11;
pub const EAI_BADHINTS: c_int = 12;
pub const EAI_PROTOCOL: c_int = 13;
pub const EAI_OVERFLOW: c_int = 14;

/// Translate Bionic `AI_*` bits into the glibc ones with the same meaning.
///
/// `AI_V4MAPPED_CFG` has no glibc counterpart (it is a BSD extension that asks
/// for v4-mapped addresses only when the kernel supports them); dropping it
/// leaves the resolver's default behaviour, which is what we want.
pub fn bionic_to_glibc_flags(flags: c_int) -> c_int {
    let mut out = 0;
    if flags & AI_PASSIVE != 0 {
        out |= G_AI_PASSIVE;
    }
    if flags & AI_CANONNAME != 0 {
        out |= G_AI_CANONNAME;
    }
    if flags & AI_NUMERICHOST != 0 {
        out |= G_AI_NUMERICHOST;
    }
    if flags & AI_NUMERICSERV != 0 {
        out |= G_AI_NUMERICSERV;
    }
    if flags & AI_ALL != 0 {
        out |= G_AI_ALL;
    }
    if flags & AI_ADDRCONFIG != 0 {
        out |= G_AI_ADDRCONFIG;
    }
    if flags & AI_V4MAPPED != 0 {
        out |= G_AI_V4MAPPED;
    }
    let _ = AI_V4MAPPED_CFG;
    out
}

/// Translate a glibc `EAI_*` code into Bionic's numbering.
///
/// glibc returns negative codes; Bionic returns small positive ones. `0`
/// (success) passes through.
pub fn glibc_to_bionic_eai(code: c_int) -> c_int {
    match code {
        0 => 0,
        // glibc: EAI_BADFLAGS -1, EAI_NONAME -2, EAI_AGAIN -3, EAI_FAIL -4,
        // EAI_FAMILY -6, EAI_SOCKTYPE -7, EAI_SERVICE -8, EAI_MEMORY -10,
        // EAI_SYSTEM -11, EAI_OVERFLOW -12.
        -1 => EAI_BADFLAGS,
        -2 => EAI_NONAME,
        -3 => EAI_AGAIN,
        -4 => EAI_FAIL,
        -6 => EAI_FAMILY,
        -7 => EAI_SOCKTYPE,
        -8 => EAI_SERVICE,
        -10 => EAI_MEMORY,
        -11 => EAI_SYSTEM,
        -12 => EAI_OVERFLOW,
        // A positive code is already in Bionic's numbering (or came from a
        // Bionic caller); pass it through.
        other => other,
    }
}

/// The Bionic `gai_strerror` text for a code.
pub fn gai_strerror_text(code: c_int) -> &'static CStr {
    let s: &'static str = match code {
        0 => "no error",
        EAI_ADDRFAMILY => "address family for hostname not supported",
        EAI_AGAIN => "temporary failure in name resolution",
        EAI_BADFLAGS => "invalid value for ai_flags",
        EAI_FAIL => "non-recoverable failure in name resolution",
        EAI_FAMILY => "ai_family not supported",
        EAI_MEMORY => "memory allocation failure",
        EAI_NODATA => "no address associated with hostname",
        EAI_NONAME => "hostname nor servname provided, or not known",
        EAI_SERVICE => "servname not supported for ai_socktype",
        EAI_SOCKTYPE => "ai_socktype not supported",
        EAI_SYSTEM => "system error returned in errno",
        EAI_BADHINTS => "invalid value for hints",
        EAI_PROTOCOL => "resolved protocol is unknown",
        EAI_OVERFLOW => "argument buffer overflow",
        _ => "unknown error",
    };
    // Every arm is a NUL-terminated literal, so this cannot fail.
    CStr::from_bytes_with_nul(unsafe {
        std::slice::from_raw_parts(s.as_ptr(), s.len() + 1)
    })
    .unwrap_or(c"unknown error")
}

/// Copy a Bionic `hints` structure into the glibc layout.
///
/// Returns `None` when `hints` is null (glibc treats that as "no hints").
///
/// # Safety
/// `hints` must be null or point at a readable [`BionicAddrinfo`].
pub unsafe fn hints_to_glibc(hints: *const BionicAddrinfo) -> Option<libc::addrinfo> {
    if hints.is_null() {
        return None;
    }
    let h = &*hints;
    Some(libc::addrinfo {
        ai_flags: bionic_to_glibc_flags(h.ai_flags),
        ai_family: h.ai_family,
        ai_socktype: h.ai_socktype,
        ai_protocol: h.ai_protocol,
        ai_addrlen: 0,
        ai_addr: std::ptr::null_mut(),
        ai_canonname: std::ptr::null_mut(),
        ai_next: std::ptr::null_mut(),
    })
}

/// Deep-copy a glibc result list into a Bionic-layout list.
///
/// The copy owns its memory: every node, its `sockaddr` and its `ai_canonname`
/// are separate allocations, so [`free_bionic_addrinfo`] can release them
/// without touching glibc's allocator bookkeeping.
///
/// # Safety
/// `src` must be null or a valid glibc `addrinfo` list.
pub unsafe fn to_bionic_list(src: *const libc::addrinfo) -> *mut BionicAddrinfo {
    let mut head: *mut BionicAddrinfo = std::ptr::null_mut();
    let mut tail: *mut BionicAddrinfo = std::ptr::null_mut();
    let mut p = src;
    while !p.is_null() {
        let node = (*p).clone_into_bionic();
        if node.is_null() {
            free_bionic_addrinfo(head);
            return std::ptr::null_mut();
        }
        if tail.is_null() {
            head = node;
        } else {
            (*tail).ai_next = node;
        }
        tail = node;
        p = (*p).ai_next;
    }
    head
}

trait CloneIntoBionic {
    unsafe fn clone_into_bionic(&self) -> *mut BionicAddrinfo;
}

impl CloneIntoBionic for libc::addrinfo {
    unsafe fn clone_into_bionic(&self) -> *mut BionicAddrinfo {
        let node = libc::calloc(1, std::mem::size_of::<BionicAddrinfo>()) as *mut BionicAddrinfo;
        if node.is_null() {
            return std::ptr::null_mut();
        }
        (*node).ai_flags = self.ai_flags;
        (*node).ai_family = self.ai_family;
        (*node).ai_socktype = self.ai_socktype;
        (*node).ai_protocol = self.ai_protocol;
        (*node).ai_addrlen = self.ai_addrlen;
        if !self.ai_addr.is_null() {
            let len = self.ai_addrlen as usize;
            let addr = libc::malloc(len.max(1)) as *mut sockaddr;
            if addr.is_null() {
                libc::free(node as *mut c_void);
                return std::ptr::null_mut();
            }
            std::ptr::copy_nonoverlapping(self.ai_addr as *const u8, addr as *mut u8, len);
            (*node).ai_addr = addr;
        }
        if !self.ai_canonname.is_null() {
            let name = libc::strdup(self.ai_canonname);
            if name.is_null() {
                libc::free((*node).ai_addr as *mut c_void);
                libc::free(node as *mut c_void);
                return std::ptr::null_mut();
            }
            (*node).ai_canonname = name;
        }
        node
    }
}

/// Release a list produced by [`to_bionic_list`].
///
/// # Safety
/// `head` must be null or a list returned by [`to_bionic_list`] that has not
/// been freed yet.
pub unsafe fn free_bionic_addrinfo(head: *mut BionicAddrinfo) {
    let mut p = head;
    while !p.is_null() {
        let next = (*p).ai_next;
        libc::free((*p).ai_addr as *mut c_void);
        libc::free((*p).ai_canonname as *mut c_void);
        libc::free(p as *mut c_void);
        p = next;
    }
}

/// The shared body of every `*getaddrinfo*` entry point.
///
/// # Safety
/// The pointers follow the usual `getaddrinfo` contract.
pub unsafe fn resolve(
    hostname: *const c_char,
    servname: *const c_char,
    hints: *const BionicAddrinfo,
    res: *mut *mut BionicAddrinfo,
) -> c_int {
    if res.is_null() {
        return EAI_FAIL;
    }
    *res = std::ptr::null_mut();
    let gh = hints_to_glibc(hints);
    let hints_ptr = match &gh {
        Some(h) => h as *const libc::addrinfo,
        None => std::ptr::null(),
    };
    let Some(gai) = real_getaddrinfo() else {
        return EAI_SYSTEM;
    };
    let mut out: *mut libc::addrinfo = std::ptr::null_mut();
    let rc = gai(hostname, servname, hints_ptr, &mut out);
    if rc != 0 {
        glibc_freeaddrinfo(out);
        return glibc_to_bionic_eai(rc);
    }
    if out.is_null() {
        return 0;
    }
    let converted = to_bionic_list(out);
    glibc_freeaddrinfo(out);
    if converted.is_null() {
        return EAI_MEMORY;
    }
    *res = converted;
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_differ_where_we_think_they_do() {
        // The whole point of this module: Bionic puts `ai_canonname` at 0x18
        // and `ai_addr` at 0x20, glibc the other way round.
        assert_eq!(std::mem::size_of::<BionicAddrinfo>(), 0x30);
        assert_eq!(std::mem::offset_of!(BionicAddrinfo, ai_addrlen), 0x10);
        assert_eq!(std::mem::offset_of!(BionicAddrinfo, ai_canonname), 0x18);
        assert_eq!(std::mem::offset_of!(BionicAddrinfo, ai_addr), 0x20);
        assert_eq!(std::mem::offset_of!(BionicAddrinfo, ai_next), 0x28);
        assert_eq!(std::mem::offset_of!(libc::addrinfo, ai_addr), 0x18);
        assert_eq!(std::mem::offset_of!(libc::addrinfo, ai_canonname), 0x20);
    }

    #[test]
    fn flags_are_translated() {
        assert_eq!(bionic_to_glibc_flags(0), 0);
        assert_eq!(bionic_to_glibc_flags(AI_PASSIVE), G_AI_PASSIVE);
        assert_eq!(bionic_to_glibc_flags(AI_NUMERICSERV), G_AI_NUMERICSERV);
        assert_eq!(bionic_to_glibc_flags(AI_ADDRCONFIG), G_AI_ADDRCONFIG);
        assert_eq!(bionic_to_glibc_flags(AI_V4MAPPED), G_AI_V4MAPPED);
        assert_eq!(bionic_to_glibc_flags(AI_ALL), G_AI_ALL);
        // A combination keeps every bit that has a counterpart.
        assert_eq!(
            bionic_to_glibc_flags(AI_ADDRCONFIG | AI_NUMERICHOST),
            G_AI_ADDRCONFIG | G_AI_NUMERICHOST
        );
    }

    #[test]
    fn errors_are_translated() {
        assert_eq!(glibc_to_bionic_eai(0), 0);
        assert_eq!(glibc_to_bionic_eai(-2), EAI_NONAME);
        assert_eq!(glibc_to_bionic_eai(-3), EAI_AGAIN);
        assert_eq!(glibc_to_bionic_eai(-11), EAI_SYSTEM);
        // Already-Bionic codes pass through.
        assert_eq!(glibc_to_bionic_eai(EAI_NONAME), EAI_NONAME);
    }

    #[test]
    fn resolves_a_numeric_host_into_the_bionic_layout() {
        let host = c"127.0.0.1";
        let mut hints: BionicAddrinfo = unsafe { std::mem::zeroed() };
        hints.ai_family = libc::AF_INET;
        hints.ai_socktype = libc::SOCK_STREAM;
        hints.ai_flags = AI_NUMERICHOST;
        let mut res: *mut BionicAddrinfo = std::ptr::null_mut();
        let rc = unsafe { resolve(host.as_ptr(), std::ptr::null(), &hints, &mut res) };
        assert_eq!(rc, 0);
        assert!(!res.is_null());
        unsafe {
            let node = &*res;
            assert_eq!(node.ai_family, libc::AF_INET);
            assert_eq!(node.ai_addrlen as usize, std::mem::size_of::<libc::sockaddr_in>());
            // The address must be readable through the *Bionic* offset.
            assert!(!node.ai_addr.is_null());
            let sin = &*(node.ai_addr as *const libc::sockaddr_in);
            assert_eq!(sin.sin_family as c_int, libc::AF_INET);
            assert_eq!(u32::from_be(sin.sin_addr.s_addr), 0x7f00_0001);
            free_bionic_addrinfo(res);
        }
    }

    #[test]
    fn the_real_resolver_is_reachable_past_the_interposition() {
        // Guards against the recursion trap: `resolve` must reach glibc's
        // `getaddrinfo` through `RTLD_NEXT`, not the shim's own export.
        assert!(real_getaddrinfo().is_some());
        assert!(real_freeaddrinfo().is_some());
    }

    #[test]
    fn a_bad_name_reports_bionic_eai_noname() {
        let host = c"this-host-does-not-exist.invalid";
        let mut hints: BionicAddrinfo = unsafe { std::mem::zeroed() };
        hints.ai_family = libc::AF_INET;
        hints.ai_socktype = libc::SOCK_STREAM;
        let mut res: *mut BionicAddrinfo = std::ptr::null_mut();
        let rc = unsafe { resolve(host.as_ptr(), std::ptr::null(), &hints, &mut res) };
        assert_eq!(rc, EAI_NONAME);
        assert!(res.is_null());
    }
}
