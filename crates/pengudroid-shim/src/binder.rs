//! A minimal `/dev/binder` device emulation.
//!
//! Android's `libbinder` talks to the kernel binder driver through a character
//! device: `open("/dev/binder")`, `ioctl(BINDER_VERSION)`, an `mmap` of the
//! transfer area, then `ioctl(BINDER_WRITE_READ)` for every transaction.
//! PenguDroid does not mount binderfs and does not need the kernel driver: the
//! daemon already runs a userspace broker. This module makes the *device
//! surface* exist so `ProcessState::ProcessState` initialises, which is the
//! first thing `ActivityThread.main` needs.
//!
//! Current scope: the control ioctls succeed and transactions are refused with
//! `EINVAL`, which `libbinder` surfaces as a `RemoteException` instead of the
//! `LOG(FATAL)` that a missing `/dev/binder` produces. Wiring transactions to
//! the daemon's broker is the next step and happens here.
//!
//! The fd is a `memfd` so the `mmap` succeeds and no kernel driver is involved.

use std::ffi::CStr;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicUsize, Ordering};

use libc::{c_int, c_ulong, c_void};

/// Protocol version `libbinder` expects from Android 10 onward.
const BINDER_CURRENT_PROTOCOL_VERSION: c_int = 8;

const BINDER_WRITE_READ: c_ulong = 0xC030_6201;
const BINDER_SET_MAX_THREADS: c_ulong = 0x4004_6205;
const BINDER_SET_CONTEXT_MGR: c_ulong = 0x4004_6207;
const BINDER_VERSION: c_ulong = 0xC004_6209;
const BINDER_THREAD_EXIT: c_ulong = 0x4004_6208;

/// Size of the mapping `libbinder` requests; a `memfd` longer than this is
/// enough (the mapping is never dereferenced by libbinder itself).
const MAP_SIZE: i64 = 1 << 20;

/// The single emulated device fd, or `-1`.
static BINDER_FD: AtomicI32 = AtomicI32::new(-1);

/// Address and length of the transfer area `libbinder` mapped for the device.
/// Replies have to be placed here: the receiver reads its parcels out of this
/// mapping and hands the pointer back in `BC_FREE_BUFFER`.
static MAP_BASE: AtomicUsize = AtomicUsize::new(0);
static MAP_LEN: AtomicUsize = AtomicUsize::new(0);

/// Binder commands that appear on the write side.
const BC_TRANSACTION: u32 = 0x4040_6300;
const BC_REPLY: u32 = 0x4040_6301;
const BC_ACQUIRE: u32 = 0x4004_6302;
const BC_RELEASE: u32 = 0x4004_6303;
const BC_DECREFS: u32 = 0x4004_6304;
const BC_INCREFS: u32 = 0x4004_6306;
const BC_FREE_BUFFER: u32 = 0x4008_6303;
const BC_ENTER_LOOPER: u32 = 0x0000_630c;
const BC_EXIT_LOOPER: u32 = 0x0000_630d;
const BC_REQUEST_DEATH_NOTIFICATION: u32 = 0x4010_630e;
const BC_CLEAR_DEATH_NOTIFICATION: u32 = 0x4010_630f;
const BC_DEAD_BINDER_DONE: u32 = 0x4008_6310;
const BC_INCREFS_DONE: u32 = 0x4010_6308;
const BC_ACQUIRE_DONE: u32 = 0x4010_6309;

/// Binder commands that appear on the read side.
const BR_TRANSACTION_COMPLETE: u32 = 0x0000_7206;
const BR_REPLY: u32 = 0x8040_7203;

/// Transaction flag: no reply is expected.
const TF_ONE_WAY: u32 = 0x1;

/// Offset of the transfer area used for reply parcels.
static REPLY_OFFSET: AtomicUsize = AtomicUsize::new(0);

/// Handles handed out to the guest for brokered services.
static SERVICE_HANDLES: std::sync::LazyLock<std::sync::Mutex<std::collections::BTreeMap<String, u32>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::BTreeMap::new()));
static NEXT_HANDLE: AtomicU32 = AtomicU32::new(1);

/// `android.os.IServiceManager` transaction codes.
const ISM_GET_SERVICE: u32 = 1;
const ISM_CHECK_SERVICE: u32 = 2;

/// `flat_binder_object.type` for a handle that belongs to another process.
///
/// `B_PACK_CHARS('s', 'h', '*', <low byte>)`; the low byte is the value the
/// Android UAPI calls `BINDER_TYPE_HANDLE` (`0x85` in this build). The value was
/// read out of libbinder's own object-type comparisons rather than guessed.
const BINDER_TYPE_HANDLE: u32 = 0x7368_2A85;

/// Stability recorded for a `flat_binder_object` the broker hands out.
///
/// libbinder only recognises the bitmasks 3 (vendor), 0xc (system) and 0x3f;
/// a system-context client rejects anything else.
const BINDER_STABILITY_SYSTEM: u32 = 0xc;

/// `struct binder_transaction_data` on a 64-bit kernel.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct TransactionData {
    target_handle: u32,
    _pad0: u32,
    cookie: u64,
    code: u32,
    flags: u32,
    sender_pid: i32,
    sender_euid: u32,
    data_size: u64,
    offsets_size: u64,
    data_ptr: u64,
    offsets_ptr: u64,
}

/// `struct binder_write_read` on a 64-bit kernel.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct WriteRead {
    write_size: u64,
    write_consumed: u64,
    write_buffer: u64,
    read_size: u64,
    read_consumed: u64,
    read_buffer: u64,
}

/// Write a diagnostic line straight to stderr.
///
/// Logging must not re-enter the JVM (it would call back into binder), so this
/// deliberately uses `write(2)` only.
unsafe fn diag(line: &str) {
    libc::write(2, line.as_ptr() as *const c_void, line.len());
}

fn trace_enabled() -> bool {
    std::env::var_os("PENGUDROID_TRACE_BINDER").is_some()
}

/// Whether `path` names one of Android's binder devices.
fn is_binder_device(path: &[u8]) -> bool {
    matches!(
        path,
        b"/dev/binder" | b"/dev/binderfs/binder" | b"/dev/hwbinder" | b"/dev/vndbinder"
            | b"/dev/binderfs/hwbinder" | b"/dev/binderfs/vndbinder"
    )
}

/// Open the emulated device, returning a fd, or `None` if `path` is not a
/// binder device.
///
/// The fd is shared: `libbinder` opens the device once per process, so a
/// second open returns the same descriptor.
pub fn open_device(path: &[u8]) -> Option<RawFd> {
    if !is_binder_device(path) {
        return None;
    }
    let existing = BINDER_FD.load(Ordering::Relaxed);
    if existing >= 0 {
        return Some(existing);
    }
    let name = b"pengudroid-binder\0";
    // SAFETY: the name is NUL-terminated; the flags are the documented ones.
    let fd = unsafe { libc::memfd_create(name.as_ptr() as *const libc::c_char, libc::MFD_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    // SAFETY: `fd` is a fresh memfd.
    if unsafe { libc::ftruncate(fd, MAP_SIZE) } != 0 {
        unsafe { libc::close(fd) };
        return None;
    }
    BINDER_FD.store(fd, Ordering::Relaxed);
    Some(fd)
}

/// Whether `fd` is the emulated device.
pub fn is_device(fd: RawFd) -> bool {
    fd >= 0 && fd == BINDER_FD.load(Ordering::Relaxed)
}

/// Handle an ioctl on the emulated device.
///
/// Returns `Some(result)` when handled (the value the ioctl returns), or
/// `None` when the request is unknown, in which case the caller forwards it.
///
/// # Safety
/// `arg` must be null or a pointer valid for the request being issued.
pub unsafe fn ioctl(fd: RawFd, request: c_ulong, arg: *mut c_void) -> Option<c_int> {
    if !is_device(fd) {
        return None;
    }
    match request {
        BINDER_VERSION => {
            if arg.is_null() {
                return Some(-1);
            }
            *(arg as *mut c_int) = BINDER_CURRENT_PROTOCOL_VERSION;
            Some(0)
        }
        BINDER_SET_MAX_THREADS | BINDER_SET_CONTEXT_MGR | BINDER_THREAD_EXIT => Some(0),
        BINDER_WRITE_READ => {
            if arg.is_null() {
                return Some(-1);
            }
            Some(handle_write_read(arg as *mut WriteRead))
        }
        _ => Some(0),
    }
}

/// Payload size of a write-side command, or `None` when it is unknown.
fn command_payload(cmd: u32) -> Option<u64> {
    Some(match cmd {
        BC_TRANSACTION | BC_REPLY => 64,
        BC_FREE_BUFFER | BC_DEAD_BINDER_DONE => 8,
        BC_ACQUIRE | BC_RELEASE | BC_INCREFS | BC_DECREFS => 4,
        0x4004_6305 => 8, // BC_ATTEMPT_ACQUIRE (deprecated)
        BC_REQUEST_DEATH_NOTIFICATION | BC_CLEAR_DEATH_NOTIFICATION | BC_INCREFS_DONE
        | BC_ACQUIRE_DONE => 16,
        BC_ENTER_LOOPER | BC_EXIT_LOOPER => 0,
        _ => return None,
    })
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_ne_bytes());
}

fn push_transaction(out: &mut Vec<u8>, tr: &TransactionData) {
    out.extend_from_slice(unsafe {
        std::slice::from_raw_parts(tr as *const TransactionData as *const u8, std::mem::size_of::<TransactionData>())
    });
}

/// A reply prepared for one synchronous transaction.
struct Reply {
    parcel: Vec<u8>,
    objects: Vec<u64>,
}

fn decode_utf16le(bytes: &[u8]) -> String {
    let mut s = String::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        let u = u16::from_le_bytes([bytes[i], bytes[i + 1]]);
        i += 2;
        if u != 0 {
            s.push(char::from_u32(u as u32).unwrap_or('\u{fffd}'));
        }
    }
    s
}

/// Decode the last `String16` in a parcel.
///
/// A `String16` is `[u32 char_count][utf16 chars][nul][pad to 4]`, and the
/// service name is the final argument of `getService`/`checkService`, so
/// scanning backwards for the length whose string ends exactly at the end of
/// the parcel identifies it unambiguously.
fn last_string16(data: &[u8]) -> Option<String> {
    let n = data.len();
    if n < 8 {
        return None;
    }
    let mut pos = (n - 4) & !3;
    loop {
        if pos + 4 <= n {
            let l = u32::from_ne_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
            let end = pos + 4 + l * 2;
            if l > 0 && l <= 512 && end + 2 <= n && ((end + 2 + 3) & !3) == n {
                return Some(decode_utf16le(&data[pos + 4..end]));
            }
        }
        if pos < 4 {
            return None;
        }
        pos -= 4;
    }
}

/// Stable handle for a service name.
fn handle_for(name: &str) -> u32 {
    let mut map = SERVICE_HANDLES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(h) = map.get(name) {
        return *h;
    }
    let h = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    map.insert(name.to_string(), h);
    h
}

fn push_flat_handle(parcel: &mut Vec<u8>, objects: &mut Vec<u64>, handle: u32) {
    objects.push(parcel.len() as u64);
    parcel.extend_from_slice(&BINDER_TYPE_HANDLE.to_ne_bytes());
    parcel.extend_from_slice(&0x7fu32.to_ne_bytes()); // flags
    parcel.extend_from_slice(&handle.to_ne_bytes());
    parcel.extend_from_slice(&0u32.to_ne_bytes()); // alignment
    parcel.extend_from_slice(&0u64.to_ne_bytes()); // cookie
    // Android 12+ writes the binder's stability right after the object and
    // `Parcel::finishUnflattenBinder` reads it back. libbinder accepts only the
    // bitmask values 3 (vendor), 0xc (system) and 0x3f; a `handle` handed to a
    // system-context client must be 0xc, otherwise every transaction fails with
    // "Cannot do a user transaction on a vendor stability binder ... in a
    // system stability context".
    parcel.extend_from_slice(&BINDER_STABILITY_SYSTEM.to_ne_bytes());
}

/// Build the reply parcel for a transaction.
///
/// Every reply starts with the AIDL status word (`0` = no exception); a
/// `getService`/`checkService` call on the service manager additionally gets a
/// `flat_binder_object` describing the handle the broker assigned to the name.
/// Other calls succeed with no result, which is enough for void methods.
unsafe fn make_reply(tr: &TransactionData, pid: i32) -> Reply {
    let mut parcel = Vec::new();
    let mut objects = Vec::new();
    parcel.extend_from_slice(&0u32.to_ne_bytes());
    if tr.target_handle == 0 && (tr.code == ISM_GET_SERVICE || tr.code == ISM_CHECK_SERVICE) {
        if tr.data_size > 0 && tr.data_ptr != 0 {
            let data = std::slice::from_raw_parts(tr.data_ptr as *const u8, tr.data_size as usize);
            if let Some(name) = last_string16(data) {
                if trace_enabled() {
                    diag(&format!("[binder]   -> {name} (handle {})\n", handle_for(&name)));
                }
                push_flat_handle(&mut parcel, &mut objects, handle_for(&name));
            }
        }
    }
    let _ = pid;
    // Pad to 8 bytes: binder's parcel buffers are 8-byte aligned and some
    // readers are stricter about the size than about trailing zero bytes.
    while parcel.len() % 8 != 0 {
        parcel.push(0);
    }
    Reply { parcel, objects }
}

/// Place a reply parcel in the guest's transfer area.
///
/// Returns the address of the parcel and of the object-offset table.
unsafe fn place_reply(reply: &Reply) -> Option<(u64, u64)> {
    let base = MAP_BASE.load(Ordering::Relaxed);
    let len = MAP_LEN.load(Ordering::Relaxed);
    if base == 0 || len == 0 {
        return None;
    }
    let parcel_len = reply.parcel.len();
    let offsets_len = reply.objects.len() * 8;
    let parcel_span = (parcel_len + 7) & !7;
    let offsets_span = (offsets_len + 7) & !7;
    let total = parcel_span + offsets_span;
    let mut off = REPLY_OFFSET.load(Ordering::Relaxed);
    if off + total > len {
        off = 0;
    }
    let p = (base + off) as *mut u8;
    std::ptr::copy_nonoverlapping(reply.parcel.as_ptr(), p, parcel_len);
    let offsets_addr = if offsets_len > 0 {
        let o = (base + off + parcel_span) as *mut u8;
        for (i, v) in reply.objects.iter().enumerate() {
            std::ptr::copy_nonoverlapping(v.to_ne_bytes().as_ptr(), o.add(i * 8), 8);
        }
        (base + off + parcel_span) as u64
    } else {
        0
    };
    REPLY_OFFSET.store(off + total, Ordering::Relaxed);
    if trace_enabled() {
        let mut verify = String::from("[binder]   verify:");
        for i in 0..parcel_len.min(32) {
            verify.push_str(&format!(" {:02x}", *p.add(i)));
        }
        verify.push('\n');
        diag(&verify);
    }
    Some(((base + off) as u64, offsets_addr))
}

/// Read a `TransactionData` from a binder command stream.
///
/// Binder writes a 4-byte command code followed by the struct, so the struct is
/// only 4-byte aligned even though it contains `u64` fields. A plain pointer
/// dereference is undefined behaviour and Rust's `debug_assertions` abort on it
/// ("misaligned pointer dereference"), which killed the guest the first time a
/// real transaction arrived.
///
/// # Safety
/// `buf + off` must point to at least `size_of::<TransactionData>()` readable
/// bytes within the write buffer.
unsafe fn transaction_at(buf: u64, off: u64) -> TransactionData {
    std::ptr::read_unaligned((buf + off) as *const TransactionData)
}

/// Emulate `BINDER_WRITE_READ`.
///
/// Consumes the queued commands and, for every synchronous transaction,
/// produces `BR_TRANSACTION_COMPLETE` followed by a `BR_REPLY` whose parcel
/// lives in the guest's transfer area.
unsafe fn handle_write_read(wr: *mut WriteRead) -> c_int {
    let wr = &mut *wr;
    if trace_enabled() {
        trace_write_read(wr as *const WriteRead);
    }

    let pid = libc::getpid();
    let mut pending: Vec<Reply> = Vec::new();
    let mut off = 0u64;
    while off + 4 <= wr.write_size {
        let cmd = *((wr.write_buffer + off) as *const u32);
        off += 4;
        let Some(size) = command_payload(cmd) else { break };
        if off + size > wr.write_size {
            break;
        }
        if cmd == BC_TRANSACTION {
            // The command stream is only 4-byte aligned (a `u32` command code
            // followed by the struct), so this must not be read through a
            // `*const TransactionData` — Rust's alignment check aborts. Copy
            // the bytes out and reinterpret them with `read_unaligned`.
            let tr = transaction_at(wr.write_buffer, off);
            if tr.flags & TF_ONE_WAY == 0 {
                pending.push(make_reply(&tr, pid));
            }
        }
        off += size;
    }
    // The driver consumes the whole write buffer even for commands we do not
    // interpret; leaving it unconsumed makes libbinder report -EINVAL.
    wr.write_consumed = wr.write_size;

    let mut out: Vec<u8> = Vec::new();
    for reply in &pending {
        push_u32(&mut out, BR_TRANSACTION_COMPLETE);
        push_u32(&mut out, BR_REPLY);
        let mut tr = TransactionData {
            sender_pid: pid,
            data_size: reply.parcel.len() as u64,
            offsets_size: (reply.objects.len() * 8) as u64,
            ..TransactionData::default()
        };
        if let Some((buf, offs)) = place_reply(reply) {
            tr.data_ptr = buf;
            tr.offsets_ptr = offs;
        } else {
            // No transfer area: report an empty parcel rather than a bogus
            // pointer, which libbinder would dereference.
            tr.data_size = 0;
            tr.offsets_size = 0;
        }
        push_transaction(&mut out, &tr);
        if trace_enabled() {
            let mut hex = String::from("[binder]   reply parcel:");
            for b in &reply.parcel {
                hex.push_str(&format!(" {b:02x}"));
            }
            hex.push_str(&format!(" objects={:?} buf={:#x} offs={:#x}\n", reply.objects, tr.data_ptr, tr.offsets_ptr));
            diag(&hex);
        }
    }

    if !out.is_empty() {
        if out.len() as u64 > wr.read_size {
            *libc::__errno_location() = libc::ENOSPC;
            return -1;
        }
        std::ptr::copy_nonoverlapping(out.as_ptr(), wr.read_buffer as *mut u8, out.len());
        wr.read_consumed = out.len() as u64;
        return 0;
    }

    wr.read_consumed = 0;
    if wr.write_size == 0 && wr.read_size > 0 {
        // A looper thread waiting for incoming transactions. Nothing can arrive
        // yet, so sleep briefly instead of returning immediately: a busy loop
        // here would spin a core.
        let ts = libc::timespec { tv_sec: 0, tv_nsec: 20_000_000 };
        libc::nanosleep(&ts, std::ptr::null_mut());
    }
    0
}

/// Record the transfer area mapped by `libbinder` for the emulated device.
pub fn record_mapping(fd: RawFd, addr: usize, len: usize) {
    if is_device(fd) && addr != usize::MAX {
        MAP_BASE.store(addr, Ordering::Relaxed);
        MAP_LEN.store(len, Ordering::Relaxed);
        if trace_enabled() {
            let mut line = format!("[binder] mapped {:#x} len {:#x}\n", addr, len);
            line.retain(|c| c != '\0');
            unsafe { diag(&line) };
        }
    }
}

/// Dump the commands a `BINDER_WRITE_READ` carries.
unsafe fn trace_write_read(wr: *const WriteRead) {
    let wr = &*wr;
    let mut line = format!(
        "[binder] WRITE_READ write={} read={}\n",
        wr.write_size, wr.read_size
    );
    diag(&line);
    // Raw dump: the command stream is the ground truth for the ABI layout.
    let n = (wr.write_size as usize).min(128);
    if n > 0 && wr.write_buffer != 0 {
        let bytes = std::slice::from_raw_parts(wr.write_buffer as *const u8, n);
        let mut hex = String::from("[binder]   raw:");
        for b in bytes {
            hex.push_str(&format!(" {b:02x}"));
        }
        hex.push('\n');
        diag(&hex);
    }
    let mut off = 0u64;
    while off + 4 <= wr.write_size {
        let cmd_ptr = (wr.write_buffer + off) as *const u32;
        let cmd = *cmd_ptr;
        off += 4;
        if cmd == BC_TRANSACTION || cmd == BC_REPLY {
            if off + std::mem::size_of::<TransactionData>() as u64 > wr.write_size {
                break;
            }
            let tr = transaction_at(wr.write_buffer, off);
            line = format!(
                "[binder]   cmd={:#x} target={} code={} flags={:#x} data={} offsets={}\n",
                cmd, tr.target_handle, tr.code, tr.flags, tr.data_size, tr.offsets_size
            );
            diag(&line);
            if tr.data_size > 0 && tr.data_ptr != 0 {
                let show = (tr.data_size as usize).min(160);
                let bytes = std::slice::from_raw_parts(tr.data_ptr as *const u8, show);
                let mut hex = String::from("[binder]   parcel:");
                for b in bytes {
                    hex.push_str(&format!(" {b:02x}"));
                }
                hex.push('\n');
                diag(&hex);
                if tr.offsets_size > 0 && tr.offsets_ptr != 0 {
                    let n = (tr.offsets_size as usize / 8).min(8);
                    let offs = std::slice::from_raw_parts(tr.offsets_ptr as *const u64, n);
                    let mut s = String::from("[binder]   offsets:");
                    for o in offs {
                        s.push_str(&format!(" {o}"));
                    }
                    s.push('\n');
                    diag(&s);
                }
            }
            off += std::mem::size_of::<TransactionData>() as u64;
        } else {
            line = format!("[binder]   cmd={cmd:#x} (stop)\n");
            diag(&line);
            break;
        }
    }
}

/// `int __pengudroid_binder_open(const char *path)`
///
/// Called from the C `open`/`openat` shims before ordinary path handling.
/// Returns a fd for a binder device, or `-1` when the path is something else.
///
/// # Safety
/// `path` must be a valid NUL-terminated string or `NULL`.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_binder_open(path: *const libc::c_char) -> c_int {
    if path.is_null() {
        return -1;
    }
    match open_device(CStr::from_ptr(path).to_bytes()) {
        Some(fd) => fd,
        None => -1,
    }
}

/// `long __pengudroid_binder_ioctl(int fd, unsigned long request, void *arg)`
///
/// `PD_IOCTL_NOT_HANDLED` (`-4096`) tells the C shim to forward the request.
///
/// # Safety
/// `arg` must be valid for `request`.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_binder_ioctl(
    fd: c_int,
    request: c_ulong,
    arg: *mut c_void,
) -> i64 {
    match ioctl(fd, request, arg) {
        Some(result) => result as i64,
        None => -4096,
    }
}

/// `void __pengudroid_binder_mapping(int fd, void *addr, unsigned long len)`
///
/// Called by the C `mmap` shim so replies can be placed in libbinder's
/// transfer area.
///
/// # Safety
/// Called from C with the result of the `mmap` syscall.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_binder_mapping(
    fd: c_int,
    addr: *mut c_void,
    len: usize,
) {
    if !addr.is_null() {
        record_mapping(fd, addr as usize, len);
    }
}

/// `int __pengudroid_binder_mmap_prot(int fd, int prot)`
///
/// libbinder maps the driver read-only; the emulated device must also be
/// writable so replies can be placed in the transfer area.
///
/// # Safety
/// Pure computation.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_binder_mmap_prot(fd: c_int, prot: c_int) -> c_int {
    if is_device(fd) {
        prot | libc::PROT_WRITE
    } else {
        prot
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_the_binder_devices() {
        for p in [
            b"/dev/binder".as_slice(),
            b"/dev/binderfs/binder",
            b"/dev/hwbinder",
            b"/dev/vndbinder",
        ] {
            assert!(is_binder_device(p), "{p:?}");
        }
        for p in [b"/dev/ashmem".as_slice(), b"/system/binder", b"/dev/binderx"] {
            assert!(!is_binder_device(p), "{p:?}");
        }
    }

    #[test]
    fn the_version_ioctl_reports_protocol_8() {
        if open_device(b"/dev/binder").is_none() {
            return; // memfd unavailable on this host
        }
        let fd = BINDER_FD.load(Ordering::Relaxed);
        assert!(is_device(fd));
        let mut version: c_int = 0;
        let r = unsafe {
            ioctl(
                fd,
                BINDER_VERSION,
                &mut version as *mut c_int as *mut c_void,
            )
        };
        assert_eq!(r, Some(0));
        assert_eq!(version, BINDER_CURRENT_PROTOCOL_VERSION);
    }

    #[test]
    fn ioctls_on_other_fds_are_not_handled() {
        assert!(unsafe { ioctl(0, BINDER_VERSION, std::ptr::null_mut()) }.is_none());
    }

    #[test]
    fn command_sizes_match_the_binder_abi() {
        assert_eq!(command_payload(BC_TRANSACTION), Some(64));
        assert_eq!(command_payload(BC_REPLY), Some(64));
        assert_eq!(command_payload(BC_FREE_BUFFER), Some(8));
        assert_eq!(command_payload(BC_ENTER_LOOPER), Some(0));
        assert_eq!(command_payload(0xdead_beef), None);
    }

    /// Build the parcel shape `writeInterfaceToken` + `writeString(name)`
    /// produces: three header words, the descriptor, then the name.
    fn service_request(descriptor: &str, name: &str) -> Vec<u8> {
        fn string16(out: &mut Vec<u8>, s: &str) {
            out.extend_from_slice(&(s.len() as u32).to_ne_bytes());
            for c in s.encode_utf16() {
                out.extend_from_slice(&c.to_ne_bytes());
            }
            out.extend_from_slice(&[0, 0]);
            while out.len() % 4 != 0 {
                out.push(0);
            }
        }
        let mut p = Vec::new();
        p.extend_from_slice(&0x8000_0000u32.to_ne_bytes()); // strict mode
        p.extend_from_slice(&u32::MAX.to_ne_bytes()); // work source uid
        p.extend_from_slice(&0x5359_5354u32.to_ne_bytes()); // token magic
        string16(&mut p, descriptor);
        string16(&mut p, name);
        p
    }

    #[test]
    fn finds_the_service_name_at_the_end_of_a_request() {
        let parcel = service_request("android.os.IServiceManager", "activity");
        assert_eq!(last_string16(&parcel).as_deref(), Some("activity"));
        let parcel = service_request("android.os.IServiceManager", "package");
        assert_eq!(last_string16(&parcel).as_deref(), Some("package"));
    }

    #[test]
    fn handles_are_stable_per_name() {
        let a = handle_for("activity");
        let b = handle_for("activity");
        assert_eq!(a, b);
        assert!(a >= 1);
    }

    #[test]
    fn parses_a_misaligned_transaction() {
        // Binder puts the struct right after a 4-byte command code, so every
        // u64 field lands on a 4-byte boundary. Reading it through a typed
        // pointer aborted the process the first time a real transaction came in.
        let mut buf = vec![0u8; 4 + std::mem::size_of::<TransactionData>() + 4];
        let data = TransactionData {
            target_handle: 0,
            _pad0: 0,
            cookie: 0,
            code: 0x5f61_6374,
            flags: 0,
            sender_pid: 1,
            sender_euid: 2,
            data_size: 8,
            offsets_size: 0,
            data_ptr: 0,
            offsets_ptr: 0,
        };
        unsafe {
            std::ptr::write_unaligned(buf.as_mut_ptr().add(4) as *mut TransactionData, data);
        }
        let base = buf.as_ptr() as u64;
        // Start at 4: the command code occupies the first word.
        let parsed = unsafe { transaction_at(base, 4) };
        assert_eq!(parsed.code, 0x5f61_6374);
        assert_eq!(parsed.data_size, 8);
        assert_eq!(parsed.sender_pid, 1);
    }
}
