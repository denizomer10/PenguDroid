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
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

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
///
/// These are the *ioctl-encoded* values (`_IOW('c', nr, size)`), which is what
/// libbinder actually writes: `0x4000_0000 | size << 16 | 'c' << 8 | nr`. The
/// plain `BC_*` numbers from `binder.h` never appear on the wire, and getting a
/// payload size wrong desynchronises the whole command stream.
const BC_TRANSACTION: u32 = 0x4040_6300;
const BC_REPLY: u32 = 0x4040_6301;
const BC_ACQUIRE_RESULT: u32 = 0x4004_6302;
const BC_FREE_BUFFER: u32 = 0x4008_6303;
const BC_INCREFS: u32 = 0x4004_6304;
const BC_ACQUIRE: u32 = 0x4004_6305;
const BC_RELEASE: u32 = 0x4004_6306;
const BC_DECREFS: u32 = 0x4004_6307;
const BC_INCREFS_DONE: u32 = 0x4010_6308;
const BC_ACQUIRE_DONE: u32 = 0x4010_6309;
const BC_ATTEMPT_ACQUIRE: u32 = 0x4008_630a;
const BC_REGISTER_LOOPER: u32 = 0x0000_630b;
const BC_ENTER_LOOPER: u32 = 0x0000_630c;
const BC_EXIT_LOOPER: u32 = 0x0000_630d;
const BC_REQUEST_DEATH_NOTIFICATION: u32 = 0x4010_630e;
const BC_CLEAR_DEATH_NOTIFICATION: u32 = 0x4010_630f;
const BC_DEAD_BINDER_DONE: u32 = 0x4008_6310;
/// `BC_TRANSACTION_SG` / `BC_REPLY_SG`: the transaction data plus a trailing
/// `binder_size_t buffers_size`. libbinder uses these for every transaction, so
/// missing them desynchronises the command stream.
const BC_TRANSACTION_SG: u32 = 0x4048_6311;
const BC_REPLY_SG: u32 = 0x4048_6312;

/// Binder commands that appear on the read side.
const BR_TRANSACTION_COMPLETE: u32 = 0x0000_7206;
const BR_REPLY: u32 = 0x8040_7203;
const BR_TRANSACTION: u32 = 0x8040_7202;

/// `flat_binder_object.type` for a reference to a binder *in this process*.
///
/// `B_PACK_CHARS('s', 'b', '*', 0x85)`. A client that passes its own binder
/// (the app handing `ApplicationThread` to the service manager) sends this, and
/// the `binder` union member is the local object's address.
const BINDER_TYPE_BINDER: u32 = 0x7362_2A85;

/// Codes still to probe, in order. Seeded from
/// `PENGUDROID_BINDER_PROBE_CODE` (a comma-separated list) when the app node is
/// first captured, so one run can try many AIDL code numbers and compare the
/// replies the app sends back.
static PROBE_CODES: std::sync::LazyLock<std::sync::Mutex<std::collections::VecDeque<u32>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::VecDeque::new()));

fn next_probe_code() -> Option<u32> {
    let mut q = PROBE_CODES.lock().unwrap_or_else(|e| e.into_inner());
    q.pop_front()
}

fn seed_probe_codes() {
    let Ok(raw) = std::env::var("PENGUDROID_BINDER_PROBE_CODE") else {
        return;
    };
    let mut q = PROBE_CODES.lock().unwrap_or_else(|e| e.into_inner());
    q.clear();
    for part in raw.split(',') {
        if let Ok(v) = part.trim().parse::<u32>() {
            q.push_back(v);
        }
    }
    // Default: `IBinder.INTERFACE_TRANSACTION`, which libbinder answers with the
    // interface descriptor - proof that a call reached the app.
    if q.is_empty() {
        q.push_back(0x5f4e_5446);
    }
}

///
/// libbinder flattens a local binder as `obj.binder = getWeakRefs()` and
/// `obj.cookie = this` (the `BBinder*`). An incoming transaction has to carry
/// both: `target.ptr` is the weakref type and `cookie` is the object.
static APP_THREAD_WEAK: AtomicU64 = AtomicU64::new(0);
static APP_THREAD_COOKIE: AtomicU64 = AtomicU64::new(0);
/// Set when a probe transaction should be delivered to `APP_THREAD_NODE`.
static APP_THREAD_PROBE: AtomicBool = AtomicBool::new(false);

/// `IWindow.resized` (AIDL code 2): the callback the window manager sends after
/// an asynchronous relayout.
const IWINDOW_RESIZED: u32 = 2;

/// `IBinder.INTERFACE_TRANSACTION`: answered by every `Binder` with its
/// descriptor, so it is the cheapest proof that a delivery reached a Java stub.
const IBINDER_INTERFACE_TRANSACTION: u32 = 0x5F4E_5446;

/// AIDL descriptor of the app's `IWindow`, which `IWindow.Stub.onTransact`
/// enforces as the parcel's interface token.
const IWINDOW_DESCRIPTOR: &str = "android.view.IWindow";

/// Body of `IWindow.resized` after the interface token, marshalled by the host
/// with the framework's own `Parcel` writers (`ClientWindowFrames` is a
/// framework detail and hand-rolling it shifts every following field).
static RESIZED_PAYLOAD: std::sync::LazyLock<std::sync::Mutex<Vec<u8>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

/// A transaction waiting to be delivered to a binder node owned by this process.
///
/// The kernel binder driver queues these itself; the userspace broker has to
/// keep them and hand them out on the next `BINDER_WRITE_READ` that has room.
struct PendingAppTransaction {
    weak: u64,
    cookie: u64,
    code: u32,
    flags: u32,
    parcel: Vec<u8>,
}

static APP_TX_QUEUE: std::sync::LazyLock<std::sync::Mutex<std::collections::VecDeque<PendingAppTransaction>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::VecDeque::new()));

/// Transaction flag: no reply is expected.
const TF_ONE_WAY: u32 = 0x1;
/// The reply contains a transport `status_t`, not an AIDL exception parcel.
const TF_STATUS_CODE: u32 = 0x8;
/// `utils/Errors.h`: an unimplemented Binder transaction.
const UNKNOWN_TRANSACTION: i32 = -libc::EBADMSG;

/// Number of reply regions carved out of the transfer area.
///
/// Replies used to come from one shared bump pointer (`REPLY_OFFSET`) that was
/// read and written non-atomically. Two threads transacting at the same time
/// therefore got the **same** offset and overwrote each other's reply, and when
/// the pointer wrapped it could reuse bytes another thread was still reading.
/// The symptom was window state that never arrived: a clobbered relayout reply
/// reads back as "no ClientWindowFrames", and `ViewRootImpl` then keeps its
/// empty frame (`mWidth = 0`, `SurfaceView` 0×0, no BLAST queue).
///
/// Each thread now owns a region. That is sufficient because the emulated
/// transaction runs on the *caller's* thread: the reply is written and consumed
/// before that thread leaves `ioctl`. The region still bumps, so a thread never
/// overwrites its own not-yet-read reply ordering.
const REPLY_SLOTS: usize = 16;

/// Next region to hand out (wraps around `REPLY_SLOTS`).
static NEXT_REPLY_SLOT: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// `(slot, next free offset inside the slot)`, `usize::MAX` until assigned.
    static REPLY_SLOT: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((usize::MAX, 0)) };
}

/// Handles handed out to the guest for brokered services.
static SERVICE_HANDLES: std::sync::LazyLock<std::sync::Mutex<std::collections::BTreeMap<String, u32>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::BTreeMap::new()));
static NEXT_HANDLE: AtomicU32 = AtomicU32::new(1);

/// `IBinder::PING_TRANSACTION` ("GNP_").
const PING_TRANSACTION: u32 = 0x5F50_4E47;
/// `IBinder::INTERFACE_TRANSACTION` ("FTN_").
const INTERFACE_TRANSACTION: u32 = 0x5F4E_5446;

// Stable HIDL reserved transactions, not IBinder's system transactions.
// system/libhwbinder@2f528244/include/hwbinder/IBinder.h.
const HIDL_INTERFACE_CHAIN: u32 = 0x0F43_484E;
const HIDL_INTERFACE_DESCRIPTOR: u32 = 0x0F44_5343;
const HIDL_PING: u32 = 0x0F50_4E47;
const HIDL_BASE: &str = "android.hidl.base@1.0::IBase";
const HIDL_MANAGER_HANDLE: &str = "hidl.service_manager";
const HIDL_MANAGER_CHAIN: [&str; 4] = [
    "android.hidl.manager@1.2::IServiceManager",
    "android.hidl.manager@1.1::IServiceManager",
    "android.hidl.manager@1.0::IServiceManager",
    HIDL_BASE,
];
const HIDL_COMPONENT_STORE: &str = "android.hardware.media.c2@1.0::IComponentStore";
const HIDL_GET: u32 = 1;
const HIDL_GET_TRANSPORT: u32 = 3;
const HIDL_LIST_BY_INTERFACE: u32 = 5;
const HIDL_LIST_MANIFEST_BY_INTERFACE: u32 = 13;

/// Is this transaction a HIDL call rather than a Java `Parcel` one?
///
/// A Java parcel is `[u32 strictModePolicy][i32 workSourceUid][String16
/// interfaceToken]`, so the token `TSYS` sits at offset 8. HIDL payloads carry
/// only the method arguments and never have that header.
fn is_hidl_request(tr: &TransactionData) -> bool {
    if tr.data_size < 12 || tr.data_ptr == 0 {
        return true;
    }
    // SAFETY: the driver guarantees `data_size` readable bytes at `data_ptr`.
    let data = unsafe { std::slice::from_raw_parts(tr.data_ptr as *const u8, 12) };
    &data[8..12] != b"TSYS"
}

/// Append a `binder_buffer_object` to `parcel` and record its offset.
///
/// HIDL never inlines a string: `writeEmbeddedBuffer` writes this 40-byte
/// record into the parcel data and puts its position in the offsets array,
/// which `readObject<T>(handle)` then indexes. Returns the object's index in
/// `objects`, which is what a child's `parent` field refers to.
pub(crate) fn push_buffer_object(
    parcel: &mut Vec<u8>,
    objects: &mut Vec<u64>,
    flags: u32,
    data: u64,
    length: u64,
    parent: u64,
    parent_offset: u64,
) -> u64 {
    let off = parcel.len() as u64;
    parcel.extend_from_slice(&0x7074_2a85u32.to_ne_bytes()); // type
    parcel.extend_from_slice(&flags.to_ne_bytes());
    parcel.extend_from_slice(&data.to_ne_bytes());
    parcel.extend_from_slice(&length.to_ne_bytes());
    parcel.extend_from_slice(&parent.to_ne_bytes());
    parcel.extend_from_slice(&parent_offset.to_ne_bytes());
    objects.push(off);
    objects.len() as u64 - 1
}

/// Leak a NUL-terminated copy of `s` and return its address.
///
/// The reply parcel is copied into the shared mapping after it is built, so a
/// pointer into the `Vec` would dangle; the client dereferences the pointer
/// after the reply has been handed over, and the number of HIDL strings we
/// emit is tiny.
pub(crate) fn leak_cstr(s: &str) -> (u64, u64) {
    let mut bytes = s.as_bytes().to_vec();
    bytes.push(0);
    let len = bytes.len() as u64;
    let boxed: &'static [u8] = Box::leak(bytes.into_boxed_slice());
    (boxed.as_ptr() as u64, len)
}

/// Read the `index`-th `hidl_string` argument of a HIDL request.
///
/// The parcel is `[interface token][40-byte object][40-byte data]...`, two
/// objects per argument; the odd one carries the bytes at `buffer`/`length`.
unsafe fn hidl_arg_string(tr: &TransactionData, index: usize) -> Option<String> {
    if tr.data_size == 0 || tr.data_ptr == 0 {
        return None;
    }
    let data = std::slice::from_raw_parts(tr.data_ptr as *const u8, tr.data_size as usize);
    let mut off = 0usize;
    while off < data.len() && data[off] != 0 {
        off += 1;
    }
    off = (off + 1 + 3) & !3;
    let obj = off + index * 80 + 40;
    if obj + 40 > data.len() {
        return None;
    }
    let block = &data[obj..obj + 40];
    let ptr = u64::from_ne_bytes(block[8..16].try_into().ok()?) as usize;
    let len = u64::from_ne_bytes(block[16..24].try_into().ok()?) as usize;
    if ptr == 0 || len == 0 || len > 512 {
        return None;
    }
    let bytes = std::slice::from_raw_parts(ptr as *const u8, len);
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(len);
    Some(String::from_utf8_lossy(&bytes[..end]).into_owned())
}

/// Write a `hidl_vec<hidl_string>` reply: the 16-byte vec struct, the element
/// array, and each string, all as `binder_buffer_object` records.
fn push_hidl_string_vec(parcel: &mut Vec<u8>, objects: &mut Vec<u64>, names: &[&str]) {
    let mut data: Vec<(u64, u64)> = Vec::with_capacity(names.len());
    for name in names {
        data.push(leak_cstr(name));
    }
    let mut arr = Vec::with_capacity(16 * names.len());
    for (ptr, len) in &data {
        arr.extend_from_slice(&ptr.to_ne_bytes());
        arr.extend_from_slice(&(len - 1).to_ne_bytes());
    }
    let arr: &'static [u8] = Box::leak(arr.into_boxed_slice());

    let mut vec = Vec::with_capacity(16);
    vec.extend_from_slice(&(arr.as_ptr() as u64).to_ne_bytes());
    vec.extend_from_slice(&(names.len() as u32).to_ne_bytes());
    vec.extend_from_slice(&0u32.to_ne_bytes());
    let vec: &'static [u8] = Box::leak(vec.into_boxed_slice());

    let vec_idx = push_buffer_object(parcel, objects, 0, vec.as_ptr() as u64, 16, 0, 0);
    let arr_idx = push_buffer_object(
        parcel,
        objects,
        1,
        arr.as_ptr() as u64,
        (16 * names.len()) as u64,
        vec_idx,
        0,
    );
    for (i, (ptr, len)) in data.iter().enumerate() {
        push_buffer_object(parcel, objects, 1, *ptr, *len, arr_idx, (i * 16) as u64);
    }
}

/// LP64 hidl_string/hidl_vec header. `owns_buffer` and its padding stay zero.
/// system/libhidl@061055fc/base/include/hidl/HidlSupport.h.
#[repr(C)]
struct HidlHeader {
    data: u64,
    size: u32,
    owns_buffer: u32,
}

/// Immutable backing storage shared by replies. The transfer area copies only
/// object records, not their pointees; these allocations must outlive every
/// read. A finite set of LazyLocks avoids leaking fresh buffers on each query.
struct HidlStrings {
    strings: Vec<Box<[u8]>>,
    elements: Box<[HidlHeader]>,
    header: HidlHeader,
}

impl HidlStrings {
    fn new(names: &[&str]) -> Self {
        let strings: Vec<Box<[u8]>> = names.iter().map(|name| {
            let mut bytes = name.as_bytes().to_vec();
            bytes.push(0);
            bytes.into_boxed_slice()
        }).collect();
        let elements: Box<[HidlHeader]> = strings.iter().map(|bytes| HidlHeader {
            data: bytes.as_ptr() as u64,
            size: (bytes.len() - 1) as u32,
            owns_buffer: 0,
        }).collect();
        let header = HidlHeader {
            data: if elements.is_empty() { 0 } else { elements.as_ptr() as u64 },
            size: elements.len() as u32,
            owns_buffer: 0,
        };
        Self { strings, elements, header }
    }

    fn write_vec(&self, parcel: &mut Vec<u8>, objects: &mut Vec<u64>) {
        let root = push_buffer_object(parcel, objects, 0,
            &self.header as *const HidlHeader as u64, 16, 0, 0);
        let array = push_buffer_object(parcel, objects, 1, self.header.data,
            (self.elements.len() * 16) as u64, root, 0);
        if self.elements.is_empty() {
            // libhwbinder Parcel::writeObject keeps the null record in the
            // parcel, but omits it from the object-offset table.
            objects.pop();
        }
        for (index, bytes) in self.strings.iter().enumerate() {
            push_buffer_object(parcel, objects, 1, bytes.as_ptr() as u64,
                bytes.len() as u64, array, (index * 16) as u64);
        }
    }

    fn write_string(&self, parcel: &mut Vec<u8>, objects: &mut Vec<u64>) {
        assert_eq!(self.elements.len(), 1);
        let root = push_buffer_object(parcel, objects, 0,
            self.elements.as_ptr() as u64, 16, 0, 0);
        let bytes = &self.strings[0];
        push_buffer_object(parcel, objects, 1, bytes.as_ptr() as u64,
            bytes.len() as u64, root, 0);
    }
}

static HIDL_MANAGER_STRINGS: std::sync::LazyLock<HidlStrings> =
    std::sync::LazyLock::new(|| HidlStrings::new(&HIDL_MANAGER_CHAIN));
static HIDL_MANAGER_DESCRIPTOR: std::sync::LazyLock<HidlStrings> =
    std::sync::LazyLock::new(|| HidlStrings::new(&HIDL_MANAGER_CHAIN[..1]));
static HIDL_EMPTY_INSTANCES: std::sync::LazyLock<HidlStrings> =
    std::sync::LazyLock::new(|| HidlStrings::new(&[]));
static HIDL_DEFAULT_INSTANCE: std::sync::LazyLock<HidlStrings> =
    std::sync::LazyLock::new(|| HidlStrings::new(&["default"]));

unsafe fn hidl_token(tr: &TransactionData) -> Option<&str> {
    if tr.data_ptr == 0 || tr.data_size == 0 { return None; }
    let data = std::slice::from_raw_parts(tr.data_ptr as *const u8, tr.data_size as usize);
    let end = data.iter().take(512).position(|&b| b == 0)?;
    std::str::from_utf8(&data[..end]).ok()
}

fn hosted_hidl_service(fq: &str, instance: &str) -> Option<&'static str> {
    if instance != "default" { return None; }
    if HIDL_MANAGER_CHAIN[..3].contains(&fq) {
        Some(HIDL_MANAGER_HANDLE)
    } else if fq == HIDL_COMPONENT_STORE && crate::codec2::available() {
        Some("codec2")
    } else {
        None
    }
}

/// HIDL binders do not carry AIDL's extra stability word. In particular a
/// nullable result is BINDER_TYPE_BINDER with zero flags/pointers, and has no
/// entry in the offsets table; handle 0 would mean the context object instead.
fn push_hidl_binder(parcel: &mut Vec<u8>, objects: &mut Vec<u64>, handle: Option<u32>) {
    if let Some(handle) = handle {
        objects.push(parcel.len() as u64);
        parcel.extend_from_slice(&BINDER_TYPE_HANDLE.to_ne_bytes());
        parcel.extend_from_slice(&0x100u32.to_ne_bytes()); // ACCEPTS_FDS
        parcel.extend_from_slice(&(handle as u64).to_ne_bytes());
        parcel.extend_from_slice(&0u64.to_ne_bytes());
    } else {
        parcel.extend_from_slice(&BINDER_TYPE_BINDER.to_ne_bytes());
        parcel.extend_from_slice(&[0u8; 20]);
    }
}

/// The context object and every manager-version self-lookup share this
/// dispatcher. Source contracts: libhwbinder@2f528244, libhidl@061055fc,
/// hwservicemanager@60271965. Unsupported calls fail; absent services are typed
/// successful results, otherwise Codec2Client retries manifest discovery forever.
unsafe fn hidl_service_manager_reply(
    tr: &TransactionData, parcel: &mut Vec<u8>, objects: &mut Vec<u64>,
) -> Result<(), &'static str> {
    let expected_token = match tr.code {
        PING_TRANSACTION if tr.data_size == 0 => return Ok(()),
        HIDL_INTERFACE_CHAIN | HIDL_INTERFACE_DESCRIPTOR | HIDL_PING => HIDL_BASE,
        HIDL_GET | HIDL_GET_TRANSPORT | HIDL_LIST_BY_INTERFACE => HIDL_MANAGER_CHAIN[2],
        HIDL_LIST_MANIFEST_BY_INTERFACE => HIDL_MANAGER_CHAIN[0],
        _ => return Err("unsupported HIDL IServiceManager method"),
    };
    if hidl_token(tr) != Some(expected_token) {
        return Err("HIDL method requires its declaring interface token");
    }
    match tr.code {
        HIDL_INTERFACE_CHAIN => HIDL_MANAGER_STRINGS.write_vec(parcel, objects),
        HIDL_INTERFACE_DESCRIPTOR => HIDL_MANAGER_DESCRIPTOR.write_string(parcel, objects),
        HIDL_PING => {},
        HIDL_GET | HIDL_GET_TRANSPORT => {
            let fq = hidl_arg_string(tr, 0).ok_or("missing HIDL service descriptor")?;
            let instance = hidl_arg_string(tr, 1).ok_or("missing HIDL service instance")?;
            let service = hosted_hidl_service(&fq, &instance);
            if tr.code == HIDL_GET {
                push_hidl_binder(parcel, objects, service.map(handle_for));
            } else {
                // Transport is uint8_t, but Parcel::writeUint8/readUint8 use
                // write/read with four-byte padding, not a raw byte stream.
                parcel.extend_from_slice(&u32::from(service.is_some()).to_ne_bytes());
            }
            if trace_enabled() {
                diag(&format!("[binder] hidl discovery code={} {fq}/{instance}: {service:?}\n", tr.code));
            }
        }
        HIDL_LIST_BY_INTERFACE | HIDL_LIST_MANIFEST_BY_INTERFACE => {
            let fq = hidl_arg_string(tr, 0).ok_or("missing HIDL list interface")?;
            // Code 5 lists registered instances; code 13 lists declared ones.
            // PenguDroid has no lazy HALs, so both use the actual hosted set,
            // never a GSI manifest whose services cannot run on the host.
            let available = hosted_hidl_service(&fq, "default").is_some();
            if available {
                HIDL_DEFAULT_INSTANCE.write_vec(parcel, objects);
            } else {
                HIDL_EMPTY_INSTANCES.write_vec(parcel, objects);
            }
            if trace_enabled() {
                diag(&format!("[binder] hidl instances code={} {fq}: {}\n", tr.code,
                    if available { "[default]" } else { "[]" }));
            }
        }
        _ => unreachable!(),
    }
    Ok(())
}

/// API-37 IServiceManager get/check binder replies, followed by get/check
/// Service-union replies. No transaction is known until the host installs all four.
static SERVICE_MANAGER_CODES: Mutex<[u32; 4]> = Mutex::new([0; 4]);

fn service_manager_shape(codes: [u32; 4], code: u32) -> Option<bool> {
    if code == 0 || codes.contains(&0) { return None; }
    if codes[..2].contains(&code) { Some(false) }
    else if codes[2..].contains(&code) { Some(true) }
    else { None }
}

/// Install getService/checkService/getService2/checkService2 from the loaded Stub.
/// All four must be nonzero, distinct user transaction codes. Failure leaves the
/// previously installed table unchanged.
#[no_mangle]
pub extern "C" fn __pengudroid_binder_set_service_manager_codes(
    get: u32, check: u32, get2: u32, check2: u32,
) -> bool {
    let codes = [get, check, get2, check2];
    if codes.iter().any(|&c| c == 0 || c >= 0x00ff_fffe)
        || codes.iter().enumerate().any(|(i, c)| codes[..i].contains(c)) {
        return false;
    }
    *SERVICE_MANAGER_CODES.lock().unwrap_or_else(|e| e.into_inner()) = codes;
    true
}


/// `android.content.pm.IPackageManager` transaction code for `getPackageInfo`.
///
/// Read off a live transaction (`PENGUDROID_TRACE_BINDER=1`): the app asks the
/// `package` service with code 3 and the package name as the only argument.
const IPM_GET_PACKAGE_INFO: u32 = 3;

/// `IPackageManager` transaction code for `getApplicationInfo`.
///
/// Also read off a live transaction: code 9, arguments
/// `(String packageName, int flags, int userId)`.
const IPM_GET_APPLICATION_INFO: u32 = 9;

/// `IPackageManager` transaction code for `getActivityInfo`.
///
/// `AppCompatDelegate` resolves the activity through the package manager while
/// it is being created; a `NameNotFoundException` there makes it fall back to
/// the platform theme, which is not an AppCompat theme.
const IPM_GET_ACTIVITY_INFO: u32 = 11;

/// `IPackageManager` transaction code for `getProviderInfo`.
///
/// `androidx.startup.InitializationProvider.onCreate` reads its own
/// `ProviderInfo.metaData` to find the initializer list, so this has to answer
/// with the entry the host marshalled for that class.
const IPM_GET_PROVIDER_INFO: u32 = 15;

/// `IActivityTaskManager` transaction code for `getActivityClientController`.
///
/// `ActivityClient.getDisplayId` calls it through a `Singleton`; with no system
/// server the reply is empty, the singleton caches null and the next call NPEs
/// inside `createBaseContextForActivity`.
/// ActivityTaskManager `getActivityClientController`, then IActivityManager
/// `getContentProvider`/`startService`/`bindService`/`bindServiceInstance`.
/// Android 17 replaced `bindIsolatedService` with `bindServiceInstance`.
///
/// Hard-locked to API 37: the table is all-`u32::MAX` (unknown) until ART
/// installs the loaded framework's own `Stub` constants. No API-33 number is
/// ever guessed, so a native-only launch never mistakes an unrelated
/// transaction for a service call and a missing table fails binding loudly.
static APPLICATION_TRANSACTION_CODES: Mutex<[u32; 5]> = Mutex::new([u32::MAX; 5]);

#[no_mangle]
pub extern "C" fn __pengudroid_binder_set_application_transactions(
    controller: u32,
    provider: u32,
    start_service: u32,
    bind_service: u32,
    bind_service_instance: u32,
) -> bool {
    let codes = [controller, provider, start_service, bind_service, bind_service_instance];
    if codes.iter().any(|&code| code == 0 || code >= 0x00ff_fffe) { return false; }
    *APPLICATION_TRANSACTION_CODES.lock().unwrap_or_else(|e| e.into_inner()) = codes;
    true
}

/// `startActivity` on the task/activity manager, then `finishActivity` on the
/// activity manager/client controller. Installed from the loaded API-37 Stubs.
static ACTIVITY_LIFECYCLE_CODES: Mutex<[u32; 4]> = Mutex::new([u32::MAX; 4]);
static ACTIVITY_START_RESULTS: Mutex<Option<[i32; 4]>> = Mutex::new(None);

/// The four `ActivityManager.START_*` constants: success, unresolved intent,
/// class not found, and canceled. Never choose framework result codes by guess.
///
/// # Safety
/// `codes` must reference four readable i32s when `len == 4`.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_binder_set_activity_start_results(codes: *const i32, len: usize) -> bool {
    if codes.is_null() || len != 4 { return false; }
    let Ok(values) = <[i32; 4]>::try_from(std::slice::from_raw_parts(codes, len)) else { return false; };
    if values[0] != 0 || values[1..].iter().any(|v| *v >= 0) { return false; }
    *ACTIVITY_START_RESULTS.lock().unwrap_or_else(|e| e.into_inner()) = Some(values);
    true
}

/// Install the four lifecycle transaction codes.
#[no_mangle]
pub extern "C" fn __pengudroid_binder_set_activity_lifecycle_transactions(
    start_task: u32,
    start_activity: u32,
    finish: u32,
    finish_client: u32,
) -> bool {
    let codes = [start_task, start_activity, finish, finish_client];
    if codes.iter().any(|&code| code == 0 || code >= 0x00ff_fffe) { return false; }
    *ACTIVITY_LIFECYCLE_CODES.lock().unwrap_or_else(|e| e.into_inner()) = codes;
    true
}

/// The hosted application's package, set before `handleBindApplication`.
///
/// Only an activity whose `Intent` names a component in this package can be
/// launched by the host; resolving another package would need a real package
/// manager. The shim checks the component before admitting a request, so an
/// unhostable start fails in the caller instead of being acknowledged.
static HOSTED_PACKAGE: Mutex<Option<String>> = Mutex::new(None);
static HOSTED_ACTIVITIES: Mutex<std::collections::BTreeSet<String>> = Mutex::new(std::collections::BTreeSet::new());

/// Install the NUL-separated UTF-8 class names from the hosted manifest.
///
/// # Safety
/// `ptr` must reference `len` readable bytes; no pointer is retained.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_binder_set_hosted_activities(ptr: *const u8, len: usize) -> bool {
    if ptr.is_null() || len > 1024 * 1024 { return false; }
    let Ok(text) = std::str::from_utf8(std::slice::from_raw_parts(ptr, len)) else { return false; };
    let mut names = std::collections::BTreeSet::new();
    for name in text.split('\0').filter(|name| !name.is_empty()) {
        if name.len() > 1024 { return false; }
        names.insert(name.to_owned());
    }
    *HOSTED_ACTIVITIES.lock().unwrap_or_else(|e| e.into_inner()) = names;
    true
}

const ACTIVITY_HOST_DESCRIPTOR: &str = "org.pengudroid.IHostedActivity";
// Native Binder identities are registered through a private in-process Binder
// transaction. No Java handle addresses or guessed JavaBBinder layout is used.
static ACTIVITY_TOKENS: Mutex<std::collections::BTreeMap<(u64, u64), u64>> =
    Mutex::new(std::collections::BTreeMap::new());
static ACTIVITY_WAKEUP: AtomicBool = AtomicBool::new(false);

#[no_mangle]
pub extern "C" fn __pengudroid_binder_take_activity_wakeup() -> bool {
    ACTIVITY_WAKEUP.swap(false, Ordering::AcqRel)
}

#[no_mangle]
pub extern "C" fn __pengudroid_binder_forget_activity_token(id: u64) {
    ACTIVITY_TOKENS.lock().unwrap_or_else(|e| e.into_inner()).retain(|_, value| *value != id);
}

#[no_mangle]
pub unsafe extern "C" fn __pengudroid_binder_set_hosted_package(ptr: *const u8, len: usize) -> bool {
    if ptr.is_null() || len == 0 {
        return false;
    }
    let bytes = std::slice::from_raw_parts(ptr, len);
    let Ok(package) = std::str::from_utf8(bytes) else { return false; };
    *HOSTED_PACKAGE.lock().unwrap_or_else(|e| e.into_inner()) = Some(package.to_string());
    true
}


/// ISessionManager.createSession, ISession.getController/getBinderForSetQueue,
/// IMediaMetricsManager.getPlaybackSessionId, installed from the API-37 Stubs.
static MEDIA_TRANSACTION_CODES: Mutex<Option<[u32; 4]>> = Mutex::new(None);

/// `bool __pengudroid_binder_set_media_transactions(const u32 *codes, size_t len)`
///
/// `len` must be 4 (u32 elements, not bytes). The order is createSession,
/// getController, getBinderForSetQueue, getPlaybackSessionId, from the loaded
/// android.media.session.ISessionManager$Stub, android.media.session.ISession$Stub
/// (the middle two), and android.media.metrics.IMediaMetricsManager$Stub.
/// Every code must be in 1..0x00ff_fffe. Only the middle pair must be distinct:
/// different Stub types may reuse the same code. The table is copied and
/// replaced atomically; false leaves the previous table unchanged. No media
/// methods dispatch before a successful installation.
///
/// # Safety
/// For a non-null pointer and len == 4, `codes` must point to four aligned,
/// readable u32s for the duration of this call. The pointer is not retained.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_binder_set_media_transactions(
    codes: *const u32,
    len: usize,
) -> bool {
    if codes.is_null() || len != 4 { return false; }
    let codes: [u32; 4] = std::slice::from_raw_parts(codes, len).try_into().unwrap();
    if codes.iter().any(|&code| code == 0 || code >= 0x00ff_fffe)
        || codes[1] == codes[2] {
        return false;
    }
    *MEDIA_TRANSACTION_CODES.lock().unwrap_or_else(|e| e.into_inner()) = Some(codes);
    true
}

/// API-37 IUserManager unlock queries, unknown until installed from its Stub.
static USER_TRANSACTION_CODES: Mutex<Option<[u32; 2]>> = Mutex::new(None);

/// Install IUserManager$Stub codes in isUserUnlocked/isUserUnlockingOrUnlocked
/// order. `len` must be 2 u32 elements; both codes must be distinct and in
/// 1..0x00ff_fffe. The table is copied and replaced as one unit. False preserves
/// the previous table; no user method dispatches before installation.
///
/// # Safety
/// For a non-null pointer and len == 2, `codes` must point to two aligned,
/// readable u32s for this call. The pointer is not retained.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_binder_set_user_transactions(
    codes: *const u32,
    len: usize,
) -> bool {
    if codes.is_null() || len != 2 { return false; }
    let codes: [u32; 2] = std::slice::from_raw_parts(codes, len).try_into().unwrap();
    if codes.iter().any(|&code| code == 0 || code >= 0x00ff_fffe)
        || codes[0] == codes[1] {
        return false;
    }
    *USER_TRANSACTION_CODES.lock().unwrap_or_else(|e| e.into_inner()) = Some(codes);
    true
}

/// `IContentProvider` transaction codes, read out of this framework.jar with
/// `--dump-class android.content.IContentProvider`.
///
/// They are *not* the AOSP source order (the AIDL is generated), so they are
/// listed explicitly rather than derived from a method list.
const ICP_QUERY: u32 = 1;
const ICP_GET_TYPE: u32 = 2;
const ICP_INSERT: u32 = 3;
const ICP_DELETE: u32 = 4;
const ICP_UPDATE: u32 = 10;
const ICP_BULK_INSERT: u32 = 13;
const ICP_OPEN_FILE: u32 = 14;
const ICP_OPEN_ASSET_FILE: u32 = 15;
const ICP_APPLY_BATCH: u32 = 20;
const ICP_CALL: u32 = 21;
const ICP_GET_STREAM_TYPES: u32 = 22;
const ICP_OPEN_TYPED_ASSET_FILE: u32 = 23;
const ICP_CREATE_CANCELATION_SIGNAL: u32 = 24;
const ICP_CANONICALIZE: u32 = 25;
const ICP_UNCANONICALIZE: u32 = 26;
const ICP_REFRESH: u32 = 27;
const ICP_CHECK_URI_PERMISSION: u32 = 28;

/// `IBulkCursor` transaction codes (`BulkCursorNative`).
const IBC_GET_CURSOR_WINDOW: u32 = 1;
const IBC_CLOSE: u32 = 4;

/// Measured API-37 android.gui.ISurfaceComposer AIDL transactions. Every reply
/// starts with binder::Status, regardless of the SurfaceFlinger service alias.
const ISC_CREATE_DISPLAY_EVENT_CONNECTION: u32 = 2;
const ISC_CREATE_CONNECTION: u32 = 3;
const ISC_GET_PHYSICAL_DISPLAY_IDS: u32 = 6;
const ISC_GET_PHYSICAL_DISPLAY_TOKEN: u32 = 7;
const ISC_GET_STATIC_DISPLAY_INFO: u32 = 12;
const ISC_GET_DYNAMIC_DISPLAY_INFO: u32 = 13;
const ISC_GET_COMPOSITION_PREFERENCE: u32 = 34;
const ISC_GET_MAX_ACQUIRED_BUFFER_COUNT: u32 = 69;

/// API-37 android.gui.ISurfaceComposerClient.createSurface (generated AIDL).
const ISCC_CREATE_SURFACE: u32 = 1;

/// Tags of the **API-37 C++** `ISurfaceComposer` interface.
///
/// `libgui.so` still ships both interfaces on the same SurfaceFlinger handle:
/// the generated AIDL one above (`android.gui.ISurfaceComposer`) and the C++ one
/// whose descriptor is `android.ui.ISurfaceComposer`. The numbers are the
/// `ISurfaceComposerTag` enum in the pinned API-37
/// `libs/gui/include/gui/ISurfaceComposer.h`, starting at
/// `IBinder::FIRST_CALL_TRANSACTION`. They are this platform's own tags, not an
/// older release's: `SET_TRANSACTION_STATE` is `8`, measured on a live run
/// (`ISurfaceComposer code=8 token mismatch: first16=Some("android.ui.ISurfaceComposer")`).
const ISC_CPP_BOOT_FINISHED: u32 = 1;
const ISC_CPP_SET_TRANSACTION_STATE: u32 = 8;

/// Accepted C++ `ISurfaceComposer` transactions. The host owns no layers, so the
/// count is what proves a caller reached this broker and was not refused, not a
/// claim that a frame was composed.
static CPP_TRANSACTION_STATES: AtomicU64 = AtomicU64::new(0);

/// `IDisplayEventConnection` transaction code for `stealReceiveChannel`.
///
/// This one *is* AIDL, so the reply starts with a `Status` int32 and then the
/// `out BitTube`, which `BitTube::readFromParcel` reads as two file descriptors.
const IDEC_STEAL_RECEIVE_CHANNEL: u32 = 1;

/// The send ends of the vsync channels handed out by `stealReceiveChannel`.
///
/// They are kept open for the life of the process: closing the send end would
/// make the client's `recv` see EOF and tear the `Choreographer` down.
static VSYNC_SEND_FDS: std::sync::LazyLock<std::sync::Mutex<Vec<RawFd>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

/// `IWindowManager` transaction code for `openSession`.
const IWM_OPEN_SESSION: u32 = 4;

/// Semantic identifiers for the `IWindowSession` methods this broker answers.
///
/// The numeric transaction codes are *not* stable across releases: Android 17
/// inserted `relayout2`/`relayoutAsync2` after `relayout`, moved `remove` from
/// 4 to 3 and dropped `prepareToReplaceWindows`/`setInTouchMode` entirely. The
/// shim therefore asks the loaded framework for its generated
/// `IWindowSession$Stub.TRANSACTION_*` constants and dispatches on these ids,
/// with no fallback table.
const WS_ADD_TO_DISPLAY: u8 = 1;
const WS_ADD_TO_DISPLAY_AS_USER: u8 = 2;
const WS_REMOVE: u8 = 3;
const WS_RELAYOUT: u8 = 4;
const WS_RELAYOUT2: u8 = 5;
/// `relayoutAsync`: the one-way variant `ViewRootImpl` uses when only the
/// layout params changed. It clears `mWinFrame` and waits for the window
/// manager to call back `IWindow.resized` — the same call, without the reply.
const WS_RELAYOUT_ASYNC: u8 = 6;
const WS_RELAYOUT_ASYNC2: u8 = 7;
const WS_OUT_OF_MEMORY: u8 = 8;
const WS_SET_INSETS: u8 = 9;
const WS_FINISH_DRAWING: u8 = 10;
const WS_GET_IN_TOUCH_MODE: u8 = 11;
const WS_PERFORM_HAPTIC_FEEDBACK: u8 = 12;
const WS_ON_RECTANGLE_ON_SCREEN_REQUESTED: u8 = 13;
const WS_GET_WINDOW_ID: u8 = 14;
const WS_POKE_DRAW_LOCK: u8 = 15;
const WS_UPDATE_POINTER_ICON: u8 = 16;
const WS_UPDATE_TAP_EXCLUDE_REGION: u8 = 17;
const WS_UPDATE_INPUT_CHANNEL: u8 = 18;
const WS_CLEAR_TOUCHABLE_REGION: u8 = 19;
const WS_CANCEL_DRAW: u8 = 20;
const WS_MOVE_FOCUS_TO_ADJACENT_WINDOW: u8 = 21;
const WS_GRANT_INPUT_CHANNEL: u8 = 22;

/// The API-37 `code -> semantic id` table, installed by the host from the
/// loaded framework's own `IWindowSession$Stub`.
///
/// Android 13 support was removed on purpose: there is no legacy default, so a
/// window session transaction is only answered once the host has published the
/// real Android 17 constants. An empty table means "framework not initialised",
/// not "fall back to older numbers".
static WINDOW_SESSION_CODES: std::sync::LazyLock<Mutex<Vec<(u32, u8)>>> =
    std::sync::LazyLock::new(|| Mutex::new(Vec::new()));

/// Map a transaction code to the method this broker answers, if any.
fn window_session_method(code: u32) -> Option<u8> {
    WINDOW_SESSION_CODES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, m)| *m)
}

/// `bool __pengudroid_binder_set_window_session_codes(const u32 *pairs, size_t len)`
///
/// `pairs` is `[code, semantic_id, code, semantic_id, ...]`.
///
/// # Safety
/// `pairs` must point to `len` readable `u32`s.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_binder_set_window_session_codes(
    pairs: *const u32,
    len: usize,
) -> bool {
    if pairs.is_null() || len == 0 || len % 2 != 0 {
        return false;
    }
    let values = std::slice::from_raw_parts(pairs, len);
    let mut table: Vec<(u32, u8)> = Vec::with_capacity(len / 2);
    for pair in values.chunks_exact(2) {
        let (code, method) = (pair[0], pair[1] as u8);
        if code == 0 || code >= 0x00ff_fffe || method == 0 {
            continue;
        }
        if table.iter().any(|&(c, m)| c == code || m == method) {
            continue;
        }
        table.push((code, method));
    }
    if table.is_empty() {
        return false;
    }
    *WINDOW_SESSION_CODES.lock().unwrap_or_else(|e| e.into_inner()) = table;
    true
}

/// A marshalled `android.view.WindowRelayoutResult` from the loaded Android 17
/// framework.
///
/// Android 17 replaced the four separate `relayout` out-parameters
/// (`ClientWindowFrames`, `MergedConfiguration`, `InsetsState`,
/// `InsetsSourceControl[]`) with this single parcelable, so the old reply body
/// no longer lines up with what the client reads.
static WINDOW_RELAYOUT_RESULT: std::sync::LazyLock<std::sync::Mutex<Vec<u8>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

/// The server ends of the input channels handed to the client.
static INPUT_SERVER_FDS: std::sync::LazyLock<std::sync::Mutex<Vec<RawFd>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

/// `IDisplayEventConnection` transaction code for `setVsyncRate`.
const IDEC_SET_VSYNC_RATE: u32 = 2;

/// `IDisplayEventConnection` transaction code for `requestNextVsync` (oneway).
const IDEC_REQUEST_NEXT_VSYNC: u32 = 3;

/// Measured API-37 x86_64 DisplayEventReceiver::Event: a 24-byte header and
/// 200-byte VSync union, including frameTimelinesLength/numberQueuedBuffers.
const DISPLAY_EVENT_SIZE: usize = 224;
/// DisplayEventReceiver::DISPLAY_EVENT_VSYNC, read from libgui's comparison in
/// processPendingEvents (a big-endian fourcc, independent of host byte order).
const DISPLAY_EVENT_VSYNC: u32 = 0x7673_796e;
const VSYNC_FRAME_INTERVAL_NS: i64 = 16_666_666;

/// `ILocaleManager` transaction codes.
///
/// `AppCompatDelegate.getApplicationLocales()` goes through the locale service
/// on API 33+, and `LocaleListCompat.wrap(null)` makes `get(0)` NPE — which is
/// exactly how NewPipe's `Localization.getAppLocale()` dies.
const ILM_GET_APPLICATION_LOCALES: u32 = 2;
const ILM_GET_SYSTEM_LOCALES: u32 = 3;

/// A serialized `ProviderInfo` for the `settings` provider.
static PROVIDER_INFO: std::sync::LazyLock<std::sync::Mutex<Vec<u8>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

/// A serialized empty `Bundle`, the reply to `IContentProvider.call`.
///
/// `Settings.NameValueCache` reads `Bundle.getString(VALUE)`; an empty bundle
/// yields null, so `Settings.Global.getInt` falls back to its default.
static EMPTY_BUNDLE: std::sync::LazyLock<std::sync::Mutex<Vec<u8>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

/// Per-key `Bundle` replies for `Settings.Secure/Global/System.getString`.
///
/// The framework asks the `settings` provider with
/// `IContentProvider.call("GET_secure", <key>, …)` and reads
/// `reply.getString("value")`. There is no provider here, so the host installs a
/// marshalled `Bundle` per key with [`__pengudroid_binder_set_setting`].
///
/// A few keys are load-bearing rather than cosmetic: Roblox reads
/// `Settings.Secure.getString(cr, "android_id")` in `ActivitySplash.onCreate`
/// and keeps the result for the whole process, so returning null there is a
/// device that has no identity at all.
static SETTING_ENTRIES: std::sync::LazyLock<std::sync::Mutex<Vec<(String, Vec<u8>)>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

/// API-37 IThermalService codes, unknown until installed from its Stub.
static THERMAL_TRANSACTION_CODES: Mutex<Option<[u32; 11]>> = Mutex::new(None);

/// Install IThermalService$Stub codes in this semantic order:
/// registerThermalEventListener, registerThermalEventListenerWithType,
/// unregisterThermalEventListener, getCurrentTemperatures,
/// getCurrentTemperaturesWithType, registerThermalStatusListener,
/// unregisterThermalStatusListener, getCurrentThermalStatus,
/// getCurrentCoolingDevices, getCurrentCoolingDevicesWithType, getThermalHeadroom.
///
/// `len` must be 11 u32 elements. All codes must be distinct and in
/// 1..0x00ff_fffe. The table is copied and replaced as one unit. False preserves
/// the previous table; no thermal method dispatches before installation.
///
/// # Safety
/// For a non-null pointer and len == 11, `codes` must point to eleven aligned,
/// readable u32s for this call. The pointer is not retained.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_binder_set_thermal_transactions(
    codes: *const u32,
    len: usize,
) -> bool {
    if codes.is_null() || len != 11 { return false; }
    let codes: [u32; 11] = std::slice::from_raw_parts(codes, len).try_into().unwrap();
    if codes.iter().any(|&code| code == 0 || code >= 0x00ff_fffe)
        || codes.iter().enumerate().any(|(i, code)| codes[..i].contains(code)) {
        return false;
    }
    *THERMAL_TRANSACTION_CODES.lock().unwrap_or_else(|e| e.into_inner()) = Some(codes);
    true
}

/// `IDisplayManager` transaction code for `getDisplayInfo(int)`.
const IDM_GET_DISPLAY_INFO: u32 = 1;

/// Framework-marshalled virtual keyboard InputDevice, including its native map.
static VIRTUAL_INPUT_DEVICE: Mutex<Vec<u8>> = Mutex::new(Vec::new());
// Internal virtual_input::reply semantic IDs, never default wire codes.
const INPUT_OPERATIONS: [u32; 8] = [1, 2, 3, 4, 5, 6, 7, 22];
static INPUT_TRANSACTION_CODES: Mutex<Option<[u32; 8]>> = Mutex::new(None);

/// Install IInputManager$Stub codes in getInputDevice, getInputDeviceIds,
/// isInputDeviceEnabled, enableInputDevice, disableInputDevice, hasKeys,
/// getKeyCodeForKeyLocation, registerInputDevicesChangedListener order.
/// `len` must be 8 u32 elements. Only isInputDeviceEnabled may be zero (absent
/// on API 37); zero never dispatches. Other codes must be distinct and in
/// 1..0x00ff_fffe. False preserves the previous table; there is no default.
///
/// # Safety
/// For a non-null pointer and len == 8, `codes` must point to eight aligned,
/// readable u32s for this call. The contents are copied, not retained.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_binder_set_input_transactions(codes: *const u32, len: usize) -> bool {
    if codes.is_null() || len != 8 { return false; }
    let codes: [u32; 8] = std::slice::from_raw_parts(codes, len).try_into().unwrap();
    if codes.iter().enumerate().any(|(i, &c)| (c == 0 && i != 2) || c >= 0x00ff_fffe)
        || codes.iter().enumerate().any(|(i, c)| *c != 0 && codes[..i].contains(c)) { return false; }
    *INPUT_TRANSACTION_CODES.lock().unwrap_or_else(|e| e.into_inner()) = Some(codes);
    true
}


static AUDIO_FOCUS: crate::audiofocus::AudioFocus = crate::audiofocus::AudioFocus::new();
static AUDIO_FOCUS_TRANSACTIONS: Mutex<Option<crate::audiofocus::Transactions>> =
    Mutex::new(None);

/// Install request/abandon/unregister/current focus codes from the loaded Stub.
/// All must be nonzero, distinct user codes; false preserves the previous table.
/// Until installation, no focus operation dispatches.
#[no_mangle]
pub extern "C" fn __pengudroid_binder_set_audio_focus_transactions(
    request: u32, abandon: u32, unregister: u32, current: u32,
) -> bool {
    let Some(table) = crate::audiofocus::Transactions::new([request, abandon, unregister, current]) else {
        return false;
    };
    *AUDIO_FOCUS_TRANSACTIONS.lock().unwrap_or_else(|e| e.into_inner()) = Some(table);
    true
}


/// A serialized `DisplayInfo` for the default display.
static DISPLAY_INFO: std::sync::LazyLock<std::sync::Mutex<Vec<u8>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

/// A `PackageInfo` reply serialized by the host with the framework's own
/// `PackageInfo.writeToParcel`.
///
/// `LoadedApk.initializeJavaContextClassLoader()` asks `IPackageManager` for the
/// app's `PackageInfo` and refuses to continue when it is null. There is no
/// package manager here, so the host builds the answer from the
/// `ApplicationInfo` it already has; using the framework's writer keeps the
/// parcel layout exactly right instead of hand-rolling a version-specific one.
static PACKAGE_INFO: std::sync::LazyLock<std::sync::Mutex<Vec<u8>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

/// A serialized `ApplicationInfo`, for `IPackageManager.getApplicationInfo`.
static APPLICATION_INFO: std::sync::LazyLock<std::sync::Mutex<Vec<u8>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

/// A serialized `ActivityInfo`, for `IPackageManager.getActivityInfo`.
static ACTIVITY_INFO: std::sync::LazyLock<std::sync::Mutex<Vec<u8>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

/// Marshalled `ProviderInfo`s, keyed by class name.
static PROVIDER_ENTRIES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::BTreeMap<String, Vec<u8>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::BTreeMap::new()));

/// `flat_binder_object.type` for a handle that belongs to another process.
///
/// `B_PACK_CHARS('s', 'h', '*', <low byte>)`; the low byte is the value the
/// Android UAPI calls `BINDER_TYPE_HANDLE` (`0x85` in this build). The value was
/// read out of libbinder's own object-type comparisons rather than guessed.
const BINDER_TYPE_HANDLE: u32 = 0x7368_2A85;

/// `flat_binder_object.type` for a file descriptor.
///
/// `B_PACK_CHARS('f', 'd', '*', BINDER_TYPE_FD)`; the low byte is `0x85` in
/// this build, the same one `BINDER_TYPE_HANDLE` uses.
const BINDER_TYPE_FD: u32 = 0x6664_2A85;

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

/// Write a diagnostic line straight to stderr, stamped with the monotonic clock.
///
/// Logging must not re-enter the JVM (it would call back into binder), so this
/// deliberately uses `write(2)` only.
///
/// The stamp matters because the interesting questions are *ordering*
/// questions — "was the relayout reply written before or after the tap?", "did
/// the window's frame collapse between two probes?" — and the apphost's probe
/// output and this trace come from the same process, so one monotonic clock
/// lines them up exactly.
/// Milliseconds on `CLOCK_MONOTONIC`. The apphost stamps its probe lines with
/// the same clock, so the two logs can be aligned exactly.
pub(crate) fn monotonic_ms() -> u128 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid `timespec`.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u128 * 1000 + ts.tv_nsec as u128 / 1_000_000
}

unsafe fn diag(line: &str) {
    let stamp = format!("[+{} ms] {line}", monotonic_ms());
    libc::write(2, stamp.as_ptr() as *const c_void, stamp.len());
}

fn trace_enabled() -> bool {
    std::env::var_os("PENGUDROID_TRACE_BINDER").is_some()
}

/// Whether `path` names one of Android's binder devices.
pub(crate) fn is_binder_device(path: &[u8]) -> bool {
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
///
/// Binder commands are ioctl-encoded (`_IOW('c', nr, type)`), so the payload
/// size is part of the command word itself: bits 16..30. Deriving it from there
/// is what keeps the stream aligned — a hand-written table silently desyncs the
/// moment a command carries a different argument type than expected (which is
/// exactly how `BC_TRANSACTION_SG` used to swallow the next transaction).
fn command_payload(cmd: u32) -> Option<u64> {
    if (cmd >> 8) & 0xff != u32::from(b'c') {
        return None;
    }
    let direction = cmd >> 30;
    let size = ((cmd >> 16) & 0x3fff) as u64;
    match direction {
        0 if size == 0 => Some(0),
        1 if size <= 4096 => Some(size),
        _ => None,
    }
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
    flags: u32,
}

impl Reply {
    fn unknown_transaction(tr: &TransactionData, reason: &str) -> Self {
        unsafe {
            diag(&format!(
                "[binder] UNKNOWN_TRANSACTION target={} ({}) code={}: {reason}\n",
                tr.target_handle,
                name_for_handle(tr.target_handle).as_deref().unwrap_or("service_manager"),
                tr.code,
            ));
        }
        Self {
            parcel: UNKNOWN_TRANSACTION.to_ne_bytes().to_vec(),
            objects: Vec::new(),
            flags: TF_STATUS_CODE,
        }
    }
}

/// Keep native replies byte-for-byte; a missing reply is not a software codec.
fn codec2_dispatch_reply(tr: &TransactionData, reply: Option<(Vec<u8>, Vec<u64>)>) -> Reply {
    let Some((parcel, objects)) = reply else {
        return Reply::unknown_transaction(tr, "native Codec2 dispatch failed; no fabricated reply");
    };
    if trace_enabled() {
        unsafe {
            diag(&format!(
                "[binder]   -> codec2 service code {} ({} bytes, {} objects)\n",
                tr.code, parcel.len(), objects.len()
            ));
        }
    }
    Reply { parcel, objects, flags: 0 }
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

/// Encode our own reply from the AIDL schema. New clients read a typed Service
/// union (tag 0) containing a typed, size-delimited ServiceWithMetadata.
fn push_service_reply(parcel: &mut Vec<u8>, objects: &mut Vec<u64>,
    handle: Option<u32>, metadata: bool) {
    let size_at = if metadata {
        for value in [1u32, 0, 1] { parcel.extend_from_slice(&value.to_ne_bytes()); }
        let at = parcel.len();
        parcel.extend_from_slice(&0u32.to_ne_bytes());
        Some(at)
    } else { None };
    if let Some(handle) = handle {
        push_flat_handle(parcel, objects, handle);
    } else {
        parcel.extend_from_slice(&BINDER_TYPE_BINDER.to_ne_bytes());
        parcel.extend_from_slice(&0x7fu32.to_ne_bytes());
        parcel.extend_from_slice(&[0u8; 16]);
    }
    if let Some(at) = size_at {
        parcel.extend_from_slice(&0u32.to_ne_bytes()); // isLazyService = false
        let size = (parcel.len() - at) as u32;
        parcel[at..at + 4].copy_from_slice(&size.to_ne_bytes());
    }
}

/// Append a `flat_binder_object` of type `BINDER_TYPE_FD`.
///
/// `Parcel::writeFileDescriptor` writes the descriptor number in the `handle`
/// slot and, unlike `writeStrongBinder`, no stability word follows: the
/// stability is only written for binder objects.
fn push_flat_fd(parcel: &mut Vec<u8>, objects: &mut Vec<u64>, fd: RawFd) {
    objects.push(parcel.len() as u64);
    parcel.extend_from_slice(&BINDER_TYPE_FD.to_ne_bytes());
    parcel.extend_from_slice(&0x7fu32.to_ne_bytes()); // flags
    parcel.extend_from_slice(&(fd as u32).to_ne_bytes()); // handle = fd
    parcel.extend_from_slice(&0u32.to_ne_bytes()); // alignment
    parcel.extend_from_slice(&0u64.to_ne_bytes()); // cookie
}

/// Build the reply to an `IContentProvider` transaction.
///
/// There is no real provider behind the handle, so every method answers with the
/// "nothing here" value its client expects: a zero-row cursor for `query`, an
/// empty `Bundle` for `call`, `0` rows for the mutators. The shapes come from
/// `ContentProviderNative.onTransact` and `ContentProviderProxy` in this
/// framework.jar, not from guesswork.
fn provider_reply(code: u32, data: Option<&[u8]>, parcel: &mut Vec<u8>, objects: &mut Vec<u64>) {
    // The status word was already written by `make_reply`; writing another one
    // here shifts every result by four bytes, which turns the `call` reply's
    // `Bundle` into an empty one (`readBundle` reads the extra zero as a length)
    // and makes `Settings.NameValueCache.getStringsForPrefix` NPE on a null map.
    match code {
        ICP_QUERY => {
            // `[presence=1][BulkCursorDescriptor]`. The descriptor is
            //   writeStrongBinder(cursor)
            //   writeStringArray(columnNames)
            //   writeInt(wantsAllOnMoveCalls)
            //   writeInt(count)
            //   writeInt(window != null) [+ CursorWindow]
            // `BulkCursorToCursorAdaptor.getCount()` returns the descriptor's
            // `count` without touching the binder, so a count of 0 makes every
            // `moveToNext()` return false and the window is never requested.
            parcel.extend_from_slice(&1u32.to_ne_bytes());
            push_flat_handle(parcel, objects, handle_for("empty_cursor"));
            parcel.extend_from_slice(&0u32.to_ne_bytes()); // no column names
            parcel.extend_from_slice(&0u32.to_ne_bytes()); // wantsAllOnMoveCalls
            parcel.extend_from_slice(&0u32.to_ne_bytes()); // count
            parcel.extend_from_slice(&0u32.to_ne_bytes()); // no window
        }
        ICP_CALL => {
            // Android's getStringsForPrefix accepts null as an unsupported
            // list request and returns an empty map. Never reuse a scalar
            // Bundle here: getSerializable("value") expects a HashMap.
            if data.is_some_and(|data| parcel_contains_string16(data, "LIST_config")) {
                parcel.extend_from_slice(&(-1i32).to_ne_bytes());
                return;
            }
            // `Settings.NameValueCache` calls with the key as the `arg`
            // argument and reads `bundle.getString("value")`; `getStringsForPrefix`
            // asks for a prefix instead and expects a `HashMap` under "value".
            // Match the parcel against the keys the host installed rather than
            // trying to parse the AIDL argument order.
            let matched = data.and_then(|data| {
                let entries = SETTING_ENTRIES.lock().unwrap_or_else(|e| e.into_inner());
                entries
                    .iter()
                    .find(|(key, _)| parcel_contains_string16(data, key))
                    .map(|(_, bytes)| bytes.clone())
            });
            match matched {
                Some(bytes) if !bytes.is_empty() => parcel.extend_from_slice(&bytes),
                _ => {
                    let bundle = EMPTY_BUNDLE.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    if bundle.is_empty() {
                        parcel.extend_from_slice(&(-1i32).to_ne_bytes()); // null Bundle
                    } else {
                        parcel.extend_from_slice(&bundle);
                    }
                }
            }
        }
        ICP_GET_TYPE => parcel.extend_from_slice(&(-1i32).to_ne_bytes()), // null String
        ICP_INSERT | ICP_CANONICALIZE | ICP_UNCANONICALIZE => {
            // `Uri.writeToParcel(null)` is a presence int.
            parcel.extend_from_slice(&0u32.to_ne_bytes());
        }
        ICP_DELETE | ICP_UPDATE | ICP_BULK_INSERT | ICP_CHECK_URI_PERMISSION => {
            parcel.extend_from_slice(&0u32.to_ne_bytes());
        }
        ICP_OPEN_FILE | ICP_OPEN_ASSET_FILE | ICP_OPEN_TYPED_ASSET_FILE => {
            parcel.extend_from_slice(&0u32.to_ne_bytes()); // no fd
        }
        ICP_APPLY_BATCH | ICP_GET_STREAM_TYPES => {
            parcel.extend_from_slice(&(-1i32).to_ne_bytes()); // null array
        }
        ICP_CREATE_CANCELATION_SIGNAL => {
            push_flat_handle(parcel, objects, handle_for("empty_cursor"));
        }
        ICP_REFRESH => parcel.extend_from_slice(&(-1i32).to_ne_bytes()), // false
        _ => {}
    }
    if trace_enabled() {
        unsafe { diag(&format!("[binder]   -> provider code {code}\n")) };
    }
}

/// Build the reply to an `IWindowSession` transaction.
///
/// The session is a stub: it accepts the window and reports success, which is
/// what `ViewRootImpl.setView` needs to continue into its first traversal. The
/// surface it hands back is not a real one yet, so drawing stops at the
/// `SurfaceControl` boundary.
fn window_session_reply(code: u32, parcel: &mut Vec<u8>, objects: &mut Vec<u64>) {
    match window_session_method(code) {
        Some(WS_ADD_TO_DISPLAY_AS_USER) => {
            add_to_display_reply(parcel, objects);
        }
        Some(WS_GET_IN_TOUCH_MODE) | Some(WS_PERFORM_HAPTIC_FEEDBACK) => {
            parcel.extend_from_slice(&0u32.to_ne_bytes()); // false / no feedback
        }
        Some(WS_RELAYOUT) => {
            // Android 17 `IWindowSession.relayout`:
            //   `int relayout(..., out @nullable WindowRelayoutResult outRelayoutResult,
            //                  out SurfaceControl outSurface)`
            // The reply is `[result][presence][WindowRelayoutResult][SurfaceControl]`.
            parcel.extend_from_slice(&0u32.to_ne_bytes()); // result
            push_window_relayout_result(parcel);
            push_surface_control_reply(parcel, objects);
        }
        Some(WS_RELAYOUT2) => {
            // `int relayout2(..., in SurfaceControl surface,
            //                out @nullable WindowRelayoutResult)`
            parcel.extend_from_slice(&0u32.to_ne_bytes()); // result
            push_window_relayout_result(parcel);
        }
        Some(WS_OUT_OF_MEMORY) | Some(WS_CANCEL_DRAW) | Some(WS_MOVE_FOCUS_TO_ADJACENT_WINDOW) => {
            parcel.extend_from_slice(&0u32.to_ne_bytes()); // false
        }
        Some(WS_GET_WINDOW_ID) => {
            push_flat_handle(parcel, objects, handle_for("window_token"));
        }
        Some(WS_GRANT_INPUT_CHANNEL) => {
            // `@nullable InputChannel grantInputChannel(...)`
            push_input_channel_present(parcel, objects);
        }
        Some(WS_REMOVE)
        | Some(WS_SET_INSETS)
        | Some(WS_FINISH_DRAWING)
        | Some(WS_ON_RECTANGLE_ON_SCREEN_REQUESTED)
        | Some(WS_POKE_DRAW_LOCK)
        | Some(WS_UPDATE_POINTER_ICON)
        | Some(WS_UPDATE_TAP_EXCLUDE_REGION)
        | Some(WS_UPDATE_INPUT_CHANNEL)
        | Some(WS_CLEAR_TOUCHABLE_REGION) => {
            // Everything else is `oneway void` (or `void`): the status word that
            // `make_reply` already wrote is the whole reply.
        }
        _ => {}
    }
    if trace_enabled() {
        unsafe {
            diag(&format!(
                "[binder]   -> window session code {code} ({:?}) reply={} object(s)\n",
                window_session_method(code),
                objects.len()
            ))
        };
    }
}

/// The marshalled `WindowRelayoutResult` the host installed from the loaded
/// Android 17 framework. There is no hand-written fallback.
fn window_relayout_result() -> Option<Vec<u8>> {
    let bytes = WINDOW_RELAYOUT_RESULT
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if bytes.is_empty() {
        None
    } else {
        Some(bytes.clone())
    }
}

/// Emit an AIDL `out @nullable WindowRelayoutResult`.
fn push_window_relayout_result(parcel: &mut Vec<u8>) {
    match window_relayout_result() {
        Some(result) => {
            parcel.extend_from_slice(&1u32.to_ne_bytes()); // present
            parcel.extend_from_slice(&result);
        }
        None => {
            if trace_enabled() {
                unsafe { diag("[binder]   window reply: NO WindowRelayoutResult installed\n") };
            }
            parcel.extend_from_slice(&0u32.to_ne_bytes()); // absent
        }
    }
}

/// The `out SurfaceControl` half of `relayout`.
///
/// `SurfaceControl.readFromParcel` reads the Java-side header and then
/// `SurfaceControl::readFromParcel`'s
/// `[client][handle][layerId][transformHint][w][h][format]`.
fn push_surface_control_reply(parcel: &mut Vec<u8>, objects: &mut Vec<u64>) {
    parcel.extend_from_slice(&1u32.to_ne_bytes()); // present
    // `SurfaceControl.readFromParcel` starts with `readString8`.
    push_string8(parcel, "pengudroid");
    parcel.extend_from_slice(&1280i32.to_ne_bytes()); // width
    parcel.extend_from_slice(&720i32.to_ne_bytes()); // height
    parcel.extend_from_slice(&1i32.to_ne_bytes()); // hasNativeObject
    push_flat_handle(parcel, objects, handle_for("surface_composer_client"));
    push_flat_handle(parcel, objects, handle_for("surface_handle"));
    parcel.extend_from_slice(&1i32.to_ne_bytes()); // layerId
    parcel.extend_from_slice(&0u32.to_ne_bytes()); // transformHint
    parcel.extend_from_slice(&1280u32.to_ne_bytes()); // width
    parcel.extend_from_slice(&720u32.to_ne_bytes()); // height
    parcel.extend_from_slice(&1u32.to_ne_bytes()); // format: RGBA_8888
}

/// Write one `android.view.InputChannel` the way Android 17's native code does.
///
/// Source of truth, no guessing: `libinput.so`'s
/// `android::os::InputChannelCore::writeToParcel` (disassembled) and
/// `android_view_InputChannel_nativeWriteToParcel` in `libandroid_runtime.so`.
///
/// Android 17's AIDL C++ backend prefixes every parcelable with its own byte
/// length, so that a reader can skip fields a newer writer added:
///
/// ```text
/// int32      isInitialized = 1     written by nativeWriteToParcel
/// int32      coreSize              bytes from coreSize through the end
/// String16   name                  writeUtf8AsUtf16
/// int32      parcelablePresent=1   `Parcel::writeData(const Parcelable&)`
///                                  (`kNonNullParcelableFlag`)
/// int32      hasComm = 0           ParcelFileDescriptor (writeParcelable)
/// flat fd                          BINDER_TYPE_FD, flags=0, cookie=1
/// flat token                       writeStrongBinder(nullptr): binder == 0,
///                                  so no offset-table entry is recorded
/// ```
///
/// Missing `coreSize` was the whole reason the client logged
/// `channel '' ... socket (-1)`: the reader treats the next word as a size and
/// bails out with `BAD_VALUE` when it is smaller than 4. The
/// `kNonNullParcelableFlag` word is the second half of the same story: without
/// it `readData(Parcelable*)` returns `UNEXPECTED_NULL` and the descriptor is
/// never read.
fn push_input_channel_core(
    parcel: &mut Vec<u8>,
    objects: &mut Vec<u64>,
    name: &str,
    fd: RawFd,
) {
    parcel.extend_from_slice(&1i32.to_ne_bytes()); // isInitialized
    let size_at = parcel.len();
    parcel.extend_from_slice(&0i32.to_ne_bytes()); // coreSize placeholder
    parcel.extend_from_slice(&string16(name));
    parcel.extend_from_slice(&1i32.to_ne_bytes()); // kNonNullParcelableFlag
    parcel.extend_from_slice(&0i32.to_ne_bytes()); // ParcelFileDescriptor.hasComm
    objects.push(parcel.len() as u64);
    parcel.extend_from_slice(&BINDER_TYPE_FD.to_ne_bytes());
    parcel.extend_from_slice(&0u32.to_ne_bytes()); // flags
    parcel.extend_from_slice(&(fd as u32).to_ne_bytes()); // handle = fd
    parcel.extend_from_slice(&0u32.to_ne_bytes()); // alignment
    parcel.extend_from_slice(&1u64.to_ne_bytes()); // cookie: takeOwnership
    // `IBinder token` is null: the client only hands it back to a window manager,
    // which PenguDroid does not have. `writeStrongBinder` emits the object and
    // then its stability word (`finishFlattenBinder`), so the null token is 28
    // bytes, not 24.
    parcel.extend_from_slice(&BINDER_TYPE_BINDER.to_ne_bytes());
    parcel.extend_from_slice(&0u32.to_ne_bytes()); // flags
    parcel.extend_from_slice(&0u32.to_ne_bytes()); // binder == 0
    parcel.extend_from_slice(&0u32.to_ne_bytes()); // alignment
    parcel.extend_from_slice(&0u64.to_ne_bytes()); // cookie
    parcel.extend_from_slice(&0u32.to_ne_bytes()); // stability repr (null binder)
    let size = (parcel.len() - size_at) as u32;
    parcel[size_at..size_at + 4].copy_from_slice(&size.to_ne_bytes());
}

/// Emit `[presence][InputChannel]` for an AIDL parcelable out value.
fn push_input_channel_present(parcel: &mut Vec<u8>, objects: &mut Vec<u64>) {
    match input_channel_pair() {
        Some((client, server)) if std::env::var_os("PENGUDROID_NO_INPUT_CHANNEL").is_none() => {
            // `F_GETFD` proves the descriptor is open at reply-build time; if it
            // were -1 here, the client would fail the same way for a different
            // reason and we would be looking in the wrong place.
            let open = unsafe { libc::fcntl(client, libc::F_GETFD) };
            if trace_enabled() {
                unsafe { diag(&format!("[binder]   input channel client={client} server={server} F_GETFD={open}\n")) };
            }
            INPUT_SERVER_FDS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(server);
            parcel.extend_from_slice(&1u32.to_ne_bytes()); // present
            push_input_channel_core(parcel, objects, "pengudroid", client);
        }
        Some((client, server)) => {
            // Keep both ends alive so the socketpair does not become an EOF
            // storm, but tell the client there is no channel.
            let mut fds = INPUT_SERVER_FDS.lock().unwrap_or_else(|e| e.into_inner());
            fds.push(server);
            fds.push(client);
            parcel.extend_from_slice(&0u32.to_ne_bytes()); // absent
        }
        None => {
            parcel.extend_from_slice(&0u32.to_ne_bytes()); // absent
        }
    }
}

/// Build the reply to `IWindowSession.addToDisplayAsUser`.
///
/// The status word is already in `parcel`. Android 17 declares:
///
/// ```text
/// int result                              ADD_OKAY
/// out InputChannel outInputChannel        presence + InputChannel
/// out WindowRelayoutResult result         presence + WindowRelayoutResult
/// ```
///
/// The `InputChannel` has to be built here rather than in the host because it
/// carries a file descriptor, and `Parcel.marshall()` cannot express one.
fn add_to_display_reply(parcel: &mut Vec<u8>, objects: &mut Vec<u64>) {
    let start = parcel.len();
    parcel.extend_from_slice(&0u32.to_ne_bytes()); // result: ADD_OKAY
    push_input_channel_present(parcel, objects);
    push_window_relayout_result(parcel);
    if trace_enabled() {
        let words: Vec<String> = parcel[start..]
            .chunks_exact(4)
            .take(12)
            .map(|w| format!("{}", u32::from_ne_bytes([w[0], w[1], w[2], w[3]])))
            .collect();
        unsafe {
            diag(&format!(
                "[binder]   addToDisplay reply {} byte(s) head=[{}] fds={:?}\n",
                parcel.len() - start,
                words.join(", "),
                objects
            ))
        };
    }
}

/// `Parcel::writeString8`: a byte count, the bytes, then padding to 4.
fn push_string8(parcel: &mut Vec<u8>, s: &str) {
    parcel.extend_from_slice(&(s.len() as u32).to_ne_bytes());
    parcel.extend_from_slice(s.as_bytes());
    while parcel.len() % 4 != 0 {
        parcel.push(0);
    }
}

/// Create the socket pair behind an `InputChannel`.
fn input_channel_pair() -> Option<(RawFd, RawFd)> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: `fds` is a valid two-element array for `socketpair` to fill.
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0, fds.as_mut_ptr()) } != 0 {
        return None;
    }
    Some((fds[0], fds[1]))
}

fn check_api37_sdk(sdk: Option<&str>) -> Result<(), String> {
    if sdk != Some("37") {
        return Err(format!(
            "Android 17 / API 37 x86_64 required: ro.build.version.sdk must be 37, got {sdk:?}"
        ));
    }
    Ok(())
}

fn require_api37() -> Result<(), String> {
    check_api37_sdk(crate::get_prop("ro.build.version.sdk").as_deref())
}

/// The measured API-37 event layout; only the preferred timeline is populated.
fn vsync_event(vsync_time: i64, frame: u32) -> [u8; DISPLAY_EVENT_SIZE] {
    let mut event = [0u8; DISPLAY_EVENT_SIZE];
    event[0..4].copy_from_slice(&DISPLAY_EVENT_VSYNC.to_ne_bytes());
    // displayId stays 0 (the hosted display).
    event[16..24].copy_from_slice(&vsync_time.to_ne_bytes());
    event[24..28].copy_from_slice(&frame.to_ne_bytes());
    event[32..40].copy_from_slice(&VSYNC_FRAME_INTERVAL_NS.to_ne_bytes());
    event[40..44].copy_from_slice(&0u32.to_ne_bytes()); // preferredFrameTimelineIndex
    event[44..48].copy_from_slice(&1u32.to_ne_bytes()); // frameTimelinesLength
    // numberQueuedBuffers and alignment precede FrameTimeline[7] at offset 56.
    let expected = vsync_time + VSYNC_FRAME_INTERVAL_NS;
    event[56..64].copy_from_slice(&(frame as i64).to_ne_bytes());
    event[64..72].copy_from_slice(&expected.to_ne_bytes());
    event[72..80].copy_from_slice(&expected.to_ne_bytes());
    event
}

/// Deliver one vsync event on every open `BitTube`.
///
/// The client's `DisplayEventReceiver::getEvents` does a `recv` on the socket
/// and divides the byte count by `sizeof(Event)`, so each `send` has to be
/// exactly one whole event.
fn send_vsync() -> Result<(), String> {
    require_api37()?;

    // `Header.timestamp` is the *vsync* time: the moment the frame started. It
    // has to be on `CLOCK_MONOTONIC` — the clock `System.nanoTime()` reads — and
    // it must be the real time, not a counter stepped by one interval per call.
    // `send_vsync` runs both from the 60 Hz thread and on every
    // `requestNextVsync`, so stepping it made the timebase race ahead of the
    // wall clock; `Choreographer` then logged
    //
    //     Frame time is 29787 ms in the future!  Check that graphics HAL is
    //     generating vsync timestamps using the correct timebase.
    //
    // `CLOCK_MONOTONIC` is already monotonic, so reading it is all that is needed.
    static FRAME: AtomicU32 = AtomicU32::new(0);

    let vsync_time = {
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: `ts` is a valid `timespec` for `clock_gettime` to fill.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        (ts.tv_sec as i64) * 1_000_000_000 + ts.tv_nsec as i64
    };

    // `send_vsync` is called from the 60 Hz thread *and* on every
    // `requestNextVsync`. Emitting both puts two pulses inside one frame
    // interval, and the client then logs "dispatcher … ~ last event processed
    // while scheduling was for …" for the surplus one. Half a frame interval is
    // short enough that a request is still answered promptly and long enough
    // that a request arriving just after a tick does not produce a second pulse.
    {
        static LAST_SEND: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
        let previous = LAST_SEND.load(Ordering::Relaxed);
        if previous != 0 && vsync_time - previous < VSYNC_FRAME_INTERVAL_NS / 2 {
            return Ok(());
        }
        LAST_SEND.store(vsync_time, Ordering::Relaxed);
    }
    let frame = FRAME.fetch_add(1, Ordering::Relaxed);

    // Real timestamps and a populated preferred timeline keep Choreographer
    // from treating every frame as late or in the future.
    let event = vsync_event(vsync_time, frame);

    let fds = VSYNC_SEND_FDS.lock().unwrap_or_else(|e| e.into_inner());
    let mut dropped = 0usize;
    for fd in fds.iter() {
        // SAFETY: `event` is a valid buffer and `fd` is a socket we own.
        // `MSG_DONTWAIT` keeps a client that stopped reading from blocking the
        // frame clock; a full socket just drops the pulse.
        let rc = unsafe {
            libc::send(
                *fd,
                event.as_ptr() as *const c_void,
                event.len(),
                libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
            )
        };
        if rc < 0 {
            dropped += 1;
        }
    }
    // One line per second: vsync fires 60 times a second, and every one of them
    // in the trace buries the transactions that matter.
    if trace_enabled() && frame % 60 == 0 {
        unsafe {
            diag(&format!(
                "[binder]   -> vsync frame {frame} ts={vsync_time} dropped={dropped}\n"
            ))
        };
    }
    Ok(())
}

/// Start the 60 Hz frame clock, once per process.
///
/// A real compositor emits vsync continuously; the app's `Choreographer` only
/// asks for the *next* pulse, so answering just that one request leaves the
/// frame loop stalled as soon as a traversal is skipped. A steady clock is also
/// what Roblox's engine expects once it starts drawing.
fn start_frame_clock() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        std::thread::spawn(|| loop {
            std::thread::sleep(std::time::Duration::from_micros(16_666));
            if !VSYNC_SEND_FDS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
            {
                if let Err(error) = send_vsync() {
                    unsafe { diag(&format!("[binder] frame clock stopped: {error}\n")) };
                    break;
                }
            }
        });
    });
}

/// Create the socket pair behind a `BitTube`.
///
/// `BitTube` is an `AF_UNIX` `SOCK_SEQPACKET` pair; the client keeps the receive
/// end and the broker keeps the send end so it can deliver vsync events later.
fn vsync_channel() -> Option<(RawFd, RawFd)> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: `fds` is a valid two-element array for `socketpair` to fill.
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0, fds.as_mut_ptr()) } != 0 {
        return None;
    }
    // The client takes ownership of both descriptors it is handed and closes
    // them when the `BitTube` dies. Handing it the same numbers we keep would
    // make its `close()` kill our send end, so the broker keeps a duplicate.
    // (A real binder driver dups every descriptor it transfers; this is that
    // dup, done by hand.)
    let ours = unsafe { libc::dup(fds[1]) };
    if ours < 0 {
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
        return None;
    }
    // The client is handed both ends and closes them when the `BitTube` dies, so
    // the broker keeps its own duplicates. Both are kept and both are written:
    // which end `BitTube::readFromParcel` treats as the receive side is not
    // documented, and writing to the wrong one silently stalls the frame clock.
    let ours0 = unsafe { libc::dup(fds[0]) };
    {
        let mut fds_lock = VSYNC_SEND_FDS.lock().unwrap_or_else(|e| e.into_inner());
        fds_lock.push(ours);
        if ours0 >= 0 {
            fds_lock.push(ours0);
        }
    }
    Some((fds[0], fds[1]))
}

/// Whether `data` contains `s` encoded as UTF-16LE (a `String16` payload).
fn parcel_contains_utf16(data: &[u8], s: &str) -> bool {
    let needle: Vec<u8> = s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    !needle.is_empty() && data.windows(needle.len()).any(|w| w == needle.as_slice())
}

/// Match a whole, aligned Parcel String16 field, not a substring of a key.
fn parcel_contains_string16(data: &[u8], value: &str) -> bool {
    let units: Vec<u16> = value.encode_utf16().collect();
    let mut needle = (units.len() as u32).to_le_bytes().to_vec();
    needle.extend(units.iter().flat_map(|u| u.to_le_bytes()));
    needle.extend_from_slice(&[0, 0]);
    (0..data.len()).step_by(4).any(|offset| {
        data.get(offset..offset + needle.len()) == Some(needle.as_slice())
    })
}

/// Reverse lookup of a handle, for diagnostics and service aliases.
fn name_for_handle(handle: u32) -> Option<String> {
    let map = SERVICE_HANDLES.lock().unwrap_or_else(|e| e.into_inner());
    map.iter().find(|(_, h)| **h == handle).map(|(n, _)| n.clone())
}

/// Whether `handle` is the SurfaceFlinger service.
///
/// The name matters: hwui asks the service manager for `SurfaceFlingerAIDL`
/// while other clients ask for `SurfaceFlinger`, and `handle_for` assigns a
/// *different* handle per name. Comparing against one literal name silently
/// missed every reply for the other, which is why answering
/// `getPhysicalDisplayIds` had no effect.
fn is_surface_flinger(handle: u32) -> bool {
    matches!(
        name_for_handle(handle).as_deref(),
        Some("SurfaceFlinger") | Some("SurfaceFlingerAIDL")
    )
}

/// The AIDL descriptor of a hosted interface, for the transport's
/// `INTERFACE_TRANSACTION`.
///
/// Only interfaces this broker actually implements are listed: answering with a
/// descriptor for a service that is not hosted would invite a caller to speak a
/// protocol nothing answers. Every string here is already the descriptor this
/// file validates incoming parcels against, so it is measured, not invented.
fn hosted_interface_descriptor(handle: u32) -> Option<&'static str> {
    match name_for_handle(handle)?.as_str() {
        "SurfaceFlinger" | "SurfaceFlingerAIDL" => Some("android.gui.ISurfaceComposer"),
        "surface_composer_client" => Some("android.gui.ISurfaceComposerClient"),
        "display_event_connection" => Some("android.gui.IDisplayEventConnection"),
        "pengudroid.activity_host" => Some(ACTIVITY_HOST_DESCRIPTOR),
        _ => None,
    }
}

/// Every plausible `string16` in a parcel, as `(offset, text)`.
///
/// A request parcel is not a flat list of strings: `bindService`/`startService`
/// put an `IApplicationThread` binder object between the interface descriptor and
/// the `Intent`, so walking field by field stops at the object. Trying a
/// `string16` parse at every 2-byte offset and keeping the plausible ones
/// sidesteps the layout entirely.
fn string16_candidates(data: &[u8]) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 6 <= data.len() {
        let len = u32::from_ne_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        if (1..=256).contains(&len) && pos + 4 + len * 2 <= data.len() {
            let text = decode_utf16le(&data[pos + 4..pos + 4 + len * 2]);
            let plausible = !text.is_empty()
                && text
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '/' | '_' | '-' | ':'));
            if plausible {
                out.push((pos, text));
                // The payload plus the NUL terminator, aligned to 4 bytes.
                pos += ((len + 1) * 2 + 3) & !3;
                continue;
            }
        }
        pos += 2;
    }
    out
}

/// Component name carried by a `startService`/`bindService` request.
///
/// Uses the first plausible package-like string followed by a class-like string
/// that lives under it. API-37 Intent writes its ComponentName after the seven
/// prefix fields, with the component's package as a String16. Its offset is
/// returned so intent_start can walk back to the Intent for unmarshalling.
fn service_component(data: &[u8]) -> Option<(String, String, usize)> {
    let candidates = string16_candidates(data);
    for (i, (offset, package)) in candidates.iter().enumerate() {
        if !package.contains('.') || package.starts_with("android.") {
            continue;
        }
        let prefix = format!("{package}.");
        for (_, class) in candidates.iter().skip(i + 1) {
            if class.starts_with(&prefix) {
                return Some((package.clone(), class.clone(), *offset));
            }
        }
    }
    None
}

/// Find the `string16` that ends exactly at `end`, returning its start.
///
/// Parcel strings are either a 4-byte `-1` (null) or `[u32 len][utf16][nul]`
/// padded to 4 bytes, and every field is 4-byte aligned, so stepping back in
/// 4-byte steps finds the length word unambiguously.
fn string16_start_ending_at(data: &[u8], end: usize) -> Option<usize> {
    if end < 4 || end > data.len() {
        return None;
    }
    let word = u32::from_ne_bytes([data[end - 4], data[end - 3], data[end - 2], data[end - 1]]);
    if word == u32::MAX {
        return Some(end - 4);
    }
    let lowest = end.saturating_sub(4 + 2 * 1024 + 8);
    let mut start = (end - 4) & !3;
    loop {
        if start + 4 <= end {
            let len = u32::from_ne_bytes([
                data[start],
                data[start + 1],
                data[start + 2],
                data[start + 3],
            ]) as usize;
            if (1..=1024).contains(&len) {
                let payload_end = start + 4 + len * 2;
                if (payload_end + 2 + 3) & !3 == end && payload_end + 2 <= data.len() {
                    return Some(start);
                }
            }
        }
        if start < lowest || start < 4 {
            return None;
        }
        start -= 4;
    }
}

/// Find the `string8` that ends exactly at `end`, returning its start.
///
/// Walk the API-37 Intent's String8 and nullable Uri fields backwards. Null
/// strings use -1 and a null Uri uses 0. All field boundaries are 4-byte aligned,
/// so stepping back in 4-byte steps locates the length word.
fn string8_start_ending_at(data: &[u8], end: usize) -> Option<usize> {
    if end < 4 || end > data.len() {
        return None;
    }
    let word = u32::from_ne_bytes([data[end - 4], data[end - 3], data[end - 2], data[end - 1]]);
    // `-1` is a null String8, never a valid length word.
    if word == u32::MAX {
        return Some(end - 4);
    }
    // A trailing zero word is either a null Uri (a bare `0` int) or the
    // terminator/padding of a non-null String8 whose length is a multiple of
    // four. Try the real length scan first, and fall back to the null field
    // only when no payload fits.
    let trailing_zero = word == 0;
    let lowest = end.saturating_sub(4 + 1024 + 8);
    let mut start = (end - 4) & !3;
    loop {
        if start + 4 <= end {
            let len = u32::from_ne_bytes([
                data[start],
                data[start + 1],
                data[start + 2],
                data[start + 3],
            ]) as usize;
            // `Parcel::writeString8` writes the length, the bytes, one NUL, and
            // then pads the whole payload to four bytes. The terminator is part
            // of the padded region, so the next field starts at
            // `start + 4 + align4(len + 1)` (not align4(len) + 5).
            if (1..=1024).contains(&len) && start + 4 + len <= data.len() {
                if start + 4 + ((len + 1 + 3) & !3) == end {
                    return Some(start);
                }
            }
        }
        if start < 4 || start < lowest {
            break;
        }
        start -= 4;
    }
    trailing_zero.then(|| end - 4)
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

/// Read one `Parcel.writeString16` field.
///
/// Returns the decoded string (`None` for a null) and the offset of the next
/// field. This is the forward counterpart of `string16_start_ending_at` and is
/// used to validate a candidate `Intent` instead of trusting a backward walk.
fn read_string16(data: &[u8], at: usize) -> Option<(Option<String>, usize)> {
    let len = u32_at(data, at)?;
    if len == u32::MAX {
        return Some((None, at + 4));
    }
    let len = usize::try_from(len).ok()?;
    if len > 4096 {
        return None;
    }
    let payload = data.get(at + 4..at + 4 + len * 2)?;
    let next = at + 4 + ((len * 2 + 2 + 3) & !3);
    if next > data.len() {
        return None;
    }
    Some((Some(decode_utf16le(payload)), next))
}

/// Read one `Parcel.writeString8` field.
fn read_string8(data: &[u8], at: usize) -> Option<(Option<String>, usize)> {
    let len = u32_at(data, at)?;
    if len == u32::MAX {
        return Some((None, at + 4));
    }
    let len = usize::try_from(len).ok()?;
    if len > 4096 {
        return None;
    }
    let payload = data.get(at + 4..at + 4 + len)?;
    let next = at + 4 + ((len + 1 + 3) & !3);
    if next > data.len() {
        return None;
    }
    Some((Some(String::from_utf8_lossy(payload).into_owned()), next))
}

/// Walk one `Intent.mData` field: either a null type id or `[type id][String8]`.
fn intent_uri_end(data: &[u8], at: usize) -> Option<usize> {
    match u32_at(data, at)? {
        0 | u32::MAX => Some(at + 4),
        1..=3 => read_string8(data, at + 4).map(|(_, next)| next),
        _ => None,
    }
}

/// Parse the API-37 `Intent` prefix at `at` and return its component.
///
/// `Intent.writeToParcel` order is action, data, type, identifier, flags,
/// extendedFlags, package, then `ComponentName` as two String16 fields. A
/// missing component returns `None`; no other Intent layout is accepted.
fn intent_component_at(data: &[u8], at: usize) -> Option<(String, String, usize)> {
    let (_, mut next) = read_string8(data, at)?;
    next = intent_uri_end(data, next)?;
    let (_, after_type) = read_string8(data, next)?;
    let (_, after_identifier) = read_string8(data, after_type)?;
    let after_flags = after_identifier.checked_add(4)?;
    let after_extended_flags = after_flags.checked_add(4)?;
    // The `mPackage` String8 sits after both int flags fields.
    let (_, component) = read_string8(data, after_extended_flags)?;
    let (package_name, after_package) = read_string16(data, component)?;
    let package_name = package_name?;
    let (class_name, _) = read_string16(data, after_package)?;
    let class_name = class_name?;
    if package_name.is_empty() || class_name.is_empty() {
        return None;
    }
    Some((package_name, class_name, component))
}

/// Locate the `Intent` argument of an API-37 activity-manager transaction.
///
/// The API-37 interface token follows `[strictMode][workSourceUid]["TSYS"]`.
/// `startActivity` then carries the caller binder, the calling package, an
/// optional feature id (task-manager variant only), and a typed `Intent`.
/// Parsing forward keeps `callingPackage` from being mistaken for the
/// component package. Only the loaded platform's wire format is accepted.
fn activity_intent(data: &[u8]) -> Option<(usize, String, String)> {
    if data.get(8..12)? != b"TSYS" { return None; }
    let (descriptor, mut at) = read_string16(data, 12)?;
    let descriptor = descriptor?;
    let task_manager = match descriptor.as_str() {
        "android.app.IActivityTaskManager" => true,
        "android.app.IActivityManager" => false,
        _ => return None,
    };
    let ty = u32_at(data, at)?;
    if ty != BINDER_TYPE_BINDER && ty != BINDER_TYPE_HANDLE {
        return None;
    }
    // `flat_binder_object` is type/flags/binder-or-handle/cookie (24 bytes) and
    // then the stability word libbinder writes in `finishFlattenBinder`.
    at = at.checked_add(24)?;
    if !matches!(u32_at(data, at)?, 3 | 0xc | 0x3f) {
        return None;
    }
    at += 4;
    let (_, next) = read_string16(data, at)?;
    at = next;
    if task_manager {
        let (_, next) = read_string16(data, at)?;
        at = next;
    }
    // AIDL's writeTypedObject adds a presence word before Intent.writeToParcel.
    // Skipping it is mandatory: treating `1` as an action string length can
    // make a hand-written fixture pass while every real start is rejected.
    if u32_at(data, at)? != 1 { return None; }
    at += 4;
    let (package, class, _) = intent_component_at(data, at)?;
    Some((at, package, class))
}

/// Decode only finishes for a registered local token, without result Intent or
/// task-wide removal. Unsupported finishes return false instead of closing an
/// unrelated top activity.
fn activity_finish_token(data: &[u8], descriptor: &str) -> Option<(u64, i32)> {
    if first_string16(data).as_deref() != Some(descriptor) { return None; }
    let (_, at) = read_string16(data, 12)?;
    let (weak, cookie, offset) = find_local_binder(data)?;
    if offset != at || u32_at(data, at + 32)? != 0 || u32_at(data, at + 36)? != 0 {
        return None;
    }
    let result = u32_at(data, at + 28)? as i32;
    let token = *ACTIVITY_TOKENS.lock().unwrap_or_else(|e| e.into_inner()).get(&(weak, cookie))?;
    Some((token, result))
}

/// Offset of the `Intent` that contains the component package at
/// `package_offset`.
///
/// API-37 Intent.writeToParcel writes seven prefix fields: `action` (string8),
/// `data` (nullable Uri), `type` (string8), `identifier` (string8), `flags` (int),
/// `extendedFlags` (int), and `package` (string8), then the ComponentName as two
/// String16s. The package this
/// function is given is the second of those (the component's), so walking back
/// over the seven fields before it lands on `action` — the first byte of the
/// `Intent`, and therefore the offset `Parcel.unmarshall` wants.
///
/// Only this API-37 layout is accepted; no older Intent layout is retried.
fn intent_start(data: &[u8], package_offset: usize) -> Option<usize> {
    let package = string8_start_ending_at(data, package_offset)?;
    let extended_flags = package.checked_sub(4)?;
    let flags = extended_flags.checked_sub(4)?;
    if flags == 0 {
        return None;
    }
    let identifier = string8_start_ending_at(data, flags)?;
    let ty = string8_start_ending_at(data, identifier)?;
    let uri = string8_start_ending_at(data, ty)?;
    string8_start_ending_at(data, uri)
}

/// Decode the AIDL interface token of a request parcel.
///
/// libbinder prefixes the token with a 12-byte header (`[u32][i32 -1]["TSYS"]`)
/// before the `String16`, so the string does not start at offset 0.
fn first_string16(data: &[u8]) -> Option<String> {
    if data.get(8..12)? != b"TSYS" { return None; }
    let len = u32::from_ne_bytes(data.get(12..16)?.try_into().ok()?) as usize;
    if len == 0 || len > 512 { return None; }
    let end = 16 + len * 2;
    if data.get(end..end + 2)? != [0, 0] { return None; }
    Some(decode_utf16le(data.get(16..end)?))
}

/// Whether a reply starts with an AIDL `Status` word. SurfaceComposer and its
/// client are API-37 generated AIDL; neither has a handwritten reply variant.
fn reply_has_status(handle: u32, code: u32) -> bool {

    if handle == handle_for("media.resource_manager") && code == INTERFACE_TRANSACTION {
        // `BpBinder::getInterfaceDescriptor` (`libbinder.so` `0x5ef60`) calls
        // `transact` and then `Parcel::readString16()` straight away, with no
        // status word in front of the descriptor.
        return false;
    }
    true
}

/// Generated AIDL parcelables include a presence word and a self-size prefix.
fn push_sized_parcelable(parcel: &mut Vec<u8>, fields: impl FnOnce(&mut Vec<u8>)) {
    parcel.extend_from_slice(&1i32.to_ne_bytes());
    let start = parcel.len();
    parcel.extend_from_slice(&0i32.to_ne_bytes());
    fields(parcel);
    let size = (parcel.len() - start) as i32;
    parcel[start..start + 4].copy_from_slice(&size.to_ne_bytes());
}

fn push_static_display_info(parcel: &mut Vec<u8>) {
    // API-37 gui::StaticDisplayInfo::readFromParcel (libgui +0xe56f0).
    push_sized_parcelable(parcel, |parcel| {
        for word in [0u32, 0, 2.0f32.to_bits(), 0, 0, 0, 0] {
            parcel.extend_from_slice(&word.to_ne_bytes());
        }
    });
}

fn push_dynamic_display_info(parcel: &mut Vec<u8>) {
    // API-37 gui::DynamicDisplayInfo and DisplayMode readers (+0xe6330,
    // +0x1908b0). This is a synthetic SDR display, not host HDR discovery.
    push_sized_parcelable(parcel, |parcel| {
        parcel.extend_from_slice(&1i32.to_ne_bytes()); // one supported mode
        push_sized_parcelable(parcel, |parcel| {
            parcel.extend_from_slice(&1i32.to_ne_bytes()); // mode ID
            push_sized_parcelable(parcel, |parcel| {
                parcel.extend_from_slice(&720i32.to_ne_bytes());
                parcel.extend_from_slice(&1280i32.to_ne_bytes());
            });
            for word in [320.0f32.to_bits(), 320.0f32.to_bits(), 0,
                60.0f32.to_bits(), 60.0f32.to_bits()] {
                parcel.extend_from_slice(&word.to_ne_bytes());
            }
            for value in [0i64, 16_666_667, 16_666_667] {
                parcel.extend_from_slice(&value.to_ne_bytes());
            }
            parcel.extend_from_slice(&0i32.to_ne_bytes()); // mode group
            parcel.extend_from_slice(&0i32.to_ne_bytes()); // trailing mode field
        });
        parcel.extend_from_slice(&1i32.to_ne_bytes()); // active mode ID
        parcel.extend_from_slice(&60.0f32.to_ne_bytes()); // render frame rate
        for value in [1i32, 0, 0] { // one native color mode, active native mode
            parcel.extend_from_slice(&value.to_ne_bytes());
        }
        // The generated reader accepts a body ending at activeColorMode.
        // Later capabilities retain their constructed defaults; do not invent
        // null encodings for its non-null HDR/refresh-rate parcelables.
    });
}

/// Build the reply parcel for a transaction.
///
/// AIDL replies start with a status word; unavailable contracts in the
/// SurfaceComposer, display-event, service-manager, Codec2, media, focus, user,
/// thermal and input paths return a transport UNKNOWN_TRANSACTION instead of an
/// empty success parcel.
unsafe fn make_reply(tr: &TransactionData, pid: i32) -> Reply {
    if tr.code == 0 {
        return Reply::unknown_transaction(tr, "zero is not a Binder transaction code");
    }
    // Every hosted interface answers the transport's interface-descriptor
    // transaction with its own descriptor and no status word: `Binder.onTransact`
    // and `BpBinder::getInterfaceDescriptor` both read/write it that way. Without
    // this branch a caller that probes an interface (measured: `code=1598968902`
    // against `SurfaceFlingerAIDL` in the BBR2 run) is told the transaction is
    // unsupported instead of being told which interface it reached.
    if tr.code == IBINDER_INTERFACE_TRANSACTION {
        if let Some(descriptor) = hosted_interface_descriptor(tr.target_handle) {
            let parcel = string16(descriptor);
            return Reply { parcel, objects: Vec::new(), flags: 0 };
        }
    }
    let mut parcel = Vec::new();
    let mut objects = Vec::new();
    if is_hidl_request(tr) && (tr.target_handle == 0
        || name_for_handle(tr.target_handle).as_deref() == Some(HIDL_MANAGER_HANDLE)) {
        parcel.extend_from_slice(&0i32.to_ne_bytes()); // HIDL Status::EX_NONE
        if let Err(reason) = hidl_service_manager_reply(tr, &mut parcel, &mut objects) {
            return Reply::unknown_transaction(tr, reason);
        }
        return Reply { parcel, objects, flags: 0 };
    }
    if is_surface_flinger(tr.target_handle) {
        if let Err(error) = require_api37() {
            return Reply::unknown_transaction(tr, &error);
        }
        let data = if tr.data_size > 0 && tr.data_ptr != 0 {
            std::slice::from_raw_parts(tr.data_ptr as *const u8, tr.data_size as usize)
        } else { &[] };
        if first_string16(data).as_deref() == Some("android.ui.ISurfaceComposer") {
            // The C++ interface writes no AIDL status word and no payload for the
            // tags it accepts, so the reply body stays empty; libbinder carries the
            // `status_t` in the reply header.
            //
            // `SET_TRANSACTION_STATE` is a layer transaction for a compositor this
            // host does not have: it owns no layers, so "applied" is the honest
            // result and every update stays unobserved. It is counted and traced
            // instead of being silently swallowed.
            return match tr.code {
                ISC_CPP_BOOT_FINISHED | ISC_CPP_SET_TRANSACTION_STATE => {
                    let seen = CPP_TRANSACTION_STATES.fetch_add(1, Ordering::Relaxed) + 1;
                    if trace_enabled() {
                        diag(&format!(
                            "[binder]   -> C++ ISurfaceComposer tag={} accepted ({seen} transaction(s) dropped: no hosted layers)\n",
                            tr.code
                        ));
                    }
                    Reply { parcel: Vec::new(), objects: Vec::new(), flags: 0 }
                }
                _ => Reply::unknown_transaction(
                    tr,
                    "unimplemented API-37 android.ui.ISurfaceComposer tag",
                ),
            };
        }
        if first_string16(data).as_deref() != Some("android.gui.ISurfaceComposer") {
            // The rejection is only useful with the bytes that caused it: this
            // handle is reached by more than one interface, and a silent mismatch
            // cannot tell "wrong token" from "different method encoding".
            if trace_enabled() {
                let mut hex = String::new();
                for byte in data.iter().take(192) {
                    hex.push_str(&format!("{byte:02x}"));
                }
                diag(&format!(
                    "[binder] ISurfaceComposer code={} token mismatch: {} byte(s), first16={:?}, hex={hex}\n",
                    tr.code,
                    data.len(),
                    first_string16(data)
                ));
            }
            return Reply::unknown_transaction(tr, "API-37 android.gui.ISurfaceComposer token required");
        }
        parcel.extend_from_slice(&0u32.to_ne_bytes()); // AIDL Status
        match tr.code {
            ISC_CREATE_DISPLAY_EVENT_CONNECTION => push_flat_handle(&mut parcel, &mut objects, handle_for("display_event_connection")),
            ISC_CREATE_CONNECTION => push_flat_handle(&mut parcel, &mut objects, handle_for("surface_composer_client")),
            ISC_GET_PHYSICAL_DISPLAY_IDS => { // one hosted display
                parcel.extend_from_slice(&1i32.to_ne_bytes());
                parcel.extend_from_slice(&0i64.to_ne_bytes());
            }
            ISC_GET_PHYSICAL_DISPLAY_TOKEN => push_flat_handle(&mut parcel, &mut objects, handle_for("display_token")),
            ISC_GET_STATIC_DISPLAY_INFO => push_static_display_info(&mut parcel),
            ISC_GET_DYNAMIC_DISPLAY_INFO => push_dynamic_display_info(&mut parcel),
            ISC_GET_COMPOSITION_PREFERENCE => push_sized_parcelable(&mut parcel, |parcel| {
                for value in [0i32, 1, 0, 1] {
                    parcel.extend_from_slice(&value.to_ne_bytes());
                }
            }),
            // Measured BpSurfaceComposer::getMaxAcquiredBufferCount in libgui.
            // BLASTBufferQueue::initialize requires a nonzero consumer count.
            ISC_GET_MAX_ACQUIRED_BUFFER_COUNT => parcel.extend_from_slice(&2i32.to_ne_bytes()),
            _ => return Reply::unknown_transaction(tr, "unimplemented API-37 ISurfaceComposer method"),
        }
        return Reply { parcel, objects, flags: 0 };
    }
    if name_for_handle(tr.target_handle).as_deref() == Some("pengudroid.activity_host") {
        let data = if tr.data_size != 0 && tr.data_ptr != 0 {
            std::slice::from_raw_parts(tr.data_ptr as *const u8, tr.data_size as usize)
        } else { &[] };
        if tr.code != 1 || first_string16(data).as_deref() != Some(ACTIVITY_HOST_DESCRIPTOR) {
            return Reply::unknown_transaction(tr, "unsupported hosted-activity registration");
        }
        let Some((_, at)) = read_string16(data, 12) else {
            return Reply::unknown_transaction(tr, "missing hosted-activity interface token");
        };
        let Some((weak, cookie, offset)) = find_local_binder(data).filter(|(_, _, offset)| *offset == at) else {
            return Reply::unknown_transaction(tr, "activity token must be a non-null local Binder");
        };
        let Some(bytes) = data.get(offset + 28..offset + 36) else {
            return Reply::unknown_transaction(tr, "missing activity token id");
        };
        let id = u64::from_ne_bytes(bytes.try_into().unwrap());
        let mut tokens = ACTIVITY_TOKENS.lock().unwrap_or_else(|e| e.into_inner());
        if id == 0 || tokens.len() >= 64 || tokens.contains_key(&(weak, cookie))
            || tokens.values().any(|value| *value == id) {
            return Reply::unknown_transaction(tr, "activity token registration refused");
        }
        tokens.insert((weak, cookie), id);
        return Reply { parcel: vec![0; 4], objects: Vec::new(), flags: 0 };
    }
    if trace_enabled() {
        let name = name_for_handle(tr.target_handle).unwrap_or_else(|| "?".to_string());
        diag(&format!(
            "[binder]   dispatch target={} ({name}) code={}\n",
            tr.target_handle, tr.code
        ));
    }
    if reply_has_status(tr.target_handle, tr.code) {
        parcel.extend_from_slice(&0u32.to_ne_bytes());
    }
    // Any transaction that carries a local binder object is a client handing us
    // a node it wants called back on. `attachApplication` is the one that
    // matters: its object is the app's `ApplicationThread`.
    if tr.data_size > 0 && tr.data_ptr != 0 {
        let data = std::slice::from_raw_parts(tr.data_ptr as *const u8, tr.data_size as usize);
        if let Some((weak, cookie, at)) = find_local_binder(data) {
            let previous = APP_THREAD_WEAK.swap(weak, Ordering::Relaxed);
            APP_THREAD_COOKIE.store(cookie, Ordering::Relaxed);
            if previous != weak {
                diag(&format!(
                    "[binder] captured app thread weak={weak:#x} cookie={cookie:#x} (object at offset {at}, target handle {})\n",
                    tr.target_handle
                ));
            }
            if std::env::var_os("PENGUDROID_BINDER_PROBE").is_some() {
                seed_probe_codes();
                APP_THREAD_PROBE.store(true, Ordering::Relaxed);
            }
        }
    }

    if tr.target_handle == 0 && !is_hidl_request(tr) {
        let codes = *SERVICE_MANAGER_CODES.lock().unwrap_or_else(|e| e.into_inner());
        let Some(metadata) = service_manager_shape(codes, tr.code) else {
            return Reply::unknown_transaction(tr, "IServiceManager codes not installed or method unsupported");
        };
        let data = std::slice::from_raw_parts(tr.data_ptr as *const u8, tr.data_size as usize);
        let Some(name) = last_string16(data) else {
            return Reply::unknown_transaction(tr, "IServiceManager request has no service name");
        };
        let handle = (name != "autofill").then(|| handle_for(&name));
        push_service_reply(&mut parcel, &mut objects, handle, metadata);
    }
    if tr.target_handle == handle_for("codec2") {
        if !crate::codec2::available() {
            return Reply::unknown_transaction(tr, "native Codec2 component store unavailable; no placeholder service");
        }
        // Only the real backend may list codecs or create components. Its
        // captured reply owns the native service's marshalling contract.
        if tr.data_size > 0 && tr.data_ptr != 0 && tr.code < 0x0F00_0000 {
            let data = std::slice::from_raw_parts(tr.data_ptr as *const u8, tr.data_size as usize);
            let offsets: Vec<u64> = if tr.offsets_ptr != 0 && tr.offsets_size > 0 {
                std::slice::from_raw_parts(
                    tr.offsets_ptr as *const u64,
                    (tr.offsets_size / 8) as usize,
                )
                .to_vec()
            } else {
                Vec::new()
            };
            return codec2_dispatch_reply(tr, crate::codec2::dispatch(tr.code, data, &offsets));
        }
        // Preserve the existing versioned base-interface chain only for a real
        // store. Forwarding its synchronous native reply would leave borrowed
        // hidl_vec storage dangling after onTransact returns.
        if tr.code == HIDL_INTERFACE_CHAIN {
            let chain = [
                "android.hardware.media.c2@1.0::IComponentStore",
                "android.hardware.media.c2@1.0::ICodec2",
                "android.hidl.base@1.0::IBase",
            ];
            push_hidl_string_vec(&mut parcel, &mut objects, &chain);
        } else {
            return Reply::unknown_transaction(tr, "native Codec2 dispatch unavailable or method unsupported; no fabricated reply");
        }
    }

    if tr.target_handle == handle_for("activity_task") && tr.code == APPLICATION_TRANSACTION_CODES.lock().unwrap_or_else(|e| e.into_inner())[0] {
        push_flat_handle(
            &mut parcel,
            &mut objects,
            handle_for("activity_client_controller"),
        );
        if trace_enabled() {
            diag("[binder]   -> activity_client_controller handle\n");
        }
    }
    if tr.target_handle == handle_for("activity") && trace_enabled() {
        // Identify service calls by the UTF-16 strings they carry: an
        // `Intent`'s component name is plain UTF-16, so dumping the text of the
        // request parcel says which AIDL method a transaction code is far
        // faster than guessing transaction numbers.
        let mut text = String::new();
        if tr.data_size > 0 && tr.data_ptr != 0 {
            let data = std::slice::from_raw_parts(tr.data_ptr as *const u8, tr.data_size as usize);
            let mut run: Vec<u16> = Vec::new();
            let mut i = 0usize;
            while i + 1 < data.len() {
                let unit = u16::from_ne_bytes([data[i], data[i + 1]]);
                if (0x20..0x7f).contains(&unit) {
                    run.push(unit);
                } else {
                    if run.len() >= 4 {
                        if !text.is_empty() {
                            text.push('|');
                        }
                        text.push_str(&String::from_utf16_lossy(&run));
                    }
                    run.clear();
                }
                i += 2;
            }
            if run.len() >= 4 {
                if !text.is_empty() {
                    text.push('|');
                }
                text.push_str(&String::from_utf16_lossy(&run));
            }
        }
        diag(&format!(
            "[binder]   activity code={} data={} strings={}\n",
            tr.code, tr.data_size, text
        ));
    }
    let application_codes = *APPLICATION_TRANSACTION_CODES
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let start_service = application_codes[2];
    let bind_service = application_codes[3];
    let bind_service_instance = application_codes[4];
    if tr.target_handle == handle_for("activity")
        && (tr.code == start_service
            || tr.code == bind_service
            || tr.code == bind_service_instance)
    {
        // The player lives in the app's own process (`PlayerService`), and with
        // no system_server nothing schedules its creation, so these two calls
        // are the whole reason a service never starts. The reply shapes matter:
        // `startService` returns a `ComponentName` (`[status][present][pkg][cls]`)
        // and returns **null** to mean "not found", which is what makes the app
        // give up; `bindService` returns an `int` where a negative value throws.
        let data = if tr.data_size > 0 && tr.data_ptr != 0 {
            std::slice::from_raw_parts(tr.data_ptr as *const u8, tr.data_size as usize)
        } else {
            &[][..]
        };
        let component = service_component(data);
        if trace_enabled() {
            diag(&format!(
                "[binder]   -> {} component={component:?}\n",
                if tr.code == start_service { "startService" } else { "bindService" }
            ));
        }
        if tr.code == start_service {
            match &component {
                Some((pkg, cls, _)) => {
                    parcel.extend_from_slice(&1u32.to_ne_bytes()); // present
                    parcel.extend_from_slice(&string16(pkg));
                    parcel.extend_from_slice(&string16(cls));
                }
                None => parcel.extend_from_slice(&0u32.to_ne_bytes()), // null
            }
        } else {
            parcel.extend_from_slice(&0u32.to_ne_bytes()); // 0 = accepted
        }
        if let Some((pkg, cls, package_offset)) = component {
            // The host needs the *real* `Intent`, not just the component: the
            // action is what `Service.onBind` branches on, and the extras are
            // what `onStartCommand` checks (`should_start_foreground_extra`).
            // The whole transaction buffer plus the Intent's offset lets it
            // unmarshall the framework object with `Parcel.unmarshall`, so no
            // per-app guess about either is needed.
            let maybe_offset = intent_start(data, package_offset);
            if trace_enabled() {
                diag(&format!(
                    "[binder]   package_offset={package_offset} intent_offset={maybe_offset:?}\n"
                ));
                let mut hex = String::new();
                for b in data.iter().take(320) {
                    hex.push_str(&format!("{b:02x}"));
                }
                diag(&format!("[binder]   request hex={hex}\n"));
            }
            let intent_offset = maybe_offset.unwrap_or(0);
            note_service_request(
                &pkg,
                &cls,
                tr.code == bind_service || tr.code == bind_service_instance,
                data,
                intent_offset,
            );
        }
    }
    let lifecycle = *ACTIVITY_LIFECYCLE_CODES
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let start_activity = (tr.target_handle == handle_for("activity_task") && tr.code == lifecycle[0])
        || (tr.target_handle == handle_for("activity") && tr.code == lifecycle[1]);
    if start_activity {
        let Some(results) = *ACTIVITY_START_RESULTS.lock().unwrap_or_else(|e| e.into_inner()) else {
            return Reply::unknown_transaction(tr, "framework activity-start result codes not installed");
        };
        let data = if tr.data_size > 0 && tr.data_ptr != 0 {
            std::slice::from_raw_parts(tr.data_ptr as *const u8, tr.data_size as usize)
        } else {
            &[][..]
        };
        match activity_intent(data) {
            Some((intent_offset, pkg, cls))
                if note_activity_start(&pkg, &cls, data, intent_offset) =>
            {
                // `ActivityManager.START_SUCCESS`. The request is now owned by
                // the main-thread host, which is the only place an Activity can
                // be created; the reply is not a claim that it has finished
                // launching.
                parcel.extend_from_slice(&results[0].to_ne_bytes());
                if trace_enabled() {
                    diag(&format!(
                        "[binder]   -> startActivity admitted {pkg}/{cls} intent_offset={intent_offset}\n"
                    ));
                }
            }
            other => {
                let reason = match other {
                    Some((_, ref pkg, ref cls)) => format!(
                        "startActivity for {pkg}/{cls} was not admitted (manifest, package, queue or size limit)"
                    ),
                    None => "startActivity Intent could not be parsed with the API-37 layout".to_string(),
                };
                if trace_enabled() {
                    let mut hex = String::new();
                    for byte in data.iter().take(512) {
                        hex.push_str(&format!("{byte:02x}"));
                    }
                    diag(&format!(
                        "[binder] startActivity parse failed: {} bytes, hex={hex}\n",
                        data.len()
                    ));
                }
                diag(&format!("[binder] {reason}\n"));
                // A recognized but rejected start must return a fatal framework
                // result. UNKNOWN_TRANSACTION alone is insufficient: some
                // generated proxies read a zero default after transact(false),
                // accidentally treating rejection as START_SUCCESS.
                let result = match other {
                    None => results[1],
                    Some((_, ref package, ref class)) if
                        HOSTED_PACKAGE.lock().unwrap_or_else(|e| e.into_inner()).as_deref() != Some(package.as_str())
                        || !HOSTED_ACTIVITIES.lock().unwrap_or_else(|e| e.into_inner()).contains(class) => results[2],
                    Some(_) => results[3],
                };
                parcel.extend_from_slice(&result.to_ne_bytes());
            }
        }
    }
    let finish_descriptor = if tr.target_handle == handle_for("activity") && tr.code == lifecycle[2] {
        Some("android.app.IActivityManager")
    } else if tr.target_handle == handle_for("activity_client_controller") && tr.code == lifecycle[3] {
        Some("android.app.IActivityClientController")
    } else { None };
    if let Some(descriptor) = finish_descriptor {
        let data = if tr.data_size != 0 && tr.data_ptr != 0 {
            std::slice::from_raw_parts(tr.data_ptr as *const u8, tr.data_size as usize)
        } else { &[] };
        let admitted = activity_finish_token(data, descriptor)
            .is_some_and(|(token, result)| note_activity_finish(token, result));
        parcel.extend_from_slice(&i32::from(admitted).to_ne_bytes());
    }
    if tr.target_handle == handle_for("activity") && tr.code == APPLICATION_TRANSACTION_CODES.lock().unwrap_or_else(|e| e.into_inner())[1] {
        let info = PROVIDER_INFO.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if !info.is_empty() {
            // API-37 ContentProviderHolder.writeToParcel (frameworks/base 94b4c16):
            // ProviderInfo, provider/connection binders, then three int flags.
            // readTypedObject supplies the presence int before the holder.
            parcel.extend_from_slice(&1u32.to_ne_bytes());
            parcel.extend_from_slice(&info);
            let handle = handle_for("settings_provider");
            push_flat_handle(&mut parcel, &mut objects, handle);
            push_flat_handle(&mut parcel, &mut objects, handle);
            parcel.extend_from_slice(&0u32.to_ne_bytes()); // noReleaseNeeded
            parcel.extend_from_slice(&0u32.to_ne_bytes()); // mLocal
            parcel.extend_from_slice(&0u32.to_ne_bytes()); // noReleaseNeededIfUnstable
            if trace_enabled() {
                diag(&format!("[binder]   -> ContentProviderHolder ({} bytes)\n", info.len()));
            }
        }
    }
    let media_name = name_for_handle(tr.target_handle);
    if matches!(media_name.as_deref(), Some("media_session" | "media_session_session"
        | "media_session_controller" | "media_session_queue" | "media.metrics" | "media_metrics"))
    {
        let Some(codes) = *MEDIA_TRANSACTION_CODES.lock().unwrap_or_else(|e| e.into_inner()) else {
            return Reply::unknown_transaction(tr, "API-37 media Stub transactions not installed");
        };
        match (media_name.as_deref(), tr.code) {
            (Some("media_session"), code) if code == codes[0] => {
                // MediaSession's constructor requires non-null session/controller
                // handles. These placeholders do not publish a real media session.
                push_flat_handle(&mut parcel, &mut objects, handle_for("media_session_session"));
            }
            (Some("media_session_session"), code) if code == codes[1] => {
                push_flat_handle(&mut parcel, &mut objects, handle_for("media_session_controller"));
            }
            (Some("media_session_session"), code) if code == codes[2] => {
                // Preserve the non-null result required by ParcelableListBinder.send.
                // Queue transfer itself is unsupported and must not report success.
                push_flat_handle(&mut parcel, &mut objects, handle_for("media_session_queue"));
            }
            (Some("media.metrics" | "media_metrics"), code) if code == codes[3] => {
                // PlaybackSession's @NonNull validation requires a nonempty ID.
                // Both the servicemanager and Context service names reach here.
                parcel.extend_from_slice(&string16(&format!(
                    "pengudroid-playback-{}",
                    NEXT_HANDLE.fetch_add(1, Ordering::Relaxed)
                )));
            }
            _ => return Reply::unknown_transaction(tr, "unimplemented media method (including session setters, controller and queue transfer)"),
        }
    }
    if tr.target_handle == handle_for("media.resource_manager") {
        // `MediaCodec::init` returns `ResourceManagerServiceProxy::init()`'s
        // error verbatim (`0x12d348: mov %r14d,%eax; ret`), so a missing
        // resource manager makes every `createDecoderByType` fail with
        // `UNKNOWN_ERROR` before codec2 is ever reached. The proxy only needs
        // the binder to answer `INTERFACE_TRANSACTION` with its descriptor;
        // every other call is a no-op that succeeds.
        if tr.code == INTERFACE_TRANSACTION {
            parcel.extend_from_slice(&string16("android.media.IResourceManagerService"));
            if trace_enabled() {
                diag("[binder]   -> resource manager descriptor\n");
            }
        }
    }
    if tr.target_handle == handle_for("settings_provider") {
        let data = if tr.data_size > 0 && tr.data_ptr != 0 {
            Some(std::slice::from_raw_parts(
                tr.data_ptr as *const u8,
                tr.data_size as usize,
            ))
        } else {
            None
        };
        provider_reply(tr.code, data, &mut parcel, &mut objects);
    }
    if tr.target_handle == handle_for("empty_cursor") {
        // `BulkCursorToCursorAdaptor.close()` and `.getWindow()` are the only
        // calls a zero-row cursor makes.
        match tr.code {
            IBC_GET_CURSOR_WINDOW => {
                parcel.extend_from_slice(&0u32.to_ne_bytes()); // no exception
                parcel.extend_from_slice(&0u32.to_ne_bytes()); // null window
            }
            _ => parcel.extend_from_slice(&0u32.to_ne_bytes()),
        }
        if trace_enabled() {
            diag(&format!("[binder]   -> empty cursor code {}\n", tr.code));
        }
    }
    if tr.target_handle == handle_for("user") {
        let Some(codes) = *USER_TRANSACTION_CODES.lock().unwrap_or_else(|e| e.into_inner()) else {
            return Reply::unknown_transaction(tr, "API-37 IUserManager Stub transactions not installed");
        };
        if !codes.contains(&tr.code) {
            return Reply::unknown_transaction(tr, "unimplemented IUserManager method");
        }
        // The hosted user is unlocked, allowing credential-encrypted preferences
        // and provider lookup. Only the two installed unlock queries answer true.
        parcel.extend_from_slice(&1u32.to_ne_bytes());
    }
    if tr.target_handle == handle_for("audio") {
        let Some(table) = *AUDIO_FOCUS_TRANSACTIONS.lock().unwrap_or_else(|e| e.into_inner()) else {
            return Reply::unknown_transaction(tr, "API-37 audio-focus Stub transactions not installed");
        };
        match table.operation(tr.code) {
            Some(crate::audiofocus::Operation::Request) => {
                let data = if tr.data_size > 0 && tr.data_ptr != 0 {
                    std::slice::from_raw_parts(tr.data_ptr as *const u8, tr.data_size as usize)
                } else { &[] };
                // AIDL order: AudioAttributes, durationHint, IBinder client.
                // Locate the real client object instead of guessing the
                // variable-length AudioAttributes parcel's size.
                let gain = find_local_binder(data).and_then(|(_, _, at)| {
                    let from = at.checked_sub(4)?;
                    Some(i32::from_ne_bytes(data.get(from..at)?.try_into().ok()?))
                }).unwrap_or(0);
                let result = AUDIO_FOCUS.request(gain);
                parcel.extend_from_slice(&result.to_ne_bytes());
                if trace_enabled() {
                    diag(&format!("[binder]   -> audio focus gain={gain} result={result}\n"));
                }
            }
            Some(crate::audiofocus::Operation::Abandon) => parcel.extend_from_slice(&AUDIO_FOCUS.abandon().to_ne_bytes()),
            Some(crate::audiofocus::Operation::Unregister) => { AUDIO_FOCUS.abandon(); }
            Some(crate::audiofocus::Operation::Current) => parcel.extend_from_slice(&AUDIO_FOCUS.current().to_ne_bytes()),
            None => return Reply::unknown_transaction(tr, "unimplemented IAudioService method"),
        }
    }
    if tr.target_handle == handle_for("input") {
        let Some(codes) = *INPUT_TRANSACTION_CODES.lock().unwrap_or_else(|e| e.into_inner()) else {
            return Reply::unknown_transaction(tr, "API-37 IInputManager Stub transactions not installed");
        };
        let Some(operation) = codes.iter().position(|&code| code != 0 && code == tr.code)
            .map(|i| INPUT_OPERATIONS[i]) else {
            return Reply::unknown_transaction(tr, "unimplemented IInputManager method");
        };
        let data = if tr.data_size > 0 && tr.data_ptr != 0 {
            std::slice::from_raw_parts(tr.data_ptr as *const u8, tr.data_size as usize)
        } else { &[] };
        let Some(args) = crate::virtual_input::arguments(data) else {
            return Reply::unknown_transaction(tr, "invalid IInputManager interface token");
        };
        let device = VIRTUAL_INPUT_DEVICE.lock().unwrap_or_else(|e| e.into_inner());
        let Some(body) = crate::virtual_input::reply(operation, args, &device) else {
            return Reply::unknown_transaction(tr, "unsupported or malformed IInputManager arguments");
        };
        parcel.extend_from_slice(&body);
    }
    if tr.target_handle == handle_for("display") && tr.code == IDM_GET_DISPLAY_INFO {
        let info = DISPLAY_INFO.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if !info.is_empty() {
            // `getDisplayInfo` reads the result with `readTypedObject`.
            parcel.extend_from_slice(&1u32.to_ne_bytes());
            parcel.extend_from_slice(&info);
            if trace_enabled() {
                diag(&format!("[binder]   -> DisplayInfo ({} bytes)\n", info.len()));
            }
        }
    }
    if tr.target_handle == handle_for("package") && tr.code == IPM_GET_PACKAGE_INFO {
        let cached = PACKAGE_INFO.lock().unwrap_or_else(|e| e.into_inner());
        if !cached.is_empty() {
            // `IPackageManager$Stub$Proxy.getPackageInfo` reads the result with
            // `Parcel.readTypedObject`, which first reads a presence int and
            // only then hands the parcel to `PackageInfo.CREATOR`. Without the
            // `1` the reader starts four bytes late and misreads every field.
            parcel.extend_from_slice(&1u32.to_ne_bytes());
            parcel.extend_from_slice(&cached);
            if trace_enabled() {
                diag(&format!("[binder]   -> PackageInfo ({} bytes)\n", cached.len()));
            }
        }
    }
    if tr.target_handle == handle_for("surface_composer_client") {
        if let Err(error) = require_api37() {
            return Reply::unknown_transaction(tr, &error);
        }
        if tr.code != ISCC_CREATE_SURFACE {
            return Reply::unknown_transaction(tr, "unimplemented API-37 ISurfaceComposerClient method");
        }
        // `CreateSurfaceResult createSurface(@utf8InCpp String name, int flags,
        //   @nullable IBinder parent, in LayerMetadata metadata)`
        //
        // The reply is `[Status][CreateSurfaceResult]` and the parcelable is
        // written by `android::gui::CreateSurfaceResult::writeToParcel`
        // (disassembled from the shipped `libgui.so`):
        //
        //   int32    size prefix
        //   binder   handle    (+ stability word)
        //   int32    layerId
        //   String16 layerName
        //   int32    transformHint
        //
        // `Parcel::readData(Parcelable*)` reads the `kNonNullParcelableFlag`
        // word first and returns `UNEXPECTED_NULL` when it is not 1, which the
        // proxy then turns into `PERMISSION_DENIED`.
        parcel.extend_from_slice(&1i32.to_ne_bytes()); // kNonNullParcelableFlag
        let size_at = parcel.len();
        parcel.extend_from_slice(&0i32.to_ne_bytes()); // size placeholder
        push_flat_handle(&mut parcel, &mut objects, handle_for("surface_handle"));
        parcel.extend_from_slice(&1i32.to_ne_bytes()); // layerId
        parcel.extend_from_slice(&string16("pengudroid")); // layerName
        parcel.extend_from_slice(&0u32.to_ne_bytes()); // transformHint
        let size = (parcel.len() - size_at) as u32;
        parcel[size_at..size_at + 4].copy_from_slice(&size.to_ne_bytes());
        if trace_enabled() {
            diag("[binder]   -> CreateSurfaceResult\n");
        }
    }

    if tr.target_handle == handle_for("display_event_connection") {
        if let Err(error) = require_api37() {
            return Reply::unknown_transaction(tr, &error);
        }
        if !matches!(tr.code, IDEC_STEAL_RECEIVE_CHANNEL | IDEC_SET_VSYNC_RATE | IDEC_REQUEST_NEXT_VSYNC) {
            return Reply::unknown_transaction(tr, "unimplemented IDisplayEventConnection method (getLatestVsyncEventData is unsupported)");
        }
    }
    if tr.target_handle == handle_for("display_event_connection")
        && tr.code == IDEC_STEAL_RECEIVE_CHANNEL
    {
        // `[present=1][BitTube]` — the status word was already written above.
        // The presence int is `Parcel::readData(Parcelable*)`, which returns
        // UNEXPECTED_NULL unless it reads `kNonNullParcelableFlag` (1) before
        // the parcelable body: the AIDL runtime writes it in `writeParcelable`
        // and reads it back here, so it is part of the wire format even though
        // `BitTube` itself only serialises two descriptors.
        parcel.extend_from_slice(&1u32.to_ne_bytes());
        match vsync_channel() {
            Some((recv, send)) => {
                push_flat_fd(&mut parcel, &mut objects, recv);
                push_flat_fd(&mut parcel, &mut objects, send);
                start_frame_clock();
                if trace_enabled() {
                    diag(&format!("[binder]   -> BitTube fds {recv},{send}\n"));
                }
            }
            None => {
                // `BitTube::readFromParcel` needs both descriptors; a null one
                // makes it return BAD_TYPE, which is what the client reports
                // as a failed `nativeInit`.
                push_flat_fd(&mut parcel, &mut objects, -1);
                push_flat_fd(&mut parcel, &mut objects, -1);
            }
        }
    }
    if tr.target_handle == handle_for("display_event_connection") {
        // Only the measured setVsyncRate/requestNextVsync operations are known.
        // Other methods must not receive a fabricated empty success reply.
        if tr.code != IDEC_STEAL_RECEIVE_CHANNEL {
            parcel.extend_from_slice(&0u32.to_ne_bytes());
        }
        if matches!(tr.code, IDEC_REQUEST_NEXT_VSYNC | IDEC_SET_VSYNC_RATE) {
            // ViewRootImpl schedules its first traversal from this pulse.
            if let Err(error) = send_vsync() {
                return Reply::unknown_transaction(tr, &error);
            }
        }
    }
    if tr.target_handle == handle_for("window") && tr.code == IWM_OPEN_SESSION {
        // `IWindowManager.openSession` returns the `IWindowSession` the client
        // keeps for the life of the window.
        push_flat_handle(&mut parcel, &mut objects, handle_for("window_session"));
        if trace_enabled() {
            diag("[binder]   -> window_session handle\n");
        }
    }
    if tr.target_handle == handle_for("window_session") {
        // The first argument of `relayout` is the app's own `IWindow`, i.e. a
        // local binder, so its cookie identifies the window. Two windows exist
        // in a typical app (an activity and a dialog), and a reply that lands
        // for one but not the other is invisible without this.
        if trace_enabled()
            && window_session_method(tr.code) == Some(WS_ADD_TO_DISPLAY_AS_USER)
            && tr.data_size > 0
            && tr.data_ptr != 0
        {
            let data =
                std::slice::from_raw_parts(tr.data_ptr as *const u8, tr.data_size as usize);
            let win = find_local_binder(data).map(|(_, cookie, _)| cookie).unwrap_or(0);
            diag(&format!(
                "[binder]   addToDisplay win={win:#x} data={}\n",
                data.len()
            ));
        }
        // Pair each relayout with its reply by a tick number. Two relayouts that
        // overlap across threads then show up as a `begin` without a matching
        // `end` before the next `begin` — which is what a shared reply buffer
        // would break, and what the old non-atomic bump pointer allowed.
        static RELAYOUT_TICK: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(1);
        let mut relayout_tick: Option<(u64, u64)> = None;
        if trace_enabled()
            && window_session_method(tr.code) == Some(WS_RELAYOUT_ASYNC)
            && tr.data_size > 0
            && tr.data_ptr != 0
        {
            // Dump the raw `relayoutAsync` parcel so its field layout can be read
            // off real bytes instead of being reconstructed by hand. Offset 0 is
            // the `TSYS` header, so the window's `seq` sits right after the
            // `IWindow` object; with this the *actual* requested width/height are
            // known and can be replayed to the window manager side.
            let data = std::slice::from_raw_parts(tr.data_ptr as *const u8, tr.data_size as usize);
            let win = find_local_binder(data).map(|(_, cookie, _)| cookie).unwrap_or(0);
            let mut dump = format!(
                "[binder]   relayoutAsync dump win={win:#x} data={} offsets={}\n",
                data.len(),
                tr.offsets_size
            );
            for (i, chunk) in data.chunks(32).enumerate() {
                let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
                dump.push_str(&format!("      {:04x}: {}\n", i * 32, hex.join(" ")));
            }
            diag(&dump);
        }
        if trace_enabled()
            && window_session_method(tr.code) == Some(WS_RELAYOUT)
            && tr.data_size > 0
            && tr.data_ptr != 0
        {
            let data =
                std::slice::from_raw_parts(tr.data_ptr as *const u8, tr.data_size as usize);
            let win = find_local_binder(data).map(|(_, cookie, _)| cookie).unwrap_or(0);
            // Everything after the `IWindow` object is the rest of the call:
            // `seq, requestedWidth, requestedHeight, viewVisibility, flags, …`.
            // The object is 24 bytes (type/flags/binder/cookie) plus a stability
            // word on Android 12+, so dump both candidate alignments rather than
            // guess which one this framework writes.
            let at = find_local_binder(data).map(|(_, _, at)| at).unwrap_or(0);
            let mut ints = Vec::new();
            for off in [at + 24, at + 28] {
                if off + 20 <= data.len() {
                    let v: Vec<i32> = (0..5)
                        .map(|i| {
                            i32::from_ne_bytes(
                                data[off + 4 * i..off + 4 * i + 4].try_into().unwrap(),
                            )
                        })
                        .collect();
                    ints.push(format!("@{off}:{v:?}"));
                }
            }
            let tick = RELAYOUT_TICK.fetch_add(1, Ordering::Relaxed);
            relayout_tick = Some((tick, win));
            diag(&format!(
                "[binder]   relayout#{tick} begin win={win:#x} tid={:?} data={} {}\n",
                std::thread::current().id(),
                data.len(),
                ints.join(" ")
            ));
        }
        let reply_start = parcel.len();
        window_session_reply(tr.code, &mut parcel, &mut objects);
        if let Some((tick, win)) = relayout_tick {
            diag(&format!(
                "[binder]   relayout#{tick} end win={win:#x} tid={:?} reply={} byte(s)\n",
                std::thread::current().id(),
                parcel.len() - reply_start
            ));
        }
        if window_session_method(tr.code) == Some(WS_RELAYOUT_ASYNC) {
            // `relayoutAsync` has no reply: the client expects the window manager
            // to call `IWindow.resized`, and it uses that call as the "layout is
            // settled" signal.
            //
            // Measured: replaying `IWindow.resized` through the emulated binder
            // does *not* work here — the app's `IWindow.Stub.onTransact` answers
            // `UNKNOWN_TRANSACTION` for every code (even
            // `INTERFACE_TRANSACTION`), so the replay reaches a binder node that
            // does not run the stub. The host therefore plays the window manager
            // directly (`PENGUDROID_PUMP_RESIZED` calls
            // `ViewRootImpl.dispatchResized` through JNI); this path stays behind
            // an opt-in flag for whoever wants to keep digging.
            let interface_probe = std::env::var_os("PENGUDROID_INTERFACE_PROBE").is_some();
            let binder_resized = std::env::var_os("PENGUDROID_BINDER_RESIZED").is_some();
            if !interface_probe && !binder_resized {
                if trace_enabled() {
                    diag("[binder]   relayoutAsync: host-side resized pump is authoritative\n");
                }
            } else if tr.data_size > 0 && tr.data_ptr != 0 {
                let data =
                    std::slice::from_raw_parts(tr.data_ptr as *const u8, tr.data_size as usize);
                match find_local_binder(data) {
                    Some((weak, cookie, at)) => {
                        let payload = RESIZED_PAYLOAD
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .clone();
                        if payload.is_empty() && !interface_probe {
                            diag("[binder]   relayoutAsync: no IWindow.resized payload installed\n");
                        } else {
                            let mut body = string16(IWINDOW_DESCRIPTOR);
                            if !interface_probe {
                                body.extend_from_slice(&payload);
                            }
                            // `PENGUDROID_RESIZED_CODE` and
                            // `PENGUDROID_INTERFACE_PROBE` exist to prove the
                            // delivery path itself. `INTERFACE_TRANSACTION`
                            // (1598968902) is answered by every `Binder` with its
                            // descriptor, so a *synchronous* delivery of it either
                            // returns "android.view.IWindow" (the node is the
                            // app's window stub and delivery works) or an
                            // exception (it is not).
                            let override_code = std::env::var("PENGUDROID_RESIZED_CODE")
                                .ok()
                                .and_then(|v| v.trim().parse::<u32>().ok());
                            let (code, flags) = if interface_probe {
                                (override_code.unwrap_or(IBINDER_INTERFACE_TRANSACTION), 0)
                            } else {
                                (override_code.unwrap_or(IWINDOW_RESIZED), TF_ONE_WAY)
                            };
                            APP_TX_QUEUE
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .push_back(PendingAppTransaction {
                                    weak,
                                    cookie,
                                    code,
                                    flags,
                                    parcel: body,
                                });
                            if trace_enabled() {
                                diag(&format!(
                                    "[binder]   relayoutAsync win={cookie:#x} (object at {at}): queued code {code} flags={flags:#x}\n"
                                ));
                            }
                        }
                    }
                    None => {
                        if trace_enabled() {
                            diag("[binder]   relayoutAsync: no IWindow object in the request\n");
                        }
                    }
                }
            }
        }
    }
    if tr.target_handle == handle_for("locale")
        && (tr.code == ILM_GET_APPLICATION_LOCALES || tr.code == ILM_GET_SYSTEM_LOCALES)
    {
        // `[presence=1][LocaleList]`, and a `LocaleList` is a string array of
        // language tags — empty means "no per-app override".
        parcel.extend_from_slice(&1u32.to_ne_bytes());
        parcel.extend_from_slice(&0u32.to_ne_bytes());
        if trace_enabled() {
            diag("[binder]   -> empty LocaleList\n");
        }
    }
    if tr.target_handle == handle_for("thermalservice") {
        let Some(codes) = *THERMAL_TRANSACTION_CODES.lock().unwrap_or_else(|e| e.into_inner()) else {
            return Reply::unknown_transaction(tr, "API-37 IThermalService Stub transactions not installed");
        };
        let Some(operation) = codes.iter().position(|&code| code == tr.code) else {
            return Reply::unknown_transaction(tr, "unimplemented IThermalService method");
        };
        // Semantic table positions, not wire codes. The hosted model has no
        // thermal zones or status changes; registrations remain inert. Return
        // types are pinned by frameworks/base 94b4c16 IThermalService.aidl.
        match operation {
            0 | 1 | 2 | 5 | 6 => {
                parcel.extend_from_slice(&1u32.to_ne_bytes()); // register/unregister: true
            }
            3 | 4 | 8 | 9 => {
                parcel.extend_from_slice(&0u32.to_ne_bytes()); // empty typed array
            }
            7 => parcel.extend_from_slice(&0u32.to_ne_bytes()), // THERMAL_STATUS_NONE
            10 => parcel.extend_from_slice(&0.0f32.to_ne_bytes()),
            _ => unreachable!(),
        }
        if trace_enabled() {
            diag(&format!("[binder]   -> thermal code {} operation={operation}\n", tr.code));
        }
    }
    if tr.target_handle == handle_for("package") && tr.code == IPM_GET_PROVIDER_INFO {
        // The request is `[interface token][ComponentName][flags][userId]`, so
        // the class name is not the last thing in the parcel. Match the parcel
        // against the names we installed instead of trying to parse it.
        let data = if tr.data_size > 0 && tr.data_ptr != 0 {
            Some(std::slice::from_raw_parts(
                tr.data_ptr as *const u8,
                tr.data_size as usize,
            ))
        } else {
            None
        };
        let entry = data.and_then(|data| {
            let entries = PROVIDER_ENTRIES.lock().unwrap_or_else(|e| e.into_inner());
            entries
                .iter()
                .find(|(name, _)| parcel_contains_utf16(data, name))
                .map(|(_, bytes)| bytes.clone())
        });
        if let Some(bytes) = entry {
            parcel.extend_from_slice(&1u32.to_ne_bytes());
            parcel.extend_from_slice(&bytes);
            if trace_enabled() {
                diag(&format!("[binder]   -> ProviderInfo ({} bytes)\n", bytes.len()));
            }
        }
    }
    if tr.target_handle == handle_for("package") && tr.code == IPM_GET_ACTIVITY_INFO {
        let cached = ACTIVITY_INFO.lock().unwrap_or_else(|e| e.into_inner());
        if !cached.is_empty() {
            parcel.extend_from_slice(&1u32.to_ne_bytes());
            parcel.extend_from_slice(&cached);
            if trace_enabled() {
                diag(&format!("[binder]   -> ActivityInfo ({} bytes)\n", cached.len()));
            }
        }
    }
    if tr.target_handle == handle_for("package") && tr.code == IPM_GET_APPLICATION_INFO {
        let cached = APPLICATION_INFO.lock().unwrap_or_else(|e| e.into_inner());
        if !cached.is_empty() {
            parcel.extend_from_slice(&1u32.to_ne_bytes());
            parcel.extend_from_slice(&cached);
            if trace_enabled() {
                diag(&format!("[binder]   -> ApplicationInfo ({} bytes)\n", cached.len()));
            }
        }
    }
    let _ = pid;
    // Pad to 8 bytes: binder's parcel buffers are 8-byte aligned and some
    // readers are stricter about the size than about trailing zero bytes.
    while parcel.len() % 8 != 0 {
        parcel.push(0);
    }
    Reply { parcel, objects, flags: 0 }
}

/// Find a `flat_binder_object` describing a binder that lives in this process.
///
/// The parcel layout libbinder writes is `[u32 type][u32 flags][u64 binder or
/// handle][u64 cookie]`, followed by the stability word for handle objects.
/// Returns the object's address and its offset in the parcel.
fn find_local_binder(data: &[u8]) -> Option<(u64, u64, usize)> {
    let mut at = 0usize;
    while at + 32 <= data.len() {
        let ty = u32::from_ne_bytes(data[at..at + 4].try_into().ok()?);
        if ty == BINDER_TYPE_BINDER {
            let weak = u64::from_ne_bytes(data[at + 8..at + 16].try_into().ok()?);
            let cookie = u64::from_ne_bytes(data[at + 16..at + 24].try_into().ok()?);
            if weak != 0 && cookie != 0 {
                return Some((weak, cookie, at));
            }
        }
        at += 4;
    }
    None
}

/// Build an incoming `BR_TRANSACTION` command addressed to the app's binder node.
///
/// `code` is the AIDL transaction the caller wants to invoke and `parcel` its
/// body. Returns the command bytes and the object-offset table.
///
/// `flags` carries `TF_ONE_WAY` for oneway AIDL methods. A synchronous delivery
/// makes the client call `sendReply`, which waits for `BR_TRANSACTION_COMPLETE`;
/// the broker emits that when it parses the resulting `BC_REPLY`.
unsafe fn incoming_transaction(
    target_weak: u64,
    cookie: u64,
    code: u32,
    flags: u32,
    parcel: &[u8],
) -> Option<Vec<u8>> {
    let reply = Reply {
        parcel: parcel.to_vec(),
        objects: Vec::new(),
        flags: 0,
    };
    let (addr, offsets_addr) = place_reply(&reply)?;
    let tr = TransactionData {
        target_handle: target_weak as u32,
        _pad0: (target_weak >> 32) as u32,
        cookie,
        code,
        flags,
        sender_pid: libc::getpid(),
        sender_euid: 0,
        data_size: parcel.len() as u64,
        offsets_size: 0,
        data_ptr: addr,
        offsets_ptr: offsets_addr,
    };
    let mut out = Vec::new();
    push_u32(&mut out, BR_TRANSACTION);
    push_transaction(&mut out, &tr);
    Some(out)
}

/// Read `(data_ptr, data_size)` from the command at `off`, for `BC_REPLY`.
unsafe fn tr_reply_data(buf: u64, off: u64) -> Option<(u64, u64)> {
    let tr = transaction_at(buf, off);
    Some((tr.data_ptr, tr.data_size))
}

/// Encode a `String16` the way `Parcel::writeString16` does: a `u32` char
/// count, the UTF-16LE code units, a terminating NUL, padded to 4 bytes.
pub(crate) fn string16(s: &str) -> Vec<u8> {
    let units: Vec<u16> = s.encode_utf16().collect();
    let mut out = Vec::with_capacity(4 + units.len() * 2 + 4);
    out.extend_from_slice(&(units.len() as u32).to_ne_bytes());
    for u in units {
        out.extend_from_slice(&u.to_ne_bytes());
    }
    out.extend_from_slice(&0u16.to_ne_bytes());
    while out.len() % 4 != 0 {
        out.push(0);
    }
    out
}

/// AIDL interface descriptor of `IApplicationThread`.
const IAPPLICATIONTHREAD: &str = "android.app.IApplicationThread";

/// Body of a probe transaction: just the interface token.
fn probe_parcel() -> Vec<u8> {
    string16(IAPPLICATIONTHREAD)
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
    // Each thread gets its own region so two concurrent transactions cannot
    // clobber each other's reply (see `REPLY_SLOTS`).
    let region = ((len / REPLY_SLOTS) & !7).max(64);
    if total > region {
        return None;
    }
    let (slot, mut used) = REPLY_SLOT.with(|c| c.get());
    let slot = if slot == usize::MAX {
        let s = NEXT_REPLY_SLOT.fetch_add(1, Ordering::Relaxed) % REPLY_SLOTS;
        REPLY_SLOT.with(|c| c.set((s, 0)));
        s
    } else {
        slot
    };
    if used + total > region {
        used = 0;
    }
    let off = slot * region + used;
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
    REPLY_SLOT.with(|c| c.set((slot, used + total)));
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
    // `None` marks a one-way transaction. libbinder still waits for
    // `BR_TRANSACTION_COMPLETE` for those (`waitForResponse(nullptr, nullptr)`)
    // but never for a `BR_REPLY`; emitting nothing leaves the caller spinning in
    // `talkWithDriver` forever.
    let mut pending: Vec<Option<Reply>> = Vec::new();
    // Set when the app replied to a transaction we delivered; `sendReply` waits
    // for a completion even though nobody reads the reply's contents.
    let mut answered_transaction = false;
    let mut off = 0u64;
    while off + 4 <= wr.write_size {
        let cmd = std::ptr::read_unaligned((wr.write_buffer + off) as *const u32);
        off += 4;
        let Some(size) = command_payload(cmd) else { break };
        if off + size > wr.write_size {
            break;
        }
        if cmd == BC_TRANSACTION || cmd == BC_TRANSACTION_SG {
            // The command stream is only 4-byte aligned (a `u32` command code
            // followed by the struct), so this must not be read through a
            // `*const TransactionData` — Rust's alignment check aborts. Copy
            // the bytes out and reinterpret them with `read_unaligned`.
            let tr = transaction_at(wr.write_buffer, off);
            if tr.flags & TF_ONE_WAY == 0 {
                pending.push(Some(make_reply(&tr, pid)));
            } else {
                // A oneway call still has to run: `requestNextVsync` is oneway
                // and its whole point is the side effect (delivering a pulse).
                // The caller never reads a reply, so the parcel is discarded.
                let _ = make_reply(&tr, pid);
                pending.push(None);
            }
        }
        if cmd == BC_REPLY || cmd == BC_REPLY_SG {
            // The app answering a transaction we sent it. `sendReply` waits for
            // `BR_TRANSACTION_COMPLETE` before it returns, so the reply must be
            // acknowledged even though the broker has nothing to do with its
            // contents.
            answered_transaction = true;
            if trace_enabled() {
                // Dump the parcel so a Java exception (if any) is readable.
                let (ptr, size) = tr_reply_data(wr.write_buffer, off).unwrap_or((0, 0));
                if ptr != 0 && size > 0 {
                    let n = (size as usize).min(120);
                    let bytes = std::slice::from_raw_parts(ptr as *const u8, n);
                    let mut hex = format!("[binder]   app reply ({size} bytes):");
                    for b in bytes {
                        hex.push_str(&format!(" {b:02x}"));
                    }
                    hex.push('\n');
                    diag(&hex);
                    let text: String = bytes
                        .iter()
                        .map(|b| if b.is_ascii_graphic() || *b == b' ' { *b as char } else { '.' })
                        .collect();
                    diag(&format!("[binder]   app reply text: {text}\n"));
                }
            }
        }
        off += size;
    }
    // The driver consumes the whole write buffer even for commands we do not
    // interpret; leaving it unconsumed makes libbinder report -EINVAL.
    wr.write_consumed = wr.write_size;

    let mut out: Vec<u8> = Vec::new();
    if answered_transaction {
        push_u32(&mut out, BR_TRANSACTION_COMPLETE);
    }

    // Deliver a queued transaction to the app's own binder node *before* the
    // reply that this call is waiting for.
    //
    // That is what a real driver does when `system_server` calls back inside a
    // transaction, and it is what makes this work without a binder thread pool:
    // `IPCThreadState::waitForResponse` handles `BR_TRANSACTION` in its `default:`
    // arm (dispatching it) and only then reads the `BR_REPLY` it was after. Put
    // the transaction *after* the reply and it is silently dropped.
    if APP_THREAD_PROBE.load(Ordering::Relaxed) {
        let weak = APP_THREAD_WEAK.load(Ordering::Relaxed);
        let cookie = APP_THREAD_COOKIE.load(Ordering::Relaxed);
        if weak != 0 && wr.read_size > 0 {
            // One code per read call; stop when the list is exhausted.
            if let Some(code) = next_probe_code() {
                if let Some(bytes) = unsafe { incoming_transaction(weak, cookie, code, 0, &probe_parcel()) } {
                    if bytes.len() as u64 <= wr.read_size {
                        diag(&format!(
                            "[binder] delivering code={code:#x} to app node weak={weak:#x} cookie={cookie:#x} (read_size={}, cmd_len={}, before {} reply/replies)\n",
                            wr.read_size,
                            bytes.len(),
                            pending.len()
                        ));
                        out.extend_from_slice(&bytes);
                    }
                }
            } else {
                APP_THREAD_PROBE.store(false, Ordering::Relaxed);
            }
        }
    }

    // Deliver queued callbacks to the app's own binder nodes (currently
    // `IWindow.resized`, the window manager's answer to `relayoutAsync`). The
    // kernel driver does this itself; the broker has to emulate it.
    //
    // Only on a *pure read* (`write_size == 0`), which is what a looper thread
    // parked in the driver does. Attaching the callback to the same call that
    // produced it deadlocks: the client is inside `waitForResponse` for its own
    // oneway `relayoutAsync`, `resized` runs there, and the completion it is
    // waiting for can never be parsed (measured: Java stack parked in
    // `IWindowSession$Stub$Proxy.relayoutAsync`).
    if wr.write_size == 0 && wr.read_size > 0 {
        loop {
            if out.len() as u64 >= wr.read_size {
                break;
            }
            let Some(tx) = APP_TX_QUEUE
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop_front()
            else {
                break;
            };
            let Some(bytes) =
                (unsafe { incoming_transaction(tx.weak, tx.cookie, tx.code, tx.flags, &tx.parcel) })
            else {
                if trace_enabled() {
                    diag("[binder]   queued callback could not be placed\n");
                }
                break;
            };
            if out.len() as u64 + bytes.len() as u64 > wr.read_size {
                // No room in this read buffer; keep it for the next call.
                APP_TX_QUEUE
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_front(tx);
                break;
            }
            if trace_enabled() {
                diag(&format!(
                    "[binder]   delivering code={} to app node cookie={:#x} ({} byte parcel)\n",
                    tx.code,
                    tx.cookie,
                    tx.parcel.len()
                ));
            }
            out.extend_from_slice(&bytes);
        }
    }

    for entry in &pending {
        push_u32(&mut out, BR_TRANSACTION_COMPLETE);
        let Some(reply) = entry else { continue };
        push_u32(&mut out, BR_REPLY);
        let mut tr = TransactionData {
            flags: reply.flags,
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
            if trace_enabled() {
                diag(&format!(
                    "[binder]   reply {} bytes does not fit read_size {}\n",
                    out.len(),
                    wr.read_size
                ));
            }
            *libc::__errno_location() = libc::ENOSPC;
            return -1;
        }
        std::ptr::copy_nonoverlapping(out.as_ptr(), wr.read_buffer as *mut u8, out.len());
        wr.read_consumed = out.len() as u64;
        if trace_enabled() {
            diag(&format!("[binder]   -> {} bytes, rc=0\n", out.len()));
        }
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
                let show = (tr.data_size as usize).min(400);
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
    let bytes = CStr::from_ptr(path).to_bytes();
    if bytes.starts_with(b"/dev/dma_heap/") {
        return open_dma_heap().unwrap_or(-1);
    }
    match open_device(bytes) {
        Some(fd) => fd,
        None => -1,
    }
}

// --- dma-buf heaps ---------------------------------------------------------
//
// `libdmabufheap` opens `/dev/dma_heap/system` and allocates with
// `DMA_HEAP_IOCTL_ALLOC`. There is no such device on a desktop kernel, and
// without it `CCodecBufferChannel::start` fails with "cannot allocate memory at
// all". A memfd is a perfectly good dma-buf for our purposes: it is mappable,
// shareable and the GL shim already treats dma-bufs as memfds.

/// `_IOWR('H', 0, struct dma_heap_allocation_data)`.
const DMA_HEAP_IOCTL_ALLOC: c_ulong = 0xC018_4800;

/// Fds we handed out for `/dev/dma_heap/*`.
static DMA_HEAP_FDS: Mutex<Vec<c_int>> = Mutex::new(Vec::new());

unsafe fn memfd(name: &[u8]) -> c_int {
    libc::memfd_create(name.as_ptr() as *const libc::c_char, libc::MFD_CLOEXEC)
}

unsafe fn open_dma_heap() -> Option<c_int> {
    let fd = memfd(b"pengudroid-dma-heap\0");
    if fd < 0 {
        return None;
    }
    DMA_HEAP_FDS.lock().unwrap_or_else(|e| e.into_inner()).push(fd);
    if trace_enabled() {
        diag(&format!("[binder]   dma_heap fd {fd}\n"));
    }
    Some(fd)
}

/// `struct dma_heap_allocation_data { u64 len; u32 fd; u32 fd_flags; u64 heap_flags; }`.
unsafe fn dma_heap_ioctl(fd: c_int, request: c_ulong, arg: *mut c_void) -> Option<i64> {
    let known = DMA_HEAP_FDS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&fd);
    if !known {
        return None;
    }
    if request != DMA_HEAP_IOCTL_ALLOC || arg.is_null() {
        return Some(-(libc::EINVAL as i64));
    }
    let len = *(arg as *const u64);
    let buf = memfd(b"pengudroid-dmabuf\0");
    if buf < 0 {
        return Some(-(libc::ENOMEM as i64));
    }
    if libc::ftruncate(buf, len as libc::off_t) != 0 {
        libc::close(buf);
        return Some(-(libc::ENOMEM as i64));
    }
    *((arg as *mut u32).add(2)) = buf as u32;
    if trace_enabled() {
        diag(&format!("[binder]   dma_heap alloc {len} -> fd {buf}\n"));
    }
    Some(0)
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
        None => match dma_heap_ioctl(fd, request, arg) {
            Some(result) => result,
            None => -4096,
        },
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

/// `void __pengudroid_binder_set_package_info(const uint8_t *bytes, size_t len)`
///
/// Installs the serialized `PackageInfo` the broker answers
/// `IPackageManager.getPackageInfo` with.
///
/// # Safety
/// `ptr` must point to `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_binder_set_package_info(ptr: *const u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    let bytes = std::slice::from_raw_parts(ptr, len).to_vec();
    *PACKAGE_INFO.lock().unwrap_or_else(|e| e.into_inner()) = bytes;
}

/// `void __pengudroid_binder_set_application_info(const uint8_t *bytes, size_t len)`
///
/// Installs the serialized `ApplicationInfo` the broker answers
/// `IPackageManager.getApplicationInfo` with.
///
/// # Safety
/// `ptr` must point to `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_binder_set_application_info(ptr: *const u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    let bytes = std::slice::from_raw_parts(ptr, len).to_vec();
    *APPLICATION_INFO.lock().unwrap_or_else(|e| e.into_inner()) = bytes;
}

/// `void __pengudroid_binder_set_provider_entry(const char *name, size_t name_len,
///                                            const uint8_t *bytes, size_t len)`
///
/// # Safety
/// Both pointers must be readable for their stated lengths.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_binder_set_provider_entry(
    name: *const u8,
    name_len: usize,
    bytes: *const u8,
    len: usize,
) {
    if name.is_null() || bytes.is_null() || name_len == 0 || len == 0 {
        return;
    }
    let key = String::from_utf8_lossy(std::slice::from_raw_parts(name, name_len)).into_owned();
    let value = std::slice::from_raw_parts(bytes, len).to_vec();
    PROVIDER_ENTRIES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, value);
}

/// `void __pengudroid_binder_set_window_relayout_result(const uint8_t *bytes, size_t len)`
///
/// # Safety
/// `ptr` must point to `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_binder_set_window_relayout_result(
    ptr: *const u8,
    len: usize,
) {
    if ptr.is_null() || len == 0 {
        return;
    }
    let bytes = std::slice::from_raw_parts(ptr, len).to_vec();
    *WINDOW_RELAYOUT_RESULT.lock().unwrap_or_else(|e| e.into_inner()) = bytes;
}

/// `void __pengudroid_binder_set_activity_info(const uint8_t *bytes, size_t len)`
///
/// # Safety
/// `ptr` must point to `len` readable bytes.
#[no_mangle]
/// A service the app asked the (absent) system server to start or bind.
///
/// Nothing schedules these inside the app, so the request is recorded here for
/// the host to pick up; `apphost` turns it into an `ActivityThread` call.
#[derive(Clone)]
pub struct ServiceRequest {
    pub package: String,
    pub class: String,
    /// `true` for `bindService`, `false` for `startService`.
    pub bind: bool,
    /// The transaction's data buffer, verbatim, so the host can unmarshall the
    /// `Intent` with the framework's own `Parcel.unmarshall`.
    pub parcel: Vec<u8>,
    /// Offset of the `Intent` inside `parcel`, or 0 when it was not located.
    pub intent_offset: usize,
}

/// Cap on one recorded request, including the raw transaction buffer.
///
/// `bindService`/`startForegroundService` requests are a few hundred bytes and
/// their extras are small; anything bigger is not a request this host can use.
pub const SERVICE_REQUEST_CAP: usize = 8192;

static SERVICE_REQUESTS: Mutex<Vec<ServiceRequest>> = Mutex::new(Vec::new());

fn note_service_request(package: &str, class: &str, bind: bool, data: &[u8], intent_offset: usize) {
    let mut queue = SERVICE_REQUESTS.lock().unwrap_or_else(|e| e.into_inner());
    // Log each distinct request once, but *queue every occurrence*: a rebind
    // after `unbindService` is a real new bind the host has to answer, and the
    // first `connected` callback does not carry over to it.
    if !queue
        .iter()
        .any(|r| r.package == package && r.class == class && r.bind == bind)
    {
        unsafe {
            diag(&format!(
                "[binder] service request: {} {package}/{class}\n",
                if bind { "bind" } else { "start" }
            ));
        }
    }
    queue.push(ServiceRequest {
        package: package.to_string(),
        class: class.to_string(),
        bind,
        parcel: data[..data.len().min(SERVICE_REQUEST_CAP)].to_vec(),
        intent_offset,
    });
}

/// Pop the oldest recorded service request.
///
/// Writes one record into `out` and returns `0`; `-1` when the queue is empty
/// or the record does not fit `cap`.
///
/// Record layout (all integers little-endian):
/// ```text
/// [u32 bind][u32 pkg_len][u32 cls_len][u32 intent_offset][u32 parcel_len]
/// [pkg bytes][class bytes][parcel bytes]
/// ```
/// The host calls this from its service thread; see
/// `apphost::spawn_service_host`.
///
/// # Safety
/// `out` must point at `cap` writable bytes and `out_len` at a writable
/// `usize`.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_binder_take_service_request(
    out: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> i32 {
    if out.is_null() || out_len.is_null() {
        return -1;
    }
    let request = {
        let mut queue = SERVICE_REQUESTS.lock().unwrap_or_else(|e| e.into_inner());
        if queue.is_empty() {
            return -1;
        }
        queue.remove(0)
    };
    let pkg = request.package.as_bytes();
    let cls = request.class.as_bytes();
    let total = 20 + pkg.len() + cls.len() + request.parcel.len();
    if total > cap {
        unsafe {
            diag(&format!(
                "[binder] dropping an oversized service request: {}/{}\n",
                request.package, request.class
            ));
        }
        return -1;
    }
    let mut record = Vec::with_capacity(total);
    record.extend_from_slice(&u32::from(request.bind).to_ne_bytes());
    record.extend_from_slice(&(pkg.len() as u32).to_ne_bytes());
    record.extend_from_slice(&(cls.len() as u32).to_ne_bytes());
    record.extend_from_slice(&(request.intent_offset as u32).to_ne_bytes());
    record.extend_from_slice(&(request.parcel.len() as u32).to_ne_bytes());
    record.extend_from_slice(pkg);
    record.extend_from_slice(cls);
    record.extend_from_slice(&request.parcel);
    std::ptr::copy_nonoverlapping(record.as_ptr(), out, record.len());
    *out_len = record.len();
    0
}

/// One hosted activity transition recorded by the broker.
///
/// The activity itself must be created on the main Looper thread, so the
/// broker never calls into the framework: it records the transaction and lets
/// `apphost` replay it as `ActivityThread.handleLaunchActivity` /
/// `handlePauseActivity` / `handleDestroyActivity`.
#[derive(Clone)]
pub struct ActivityRequest {
    /// `0` = start, `1` = finish.
    pub op: u8,
    /// `finishActivity`'s result code (start requests use 0).
    pub result: i32,
    pub token: u64,
    pub package: String,
    pub class: String,
    /// The transaction buffer, verbatim.
    pub parcel: Vec<u8>,
    /// Offset of the `Intent` inside `parcel`, or 0 when it was not located.
    pub intent_offset: usize,
}

/// Binder transactions can carry a whole `PlayQueue` intent. 512 KiB leaves
/// room for that while keeping the bounded queue's worst case predictable.
pub const ACTIVITY_REQUEST_CAP: usize = 512 * 1024;
/// Requests are only generated by user actions; eight pending transitions is
/// already more than the host can meaningfully present.
const ACTIVITY_REQUEST_QUEUE_CAP: usize = 8;

static ACTIVITY_REQUESTS: Mutex<Vec<ActivityRequest>> = Mutex::new(Vec::new());

/// Record an activity start. Returns `false` when the request cannot be hosted
/// (unknown package, oversized buffer, or a full queue); the caller must not
/// report success in that case.
fn note_activity_start(package: &str, class: &str, data: &[u8], intent_offset: usize) -> bool {
    if data.len() > ACTIVITY_REQUEST_CAP || package.len() > 1024 || class.len() > 1024
        || intent_offset == 0 || intent_offset >= data.len() {
        unsafe {
            diag(&format!(
                "[binder] startActivity rejected: {package}/{class} needs {} bytes (cap {ACTIVITY_REQUEST_CAP})\n",
                data.len()
            ));
        }
        return false;
    }
    let hosted = HOSTED_PACKAGE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if hosted.as_deref() != Some(package) {
        unsafe {
            diag(&format!(
                "[binder] startActivity rejected: {package}/{class} is outside the hosted package {:?}\n",
                hosted
            ));
        }
        return false;
    }
    if !HOSTED_ACTIVITIES.lock().unwrap_or_else(|e| e.into_inner()).contains(class) { return false; }
    let mut queue = ACTIVITY_REQUESTS.lock().unwrap_or_else(|e| e.into_inner());
    if queue.len() >= ACTIVITY_REQUEST_QUEUE_CAP {
        unsafe { diag("[binder] startActivity rejected: activity request queue is full\n"); }
        return false;
    }
    queue.push(ActivityRequest {
        op: 0,
        result: 0,
        token: 0,
        package: package.to_string(),
        class: class.to_string(),
        parcel: data.to_vec(),
        intent_offset,
    });
    ACTIVITY_WAKEUP.store(true, Ordering::Release);
    unsafe { diag(&format!("[binder] activity request: start {package}/{class}\n")); }
    true
}

fn note_activity_finish(token: u64, result: i32) -> bool {
    let mut queue = ACTIVITY_REQUESTS.lock().unwrap_or_else(|e| e.into_inner());
    if queue.len() >= ACTIVITY_REQUEST_QUEUE_CAP {
        unsafe { diag("[binder] finishActivity rejected: activity request queue is full\n"); }
        return false;
    }
    queue.push(ActivityRequest {
        op: 1,
        result,
        token,
        package: String::new(),
        class: String::new(),
        parcel: Vec::new(),
        intent_offset: 0,
    });
    ACTIVITY_WAKEUP.store(true, Ordering::Release);
    unsafe { diag(&format!("[binder] activity request: finish token={token}\n")); }
    true
}

/// Pop the oldest recorded activity transition.
///
/// Record layout (all integers little-endian):
/// ```text
/// [u32 op][i32 result][u64 token][u32 pkg_len][u32 cls_len][u32 intent_offset][u32 parcel_len]
/// [pkg bytes][class bytes][parcel bytes]
/// ```
/// Returns `0` on success and `-1` when the queue is empty or the record does
/// not fit `cap`.
///
/// # Safety
/// `out` must point at `cap` writable bytes and `out_len` at a writable
/// `usize`.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_binder_take_activity_request(
    out: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> i32 {
    if out.is_null() || out_len.is_null() {
        return -1;
    }
    let request = {
        let mut queue = ACTIVITY_REQUESTS.lock().unwrap_or_else(|e| e.into_inner());
        let Some(first) = queue.first() else { return -1; };
        let total = 32 + first.package.len() + first.class.len() + first.parcel.len();
        if total > cap { return -1; }
        queue.remove(0)
    };
    let pkg = request.package.as_bytes();
    let cls = request.class.as_bytes();
    let total = 32 + pkg.len() + cls.len() + request.parcel.len();
    let mut record = Vec::with_capacity(total);
    record.extend_from_slice(&(request.op as u32).to_ne_bytes());
    record.extend_from_slice(&request.result.to_ne_bytes());
    record.extend_from_slice(&request.token.to_ne_bytes());
    record.extend_from_slice(&(pkg.len() as u32).to_ne_bytes());
    record.extend_from_slice(&(cls.len() as u32).to_ne_bytes());
    record.extend_from_slice(&(request.intent_offset as u32).to_ne_bytes());
    record.extend_from_slice(&(request.parcel.len() as u32).to_ne_bytes());
    record.extend_from_slice(pkg);
    record.extend_from_slice(cls);
    record.extend_from_slice(&request.parcel);
    std::ptr::copy_nonoverlapping(record.as_ptr(), out, record.len());
    *out_len = record.len();
    0
}

#[no_mangle]
pub unsafe extern "C" fn __pengudroid_binder_set_activity_info(ptr: *const u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    let bytes = std::slice::from_raw_parts(ptr, len).to_vec();
    *ACTIVITY_INFO.lock().unwrap_or_else(|e| e.into_inner()) = bytes;
}

/// `void __pengudroid_binder_set_provider_info(const uint8_t *bytes, size_t len)`
///
/// # Safety
/// `ptr` must point to `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_binder_set_provider_info(ptr: *const u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    let bytes = std::slice::from_raw_parts(ptr, len).to_vec();
    *PROVIDER_INFO.lock().unwrap_or_else(|e| e.into_inner()) = bytes;
}

/// `void __pengudroid_binder_set_empty_bundle(const uint8_t *bytes, size_t len)`
///
/// # Safety
/// `ptr` must point to `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_binder_set_empty_bundle(ptr: *const u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    let bytes = std::slice::from_raw_parts(ptr, len).to_vec();
    *EMPTY_BUNDLE.lock().unwrap_or_else(|e| e.into_inner()) = bytes;
}

/// `void __pengudroid_binder_set_setting(const char *key, size_t key_len,
///                                      const uint8_t *bytes, size_t len)`
///
/// # Safety
/// `key` must point to `key_len` readable bytes and `bytes` to `len` readable
/// bytes.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_binder_set_setting(
    key: *const u8,
    key_len: usize,
    ptr: *const u8,
    len: usize,
) {
    if key.is_null() || key_len == 0 || ptr.is_null() || len == 0 {
        return;
    }
    let Ok(name) = std::str::from_utf8(std::slice::from_raw_parts(key, key_len)) else {
        return;
    };
    let bytes = std::slice::from_raw_parts(ptr, len).to_vec();
    let mut entries = SETTING_ENTRIES.lock().unwrap_or_else(|e| e.into_inner());
    match entries.iter_mut().find(|(k, _)| k == name) {
        Some(slot) => slot.1 = bytes,
        None => entries.push((name.to_string(), bytes)),
    }
}

/// Install the virtual keyboard's framework-written InputDevice parcel.
/// # Safety
/// `ptr` must reference `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_binder_set_input_device(ptr: *const u8, len: usize) {
    if ptr.is_null() || len == 0 { return; }
    *VIRTUAL_INPUT_DEVICE.lock().unwrap_or_else(|e| e.into_inner()) =
        std::slice::from_raw_parts(ptr, len).to_vec();
}

/// `void __pengudroid_binder_set_display_info(const uint8_t *bytes, size_t len)`
///
/// # Safety
/// `ptr` must point to `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_binder_set_display_info(ptr: *const u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    let bytes = std::slice::from_raw_parts(ptr, len).to_vec();
    *DISPLAY_INFO.lock().unwrap_or_else(|e| e.into_inner()) = bytes;
}

/// `void __pengudroid_binder_set_resized_payload(const uint8_t *bytes, size_t len)`
///
/// Installs the body of `IWindow.resized` after the interface token. The host
/// marshals it, because `ClientWindowFrames`/`InsetsState` are framework types
/// whose field order is a framework detail.
///
/// # Safety
/// `ptr` must point to `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_binder_set_resized_payload(ptr: *const u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    let bytes = std::slice::from_raw_parts(ptr, len).to_vec();
    *RESIZED_PAYLOAD.lock().unwrap_or_else(|e| e.into_inner()) = bytes;
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

    static CONTRACT_TEST_LOCK: Mutex<()> = Mutex::new(());

    struct RestoreTable<'a, T> {
        table: &'a Mutex<T>,
        previous: Option<T>,
    }

    fn reset_table<T>(table: &Mutex<T>, value: T) -> RestoreTable<'_, T> {
        let previous = std::mem::replace(&mut *table.lock().unwrap_or_else(|e| e.into_inner()), value);
        RestoreTable { table, previous: Some(previous) }
    }

    impl<T> Drop for RestoreTable<'_, T> {
        fn drop(&mut self) {
            *self.table.lock().unwrap_or_else(|e| e.into_inner()) = self.previous.take().unwrap();
        }
    }

    fn assert_required_transaction_table<const N: usize>(
        table: &Mutex<Option<[u32; N]>>,
        setter: unsafe extern "C" fn(*const u32, usize) -> bool,
        codes: [u32; N],
    ) {
        assert!(!unsafe { setter(std::ptr::null(), N) });
        for len in [0, N - 1, N + 1] {
            assert!(!unsafe { setter(codes.as_ptr(), len) });
        }
        assert_eq!(*table.lock().unwrap(), None);
        assert!(unsafe { setter(codes.as_ptr(), N) });
        for i in 0..N {
            for invalid in [0, 0x00ff_fffe, 0x00ff_ffff, 0x0100_0000] {
                let mut bad = codes;
                bad[i] = invalid;
                assert!(!unsafe { setter(bad.as_ptr(), N) });
                assert_eq!(*table.lock().unwrap(), Some(codes));
            }
            for j in 0..i {
                let mut bad = codes;
                bad[i] = bad[j];
                assert!(!unsafe { setter(bad.as_ptr(), N) });
                assert_eq!(*table.lock().unwrap(), Some(codes));
            }
        }
    }

    fn transaction(target_handle: u32, code: u32, data: &[u8]) -> TransactionData {
        TransactionData {
            target_handle,
            code,
            data_size: data.len() as u64,
            data_ptr: data.as_ptr() as u64,
            ..TransactionData::default()
        }
    }

    fn assert_unknown_transaction(reply: Reply) {
        assert_eq!(reply.flags, TF_STATUS_CODE);
        assert_eq!(reply.parcel, UNKNOWN_TRANSACTION.to_ne_bytes());
        assert!(reply.objects.is_empty());
    }

    fn assert_handle_reply(reply: Reply, name: &str) {
        assert_eq!(reply.flags, 0);
        assert_eq!(&reply.parcel[..4], &0u32.to_ne_bytes());
        assert_eq!(reply.objects, [4]);
        assert_eq!(&reply.parcel[4..8], &BINDER_TYPE_HANDLE.to_ne_bytes());
        assert_eq!(&reply.parcel[12..16], &handle_for(name).to_ne_bytes());
    }

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
        assert_eq!(command_payload(0x0000_720c), None);
        assert_eq!(command_payload(0x8004_6302), None);
        assert_eq!(command_payload(0x0004_6302), None);
        assert_eq!(command_payload(BC_INCREFS_DONE), Some(16));
        assert_eq!(command_payload(BC_ATTEMPT_ACQUIRE), Some(8));
    }

    /// Compile the reference UAPI, rather than comparing two handwritten tables.
    #[test]
    fn android17_kernel_uapi_matches() {
        use std::process::Command;
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let kernel = std::env::var_os("PENGUDROID_TEST_KERNEL_SOURCE")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| root.join(".toolchain/references/kernel-android17-6.18-2026-06_r1"));
        let header = kernel.join("include/uapi/linux/android/binder.h");
        if !header.exists() {
            eprintln!("reference kernel absent; set PENGUDROID_TEST_KERNEL_SOURCE for UAPI verification");
            return;
        }
        let temp = std::env::temp_dir().join(format!("pd-binder-uapi-{}", std::process::id()));
        std::fs::create_dir_all(&temp).unwrap();
        let source = temp.join("probe.c");
        let executable = temp.join("probe");
        let mut c = format!("#include <stdio.h>\n#include \"{}\"\nint main(void) {{\n", header.display());
        c.push_str("printf(\"%u\\n\", (unsigned)BC_TRANSACTION);\n");
        c.push_str("printf(\"%u\\n\", (unsigned)BC_REPLY);\n");
        c.push_str("printf(\"%u\\n\", (unsigned)BC_ACQUIRE_RESULT);\n");
        c.push_str("printf(\"%u\\n\", (unsigned)BC_FREE_BUFFER);\n");
        c.push_str("printf(\"%u\\n\", (unsigned)BC_INCREFS);\n");
        c.push_str("printf(\"%u\\n\", (unsigned)BC_ACQUIRE);\n");
        c.push_str("printf(\"%u\\n\", (unsigned)BC_RELEASE);\n");
        c.push_str("printf(\"%u\\n\", (unsigned)BC_DECREFS);\n");
        c.push_str("printf(\"%u\\n\", (unsigned)BC_INCREFS_DONE);\n");
        c.push_str("printf(\"%u\\n\", (unsigned)BC_ACQUIRE_DONE);\n");
        c.push_str("printf(\"%u\\n\", (unsigned)BC_ATTEMPT_ACQUIRE);\n");
        c.push_str("printf(\"%u\\n\", (unsigned)BC_REGISTER_LOOPER);\n");
        c.push_str("printf(\"%u\\n\", (unsigned)BC_ENTER_LOOPER);\n");
        c.push_str("printf(\"%u\\n\", (unsigned)BC_EXIT_LOOPER);\n");
        c.push_str("printf(\"%u\\n\", (unsigned)BC_REQUEST_DEATH_NOTIFICATION);\n");
        c.push_str("printf(\"%u\\n\", (unsigned)BC_CLEAR_DEATH_NOTIFICATION);\n");
        c.push_str("printf(\"%u\\n\", (unsigned)BC_DEAD_BINDER_DONE);\n");
        c.push_str("printf(\"%u\\n\", (unsigned)BC_TRANSACTION_SG);\n");
        c.push_str("printf(\"%u\\n\", (unsigned)BC_REPLY_SG);\n");
        c.push_str("printf(\"%u\\n\", (unsigned)BR_TRANSACTION_COMPLETE);\n");
        c.push_str("printf(\"%u\\n\", (unsigned)BR_REPLY);\n");
        c.push_str("printf(\"%u\\n\", (unsigned)BR_TRANSACTION);\n");
        c.push_str("printf(\"%u\\n\", (unsigned)TF_STATUS_CODE);\n");
        c.push_str("printf(\"%zu\\n%zu\\n\", sizeof(struct binder_transaction_data), sizeof(struct binder_write_read));\nreturn 0;}\n");
        std::fs::write(&source, c).unwrap();
        assert!(Command::new("cc").arg(&source).arg("-o").arg(&executable).status().unwrap().success());
        let output = Command::new(&executable).output().unwrap();
        assert!(output.status.success());
        let actual: Vec<u64> = String::from_utf8(output.stdout).unwrap()
            .lines().map(|s| s.parse().unwrap()).collect();
        let expected: Vec<u64> = vec![
            BC_TRANSACTION as u64,
            BC_REPLY as u64,
            BC_ACQUIRE_RESULT as u64,
            BC_FREE_BUFFER as u64,
            BC_INCREFS as u64,
            BC_ACQUIRE as u64,
            BC_RELEASE as u64,
            BC_DECREFS as u64,
            BC_INCREFS_DONE as u64,
            BC_ACQUIRE_DONE as u64,
            BC_ATTEMPT_ACQUIRE as u64,
            BC_REGISTER_LOOPER as u64,
            BC_ENTER_LOOPER as u64,
            BC_EXIT_LOOPER as u64,
            BC_REQUEST_DEATH_NOTIFICATION as u64,
            BC_CLEAR_DEATH_NOTIFICATION as u64,
            BC_DEAD_BINDER_DONE as u64,
            BC_TRANSACTION_SG as u64,
            BC_REPLY_SG as u64,
            BR_TRANSACTION_COMPLETE as u64,
            BR_REPLY as u64,
            BR_TRANSACTION as u64,
            TF_STATUS_CODE as u64,
            std::mem::size_of::<TransactionData>() as u64,
            std::mem::size_of::<WriteRead>() as u64,
        ];
        assert_eq!(actual, expected);
        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn android17_display_info_has_sized_nested_parcelables() {
        let word = |bytes: &[u8], at: usize| {
            u32::from_ne_bytes(bytes[at..at + 4].try_into().unwrap())
        };
        let mut parcel = Vec::new();
        push_static_display_info(&mut parcel);
        assert_eq!(parcel.len(), 36);
        assert_eq!((word(&parcel, 0), word(&parcel, 4)), (1, 32));
        assert_eq!(word(&parcel, 16), 2.0f32.to_bits());
        parcel.clear();
        push_dynamic_display_info(&mut parcel);
        assert_eq!(parcel.len(), 112);
        for (offset, expected) in [(0, 1), (4, 108), (8, 1), (12, 1),
            (16, 76), (20, 1), (24, 1), (28, 12), (32, 720), (36, 1280),
            (92, 1), (96, 60.0f32.to_bits()), (100, 1), (104, 0), (108, 0)] {
            assert_eq!(word(&parcel, offset), expected, "field at {offset}");
        }
    }

    #[test]
    fn display_contract_requires_the_shared_api37_property() {
        const CHILD: &str = "PENGUDROID_BINDER_TEST_PLATFORM_FAILURES";
        if std::env::var_os(CHILD).is_some() {
            // Isolate property mutations from the other host unit tests. This
            // process never loads an Android framework or runtime.
            for sdk in [None, Some(""), Some("33"), Some("36"), Some("38")] {
                {
                    let mut props = crate::props_lock();
                    if let Some(sdk) = sdk {
                        props.insert("ro.build.version.sdk".to_string(), sdk.to_string());
                    } else {
                        props.remove("ro.build.version.sdk");
                    }
                }
                assert!(send_vsync().is_err());
                for (name, code, descriptor) in [
                    ("SurfaceFlinger", ISC_CREATE_CONNECTION, "android.gui.ISurfaceComposer"),
                    ("SurfaceFlingerAIDL", ISC_GET_PHYSICAL_DISPLAY_IDS, "android.gui.ISurfaceComposer"),
                    ("surface_composer_client", ISCC_CREATE_SURFACE, "android.gui.ISurfaceComposerClient"),
                    ("display_event_connection", IDEC_STEAL_RECEIVE_CHANNEL, "android.gui.IDisplayEventConnection"),
                    ("display_event_connection", IDEC_REQUEST_NEXT_VSYNC, "android.gui.IDisplayEventConnection"),
                ] {
                    let data = service_request(descriptor, "");
                    assert_unknown_transaction(unsafe { make_reply(&transaction(handle_for(name), code, &data), 1) });
                }
                assert!(VSYNC_SEND_FDS.lock().unwrap().is_empty());
            }
            return;
        }
        assert!(check_api37_sdk(Some("37")).is_ok());
        for sdk in [None, Some(""), Some("33"), Some("36"), Some("38")] {
            let error = check_api37_sdk(sdk).unwrap_err();
            assert!(error.contains("ro.build.version.sdk must be 37"));
        }
        // The emitter reads the shared property store, not an environment-only
        // SDK selector or a default for a missing property.
        require_api37().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "binder::tests::display_contract_requires_the_shared_api37_property", "--nocapture"])
            .env(CHILD, "1")
            .env_remove("PENGUDROID_ROOT")
            .env_remove("BIONIC_APPCOMPAT_OVERRIDE")
            .output().unwrap();
        assert!(output.status.success(), "{}\n{}",
            String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    }

    #[test]
    fn android17_vsync_is_always_the_measured_224_byte_layout() {
        let time = 1_000_000_000i64;
        let event = vsync_event(time, 42);
        let word = |at| u32::from_ne_bytes(event[at..at + 4].try_into().unwrap());
        let long = |at| i64::from_ne_bytes(event[at..at + 8].try_into().unwrap());
        assert_eq!(event.len(), 224);
        assert_eq!(word(0), DISPLAY_EVENT_VSYNC);
        assert_eq!(long(8), 0); // display ID
        assert_eq!(long(16), time);
        assert_eq!(word(24), 42);
        assert_eq!(long(32), 16_666_666);
        assert_eq!(word(40), 0); // preferred timeline
        assert_eq!(word(44), 1); // timeline count, not the old timeline offset
        assert_eq!(word(48), 0); // numberQueuedBuffers
        assert_eq!(word(52), 0); // alignment
        assert_eq!(long(56), 42);
        assert_eq!(long(64), time + 16_666_666);
        assert_eq!(long(72), time + 16_666_666);
        assert!(event[80..].iter().all(|&byte| byte == 0));
    }

    #[test]
    fn surface_composer_preserves_measured_api37_aidl_replies_for_both_aliases() {
        let data = service_request("android.gui.ISurfaceComposer", "");
        for alias in ["SurfaceFlinger", "SurfaceFlingerAIDL"] {
            let target = handle_for(alias);
            for (code, result) in [(2, "display_event_connection"),
                (3, "surface_composer_client"), (7, "display_token")] {
                assert!(reply_has_status(target, code));
                assert_handle_reply(unsafe { make_reply(&transaction(target, code, &data), 1) }, result);
            }
            let reply = unsafe { make_reply(&transaction(target, 6, &data), 1) };
            assert_eq!(reply.flags, 0);
            assert_eq!(reply.parcel.len(), 16);
            assert_eq!(&reply.parcel[..4], &0u32.to_ne_bytes());
            assert_eq!(&reply.parcel[4..8], &1i32.to_ne_bytes());
            assert_eq!(&reply.parcel[8..16], &0i64.to_ne_bytes());
            for (code, size) in [(12, 40), (13, 116), (34, 28), (69, 8)] {
                let reply = unsafe { make_reply(&transaction(target, code, &data), 1) };
                assert_eq!(reply.flags, 0);
                assert_eq!(reply.parcel.len(), size);
                assert_eq!(&reply.parcel[..4], &0u32.to_ne_bytes());
                assert!(reply.objects.is_empty());
                if code == 69 {
                    assert_eq!(&reply.parcel[4..8], &2i32.to_ne_bytes());
                } else {
                    assert_eq!(&reply.parcel[4..8], &1i32.to_ne_bytes()); // sized parcelable
                }
            }
        }
    }

    #[test]
    fn surface_composer_rejects_legacy_tokens_and_unknown_contracts() {
        let _serial = CONTRACT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let aidl = service_request("android.gui.ISurfaceComposer", "");
        let cpp = service_request("android.ui.ISurfaceComposer", "");
        let headerless = string16("android.gui.ISurfaceComposer");
        for alias in ["SurfaceFlinger", "SurfaceFlingerAIDL"] {
            let target = handle_for(alias);
            for code in [0, 2, 3, 4, 5, 6, 7, 9, 12, 13, 27, 34, 54, 55, 69] {
                assert_unknown_transaction(unsafe { make_reply(&transaction(target, code, &cpp), 1) });
            }
            for code in [0, 4, 5, 8, 9, 27, 54, 55, 9999] {
                assert_unknown_transaction(unsafe { make_reply(&transaction(target, code, &aidl), 1) });
            }
            for data in [&[][..], headerless.as_slice()] {
                assert_unknown_transaction(unsafe { make_reply(&transaction(target, 6, data), 1) });
            }
        }
        for name in ["surface_composer_client", "display_event_connection"] {
            assert_unknown_transaction(unsafe { make_reply(&transaction(handle_for(name), 9999, &[]), 1) });
        }
    }

    /// The API-37 C++ `ISurfaceComposer` descriptor shares the SurfaceFlinger
    /// handle with the AIDL one. Its accepted tags answer with an empty body (the
    /// `status_t` travels in the reply header, not in a body status word), which
    /// is what a measured `libgui` caller expects for `SET_TRANSACTION_STATE`.
    #[test]
    fn cpp_surface_composer_accepts_its_own_tags_without_a_body_status() {
        let _serial = CONTRACT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let cpp = service_request("android.ui.ISurfaceComposer", "");
        for alias in ["SurfaceFlinger", "SurfaceFlingerAIDL"] {
            for code in [ISC_CPP_BOOT_FINISHED, ISC_CPP_SET_TRANSACTION_STATE] {
                let reply = unsafe { make_reply(&transaction(handle_for(alias), code, &cpp), 1) };
                assert_eq!(reply.flags, 0, "{alias} code={code}");
                assert!(reply.parcel.is_empty(), "{alias} code={code}");
                assert!(reply.objects.is_empty(), "{alias} code={code}");
            }
        }
    }

    #[test]
    fn surface_composer_client_keeps_the_api37_create_surface_parcelable() {
        let data = service_request("android.gui.ISurfaceComposerClient", "");
        let reply = unsafe { make_reply(&transaction(handle_for("surface_composer_client"), 1, &data), 1) };
        assert_eq!(reply.flags, 0);
        assert_eq!(&reply.parcel[..4], &0u32.to_ne_bytes());
        assert_eq!(&reply.parcel[4..8], &1i32.to_ne_bytes());
        assert_eq!(reply.objects, [12]); // Status, presence, self-size, then binder
        assert_eq!(&reply.parcel[12..16], &BINDER_TYPE_HANDLE.to_ne_bytes());
        assert_eq!(&reply.parcel[20..24], &handle_for("surface_handle").to_ne_bytes());
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
    fn android17_intent_location_includes_extended_flags() {
        let mut data = Vec::new();
        // action, null Uri, type, identifier, flags, extendedFlags, package.
        for word in [u32::MAX, 0, u32::MAX, u32::MAX, 4, 0, u32::MAX] {
            data.extend_from_slice(&word.to_ne_bytes());
        }
        let component_package = data.len();
        data.extend_from_slice(&18u32.to_ne_bytes());
        data.resize(data.len() + 38, 0);
        assert_eq!(intent_start(&data, component_package), Some(0));
    }

    fn write_string16(out: &mut Vec<u8>, value: Option<&str>) {
        match value {
            None => out.extend_from_slice(&u32::MAX.to_ne_bytes()),
            Some(value) => {
                out.extend_from_slice(&(value.len() as u32).to_ne_bytes());
                for unit in value.encode_utf16() {
                    out.extend_from_slice(&unit.to_ne_bytes());
                }
                out.extend_from_slice(&[0, 0]);
                while out.len() % 4 != 0 {
                    out.push(0);
                }
            }
        }
    }

    fn write_string8(out: &mut Vec<u8>, value: Option<&str>) {
        match value {
            None => out.extend_from_slice(&u32::MAX.to_ne_bytes()),
            Some(value) => {
                out.extend_from_slice(&(value.len() as u32).to_ne_bytes());
                out.extend_from_slice(value.as_bytes());
                out.push(0);
                while out.len() % 4 != 0 {
                    out.push(0);
                }
            }
        }
    }

    fn api37_intent(action: Option<&str>, package: &str, class: &str) -> Vec<u8> {
        let mut intent = Vec::new();
        write_string8(&mut intent, action);
        intent.extend_from_slice(&0u32.to_ne_bytes()); // null Uri type id
        write_string8(&mut intent, None); // type
        write_string8(&mut intent, None); // identifier
        intent.extend_from_slice(&0u32.to_ne_bytes()); // flags
        intent.extend_from_slice(&0u32.to_ne_bytes()); // extended flags
        write_string8(&mut intent, None); // package
        write_string16(&mut intent, Some(package));
        write_string16(&mut intent, Some(class));
        intent
    }

    fn api37_start_activity(package: &str, class: &str) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&0x8000_0000u32.to_ne_bytes()); // strict mode
        data.extend_from_slice(&u32::MAX.to_ne_bytes()); // work source uid
        data.extend_from_slice(b"TSYS");
        write_string16(&mut data, Some("android.app.IActivityTaskManager"));
        // `IApplicationThread caller`: flat_binder_object + stability.
        data.extend_from_slice(&BINDER_TYPE_BINDER.to_ne_bytes());
        data.extend_from_slice(&0x7fu32.to_ne_bytes());
        data.extend_from_slice(&0x1234u64.to_ne_bytes());
        data.extend_from_slice(&0x5678u64.to_ne_bytes());
        data.extend_from_slice(&0xcu32.to_ne_bytes());
        write_string16(&mut data, Some("org.schabi.newpipe")); // callingPackage
        write_string16(&mut data, None); // callingFeatureId
        data.extend_from_slice(&1u32.to_ne_bytes()); // typed Intent present
        data.extend_from_slice(&api37_intent(None, package, class));
        data
    }

    #[test]
    fn api37_activity_intent_is_located_after_the_calling_package() {
        let data = api37_start_activity("org.schabi.newpipe", "org.schabi.newpipe.PlayerActivity");
        let (offset, package, class) = activity_intent(&data).expect("activity Intent not located");
        assert_eq!(package, "org.schabi.newpipe");
        assert_eq!(class, "org.schabi.newpipe.PlayerActivity");
        assert_eq!(
            intent_component_at(&data, offset),
            Some((
                "org.schabi.newpipe".to_string(),
                "org.schabi.newpipe.PlayerActivity".to_string(),
                offset + 28,
            ))
        );
    }

    #[test]
    fn api37_activity_intent_requires_header_and_typed_object_presence() {
        let data = api37_start_activity("org.schabi.newpipe", "org.schabi.newpipe.PlayerActivity");
        let (intent, _, _) = activity_intent(&data).expect("API-37 activity Intent");
        let mut missing_magic = data.clone();
        missing_magic.drain(8..12);
        assert!(activity_intent(&missing_magic).is_none());
        for presence in [0u32, 2, u32::MAX] {
            let mut invalid = data.clone();
            invalid[intent - 4..intent].copy_from_slice(&presence.to_ne_bytes());
            assert!(activity_intent(&invalid).is_none(), "presence={presence}");
        }
    }

    #[test]
    fn string8_length_divisible_by_four_keeps_the_backward_walk_aligned() {
        // Eight-byte action: `writeString8` advances by the padded length of
        // `len + 1`, which the old `align(len)` rule got wrong by four bytes.
        let intent = api37_intent(Some("abcdefgh"), "org.schabi.newpipe", "org.schabi.newpipe.MainActivity");
        let component = intent_component_at(&intent, 0).expect("component");
        assert_eq!(component.0, "org.schabi.newpipe");
        assert_eq!(component.2, 40);
        assert_eq!(intent_start(&intent, component.2), Some(0));
    }

    #[test]
    fn start_activity_is_admitted_only_for_the_hosted_package() {
        let _serial = CONTRACT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _queue = reset_table(&ACTIVITY_REQUESTS, Vec::new());
        let _codes = reset_table(&ACTIVITY_LIFECYCLE_CODES, [1, 2, 3, 4]);
        let _results = reset_table(&ACTIVITY_START_RESULTS, Some([0, -101, -102, -103]));
        let _package = reset_table(&HOSTED_PACKAGE, Some("org.schabi.newpipe".to_string()));
        let _activities = reset_table(&HOSTED_ACTIVITIES, ["org.schabi.newpipe.PlayerActivity".to_string()].into_iter().collect());
        let target = handle_for("activity_task");

        let accepted = api37_start_activity(
            "org.schabi.newpipe",
            "org.schabi.newpipe.PlayerActivity",
        );
        let reply = unsafe { make_reply(&transaction(target, 1, &accepted), 1) };
        assert_eq!(reply.flags, 0);
        assert_eq!(&reply.parcel[..8], &[0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(ACTIVITY_REQUESTS.lock().unwrap().len(), 1);

        let rejected = api37_start_activity("com.example.other", "com.example.other.Activity");
        let reply = unsafe { make_reply(&transaction(target, 1, &rejected), 1) };
        assert_eq!(reply.flags, 0);
        assert_eq!(&reply.parcel[4..8], &(-102i32).to_ne_bytes());
        let undeclared = api37_start_activity("org.schabi.newpipe", "org.schabi.newpipe.Missing");
        let reply = unsafe { make_reply(&transaction(target, 1, &undeclared), 1) };
        assert_eq!(&reply.parcel[4..8], &(-102i32).to_ne_bytes());
        assert_eq!(ACTIVITY_REQUESTS.lock().unwrap().len(), 1);
        let reply = unsafe { make_reply(&transaction(target, 1, &[]), 1) };
        assert_eq!(&reply.parcel[4..8], &(-101i32).to_ne_bytes());

        for _ in 1..ACTIVITY_REQUEST_QUEUE_CAP {
            assert!(note_activity_start("org.schabi.newpipe", "org.schabi.newpipe.PlayerActivity", &accepted, 4));
        }
        let reply = unsafe { make_reply(&transaction(target, 1, &accepted), 1) };
        assert_eq!(&reply.parcel[4..8], &(-103i32).to_ne_bytes());
        assert!(__pengudroid_binder_take_activity_wakeup());
        assert!(!__pengudroid_binder_take_activity_wakeup());
        let mut out = [0u8; 1];
        let mut len = 0;
        assert_eq!(unsafe { __pengudroid_binder_take_activity_request(out.as_mut_ptr(), out.len(), &mut len) }, -1);
        assert_eq!(ACTIVITY_REQUESTS.lock().unwrap().len(), ACTIVITY_REQUEST_QUEUE_CAP);
    }

    #[test]
    fn activity_finish_resolves_the_requested_token_not_the_top_activity() {
        let _serial = CONTRACT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _tokens = reset_table(&ACTIVITY_TOKENS, [((0x1234, 0x5678), 1), ((0x2345, 0x6789), 2)].into_iter().collect());
        let _queue = reset_table(&ACTIVITY_REQUESTS, Vec::new());
        let descriptor = "android.app.IActivityClientController";
        let data = |weak: u64, cookie: u64| {
            let mut out = service_request(descriptor, "");
            out.truncate(out.len() - 8); // omit service_request's extra empty String16
            out.extend_from_slice(&BINDER_TYPE_BINDER.to_ne_bytes());
            out.extend_from_slice(&0x7fu32.to_ne_bytes());
            out.extend_from_slice(&weak.to_ne_bytes());
            out.extend_from_slice(&cookie.to_ne_bytes());
            out.extend_from_slice(&0xcu32.to_ne_bytes());
            out.extend_from_slice(&0i32.to_ne_bytes()); // result code
            out.extend_from_slice(&0i32.to_ne_bytes()); // null typed Intent
            out.extend_from_slice(&0i32.to_ne_bytes()); // no task-wide finish
            out
        };
        assert_eq!(activity_finish_token(&data(0x1234, 0x5678), descriptor), Some((1, 0)));
        assert_eq!(activity_finish_token(&data(0x2345, 0x6789), descriptor), Some((2, 0)));
        assert_eq!(activity_finish_token(&data(0x1234, 0x6789), descriptor), None);
        __pengudroid_binder_forget_activity_token(1);
        assert_eq!(activity_finish_token(&data(0x1234, 0x5678), descriptor), None);
        assert_eq!(ACTIVITY_TOKENS.lock().unwrap().len(), 1);
    }

    #[test]
    fn service_manager_requires_all_four_installed_stub_codes() {
        let _serial = CONTRACT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _restore = reset_table(&SERVICE_MANAGER_CODES, [0; 4]);
        let data = service_request("android.os.IServiceManager", "activity");
        for code in [0, 1, 2, 101, 203, 307, 409] {
            assert_eq!(service_manager_shape([0; 4], code), None);
            assert_unknown_transaction(unsafe { make_reply(&transaction(0, code, &data), 1) });
        }
        assert_eq!(service_manager_shape([101, 203, 0, 0], 101), None);
        assert_eq!(service_manager_shape([101, 203, 307, 409], 0), None);

        // Synthetic, deliberately remapped codes, not API-37 wire-number claims.
        let codes = [101, 203, 307, 409];
        assert!(__pengudroid_binder_set_service_manager_codes(codes[0], codes[1], codes[2], codes[3]));
        for i in 0..4 {
            for invalid in [0, 0x00ff_fffe, 0x00ff_ffff, 0x0100_0000] {
                let mut bad = codes;
                bad[i] = invalid;
                assert!(!__pengudroid_binder_set_service_manager_codes(bad[0], bad[1], bad[2], bad[3]));
            }
            for j in 0..i {
                let mut bad = codes;
                bad[i] = bad[j];
                assert!(!__pengudroid_binder_set_service_manager_codes(bad[0], bad[1], bad[2], bad[3]));
            }
        }
        assert!(!__pengudroid_binder_set_service_manager_codes(101, 203, 0, 0));
        assert_eq!(*SERVICE_MANAGER_CODES.lock().unwrap(), codes);
        for (i, code) in codes.into_iter().enumerate() {
            let metadata = i >= 2;
            assert_eq!(service_manager_shape(codes, code), Some(metadata));
            let reply = unsafe { make_reply(&transaction(0, code, &data), 1) };
            assert_eq!(reply.flags, 0);
            assert_eq!(&reply.parcel[..4], &0u32.to_ne_bytes());
            let object_at = if metadata { 20 } else { 4 };
            assert_eq!(reply.objects, [object_at as u64]);
            assert_eq!(&reply.parcel[object_at..object_at + 4], &BINDER_TYPE_HANDLE.to_ne_bytes());
        }
        for code in [0, 1, 2, 9999] {
            assert_unknown_transaction(unsafe { make_reply(&transaction(0, code, &data), 1) });
        }
    }

    #[test]
    fn absent_autofill_does_not_advertise_a_binder_that_never_calls_back() {
        let _serial = CONTRACT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _restore = reset_table(&SERVICE_MANAGER_CODES, [0; 4]);
        assert!(__pengudroid_binder_set_service_manager_codes(101, 203, 307, 409));
        let data = service_request("android.os.IServiceManager", "autofill");
        let reply = unsafe { make_reply(&transaction(0, 101, &data), 1) };
        assert_eq!(reply.flags, 0);
        assert!(reply.objects.is_empty());
        assert_eq!(&reply.parcel[4..8], &BINDER_TYPE_BINDER.to_ne_bytes());
        assert_eq!(&reply.parcel[12..28], &[0u8; 16]);
    }

    #[test]
    fn handles_are_stable_per_name() {
        let a = handle_for("activity");
        let b = handle_for("activity");
        assert_eq!(a, b);
        assert!(a >= 1);
    }

    /// `Binder.onTransact` answers the interface-descriptor transaction before
    /// any method dispatch. A caller that probes a hosted interface must get the
    /// descriptor, not a transport rejection (measured as
    /// `UNKNOWN_TRANSACTION ... code=1598968902` against `SurfaceFlingerAIDL`).
    #[test]
    fn interface_descriptor_transaction_names_each_hosted_interface() {
        let _serial = CONTRACT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for (name, descriptor) in [
            ("SurfaceFlinger", "android.gui.ISurfaceComposer"),
            ("SurfaceFlingerAIDL", "android.gui.ISurfaceComposer"),
            ("surface_composer_client", "android.gui.ISurfaceComposerClient"),
            ("display_event_connection", "android.gui.IDisplayEventConnection"),
        ] {
            let reply = unsafe {
                make_reply(&transaction(handle_for(name), IBINDER_INTERFACE_TRANSACTION, &[]), 1)
            };
            assert_eq!(reply.flags, 0, "{name}");
            assert!(reply.objects.is_empty(), "{name}");
            // No status word: the descriptor is the whole reply.
            assert_eq!(reply.parcel, string16(descriptor), "{name}");
        }
        // A handle this broker does not host is not given an invented identity.
        assert_eq!(hosted_interface_descriptor(handle_for("netstats")), None);
    }

    #[test]
    fn user_unlock_queries_require_installed_codes_and_never_alias_code_71() {
        let _serial = CONTRACT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _restore = reset_table(&USER_TRANSACTION_CODES, None);
        let target = handle_for("user");
        let codes = [403, 907]; // synthetic remapping, not framework wire numbers
        for code in [0, 71, 79, codes[0], codes[1], 9999] {
            assert_unknown_transaction(unsafe { make_reply(&transaction(target, code, &[]), 1) });
        }
        assert_required_transaction_table(&USER_TRANSACTION_CODES,
            __pengudroid_binder_set_user_transactions, codes);
        for code in codes {
            let reply = unsafe { make_reply(&transaction(target, code, &[]), 1) };
            assert_eq!(reply.flags, 0);
            assert_eq!(reply.parcel, [0u32, 1].into_iter().flat_map(u32::to_ne_bytes).collect::<Vec<_>>());
            assert!(reply.objects.is_empty());
        }
        // In the pinned API-37 Stub, 71 is clearSeedAccountData, not an unlock
        // query. Neither it nor the other old default may return true.
        for code in [0, 1, 2, 71, 79, 9999] {
            assert_unknown_transaction(unsafe { make_reply(&transaction(target, code, &[]), 1) });
        }
    }

    #[test]
    fn thermal_methods_require_installed_codes_and_preserve_semantic_reply_types() {
        let _serial = CONTRACT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _restore = reset_table(&THERMAL_TRANSACTION_CODES, None);
        let target = handle_for("thermalservice");
        // Deliberately nonsequential and reordered; positions select methods.
        let codes = [503, 101, 907, 307, 809, 203, 1103, 409, 1009, 607, 709];
        for code in (0..=11).chain([codes[5], 9999]) {
            assert_unknown_transaction(unsafe { make_reply(&transaction(target, code, &[]), 1) });
        }
        assert_required_transaction_table(&THERMAL_TRANSACTION_CODES,
            __pengudroid_binder_set_thermal_transactions, codes);
        // register/unregister booleans, empty arrays, THERMAL_STATUS_NONE, and
        // zero headroom (float bits) from the hosted no-thermal-zone model.
        let results = [1u32, 1, 1, 0, 0, 1, 1, 0, 0, 0, 0.0f32.to_bits()];
        for (code, result) in codes.into_iter().zip(results) {
            let reply = unsafe { make_reply(&transaction(target, code, &[]), 1) };
            assert_eq!(reply.flags, 0);
            assert_eq!(reply.parcel, [0u32, result].into_iter().flat_map(u32::to_ne_bytes).collect::<Vec<_>>());
            assert!(reply.objects.is_empty());
        }
        for code in (0..=11).chain([9999]) {
            assert_unknown_transaction(unsafe { make_reply(&transaction(target, code, &[]), 1) });
        }
    }

    #[test]
    fn input_requires_installed_codes_and_rejects_unknown_or_malformed_requests() {
        fn request(args: &[i32]) -> Vec<u8> {
            let mut data = vec![0; 8];
            data.extend_from_slice(b"TSYS");
            data.extend_from_slice(&string16("android.hardware.input.IInputManager"));
            data.extend(args.iter().flat_map(|word| word.to_ne_bytes()));
            data
        }
        let _serial = CONTRACT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _restore = reset_table(&INPUT_TRANSACTION_CODES, None);
        let _device_restore = reset_table(&VIRTUAL_INPUT_DEVICE, 0x1234_5678u32.to_ne_bytes().to_vec());
        let target = handle_for("input");
        let data = request(&[-1]);
        for code in [0, 1, 2, 3, 4, 5, 6, 7, 22, 101, 203, 9999] {
            assert_unknown_transaction(unsafe { make_reply(&transaction(target, code, &data), 1) });
        }
        // API 37 has no isInputDeviceEnabled method; its zero slot is not a code.
        let codes = [101, 203, 0, 409, 503, 607, 709, 809];
        assert!(!unsafe { __pengudroid_binder_set_input_transactions(std::ptr::null(), 8) });
        for len in [0, 7, 9] {
            assert!(!unsafe { __pengudroid_binder_set_input_transactions(codes.as_ptr(), len) });
        }
        assert_eq!(*INPUT_TRANSACTION_CODES.lock().unwrap(), None);
        assert!(unsafe { __pengudroid_binder_set_input_transactions(codes.as_ptr(), codes.len()) });
        for i in 0..8 {
            for invalid in [0, 0x00ff_fffe, 0x00ff_ffff, 0x0100_0000] {
                if i == 2 && invalid == 0 { continue; }
                let mut bad = codes;
                bad[i] = invalid;
                assert!(!unsafe { __pengudroid_binder_set_input_transactions(bad.as_ptr(), bad.len()) });
                assert_eq!(*INPUT_TRANSACTION_CODES.lock().unwrap(), Some(codes));
            }
            for j in 0..i {
                let mut bad = codes;
                bad[i] = bad[j];
                assert!(!unsafe { __pengudroid_binder_set_input_transactions(bad.as_ptr(), bad.len()) });
                assert_eq!(*INPUT_TRANSACTION_CODES.lock().unwrap(), Some(codes));
            }
        }
        for (code, args, body) in [
            (101, vec![-1], vec![1, 0x1234_5678]),
            (101, vec![0], vec![0]),
            (203, vec![], vec![1, -1]),
            (409, vec![-1], vec![]),
            (503, vec![-1], vec![]),
            (607, vec![-1, 0x101, 3, 29, 67, 200, 3], vec![1, 3, 1, 1, 0]),
            (709, vec![-1, 29], vec![29]),
        ] {
            let data = request(&args);
            let reply = unsafe { make_reply(&transaction(target, code, &data), 1) };
            let mut expected: Vec<u8> = [0i32].into_iter().chain(body).flat_map(i32::to_ne_bytes).collect();
            expected.resize((expected.len() + 7) & !7, 0);
            assert_eq!(reply.flags, 0);
            assert_eq!(reply.parcel, expected);
            assert!(reply.objects.is_empty());
        }
        for code in [0, 1, 2, 3, 4, 5, 6, 7, 22, 9999] {
            assert_unknown_transaction(unsafe { make_reply(&transaction(target, code, &data), 1) });
        }
        for args in [vec![], vec![-1, 0, -1], vec![-1, 0, 769], vec![-1, 0, 1, 29]] {
            let data = request(&args);
            assert_unknown_transaction(unsafe { make_reply(&transaction(target, 607, &data), 1) });
        }
        for data in [vec![], service_request("not.IInputManager", "")] {
            assert_unknown_transaction(unsafe { make_reply(&transaction(target, 203, &data), 1) });
        }
    }

    #[test]
    fn content_provider_holder_wrapper_has_three_explicit_flags_at_both_alignments() {
        let _serial = CONTRACT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _codes_restore = reset_table(&APPLICATION_TRANSACTION_CODES, [u32::MAX; 5]);
        let _info_restore = reset_table(&PROVIDER_INFO, Vec::new());
        assert!(__pengudroid_binder_set_application_transactions(101, 203, 307, 409, 503));
        for info_words in [vec![0x1234_5678u32], vec![0x1234_5678, 0x8765_4321]] {
            // ProviderInfo is an opaque, framework-marshalled body here. Vary
            // its alignment so final padding cannot masquerade as a flag.
            *PROVIDER_INFO.lock().unwrap() = info_words.iter().flat_map(|word| word.to_ne_bytes()).collect();
            let reply = unsafe { make_reply(&transaction(handle_for("activity"), 203, &[]), 1) };
            let handle = handle_for("settings_provider");
            // Independent wrapper oracle: do not call any production writer.
            // ContentProviderHolder.java at frameworks/base 94b4c16 writes
            // info, provider, connection, noReleaseNeeded, mLocal, then
            // noReleaseNeededIfUnstable (three int32s, not alignment padding).
            let binder_words = [0x7368_2a85u32, 0x7f, handle, 0, 0, 0, 0x0c];
            let mut expected = vec![0u32, 1]; // AIDL status, readTypedObject presence
            expected.extend_from_slice(&info_words);
            let provider_at = (expected.len() * 4) as u64;
            expected.extend_from_slice(&binder_words);
            let connection_at = (expected.len() * 4) as u64;
            expected.extend_from_slice(&binder_words);
            expected.extend_from_slice(&[0, 0, 0]); // the three explicit false flags
            expected.resize((expected.len() + 1) & !1, 0); // transfer-area alignment
            let expected: Vec<u8> = expected.into_iter().flat_map(u32::to_ne_bytes).collect();
            assert_eq!(reply.flags, 0);
            assert_eq!(reply.objects, [provider_at, connection_at]);
            assert_eq!(reply.parcel, expected, "ProviderInfo words: {}", info_words.len());
            assert_eq!(reply.parcel.len(), if info_words.len() == 1 { 80 } else { 88 });
        }
    }

    /// `MediaSession.setQueue` passes `ISession.getBinderForSetQueue()`'s result
    /// straight to `ParcelableListBinder.send`, which calls `transact` on it with
    /// no null check: an empty reply is an NPE while the player is being built.
    #[test]
    fn media_session_requires_installation_and_scopes_codes_to_each_stub() {
        let _serial = CONTRACT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _restore = reset_table(&MEDIA_TRANSACTION_CODES, None);
        let names = ["media_session", "media_session_session", "media_session_controller",
            "media_session_queue", "media.metrics", "media_metrics"];
        for name in names {
            for code in [0, 1, 2, 12, 101, 307] {
                assert_unknown_transaction(unsafe { make_reply(&transaction(handle_for(name), code, &[]), 1) });
            }
        }
        // Reuse across ISessionManager/ISession/IMediaMetricsManager is legal.
        // Only ISession's two operations must use different codes.
        let codes = [101, 101, 307, 101];
        assert!(!unsafe { __pengudroid_binder_set_media_transactions(std::ptr::null(), 4) });
        for len in [0, 3, 5] {
            assert!(!unsafe { __pengudroid_binder_set_media_transactions(codes.as_ptr(), len) });
        }
        assert_eq!(*MEDIA_TRANSACTION_CODES.lock().unwrap(), None);
        assert!(unsafe { __pengudroid_binder_set_media_transactions(codes.as_ptr(), codes.len()) });
        for i in 0..4 {
            for invalid in [0, 0x00ff_fffe, 0x00ff_ffff, 0x0100_0000] {
                let mut bad = codes;
                bad[i] = invalid;
                assert!(!unsafe { __pengudroid_binder_set_media_transactions(bad.as_ptr(), bad.len()) });
                assert_eq!(*MEDIA_TRANSACTION_CODES.lock().unwrap(), Some(codes));
            }
        }
        let collision = [101, 307, 307, 101];
        assert!(!unsafe { __pengudroid_binder_set_media_transactions(collision.as_ptr(), collision.len()) });
        assert_eq!(*MEDIA_TRANSACTION_CODES.lock().unwrap(), Some(codes));
        for (name, code, result) in [
            ("media_session", 101, "media_session_session"),
            ("media_session_session", 101, "media_session_controller"),
            ("media_session_session", 307, "media_session_queue"),
        ] {
            assert_handle_reply(unsafe { make_reply(&transaction(handle_for(name), code, &[]), 1) }, result);
        }
        for name in names {
            for code in [0, 1, 2, 12, 9999] {
                assert_unknown_transaction(unsafe { make_reply(&transaction(handle_for(name), code, &[]), 1) });
            }
        }
        // A session code does not select a method on another Stub, and returned
        // controller/queue handles do not claim to implement their protocols.
        for (name, code) in [("media_session", 307), ("media.metrics", 307),
            ("media_session_controller", 101), ("media_session_queue", 101)] {
            assert_unknown_transaction(unsafe { make_reply(&transaction(handle_for(name), code, &[]), 1) });
        }
    }

    /// `MediaMetricsManager.createPlaybackSession` turns the reply to
    /// `getPlaybackSessionId` into a `PlaybackSession`, whose `@NonNull`
    /// validation throws an NPE on a null id — and ExoPlayer 2.18 builds such a
    /// listener on API 31+, so an empty reply kills `Player.initPlayback`.
    ///
    /// Both names have to be recognised: clients request the service through
    /// `Context.MEDIA_METRICS_SERVICE` (`media_metrics`) while the service
    /// manager knows it as `media.metrics`.
    #[test]
    fn media_metrics_answers_a_session_id_under_both_names() {
        let _serial = CONTRACT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _restore = reset_table(&MEDIA_TRANSACTION_CODES, None);
        let codes = [101, 101, 307, 101];
        assert!(unsafe { __pengudroid_binder_set_media_transactions(codes.as_ptr(), codes.len()) });
        for name in ["media_metrics", "media.metrics"] {
            let tr = TransactionData {
                target_handle: handle_for(name),
                code: codes[3],
                ..TransactionData::default()
            };
            // SAFETY: `make_reply` only reads the transaction descriptor; this
            // one carries no parcel payload.
            let reply = unsafe { make_reply(&tr, 1) };
            assert_eq!(reply.flags, 0);
            assert_eq!(&reply.parcel[0..4], &0u32.to_ne_bytes(), "status word");
            let len =
                u32::from_ne_bytes(reply.parcel[4..8].try_into().unwrap()) as usize;
            assert!(len > 0, "{name}: the session id must not be null");
            let text: Vec<u16> = reply.parcel[8..8 + len * 2]
                .chunks(2)
                .map(|c| u16::from_ne_bytes([c[0], c[1]]))
                .collect();
            let id = String::from_utf16(&text).unwrap();
            let suffix = id
                .strip_prefix("pengudroid-playback-")
                .unwrap_or_else(|| panic!("{name}: unexpected id {id:?}"));
            assert!(suffix.parse::<u32>().is_ok(), "{name}: {id:?}");
        }
    }

    #[test]
    fn audio_focus_requires_installation_and_never_dispatches_old_defaults() {
        let _serial = CONTRACT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _restore = reset_table(&AUDIO_FOCUS_TRANSACTIONS, None);
        let target = handle_for("audio");
        AUDIO_FOCUS.abandon();
        for code in [0, 66, 67, 68, 69, 101, 203, 307, 409] {
            assert_unknown_transaction(unsafe { make_reply(&transaction(target, code, &[]), 1) });
        }
        assert!(__pengudroid_binder_set_audio_focus_transactions(101, 203, 307, 409));
        assert!(!__pengudroid_binder_set_audio_focus_transactions(0, 203, 307, 409));
        assert!(!__pengudroid_binder_set_audio_focus_transactions(101, 101, 307, 409));
        AUDIO_FOCUS.request(3);
        for code in [0, 66, 67, 68, 69, 9999] {
            assert_unknown_transaction(unsafe { make_reply(&transaction(target, code, &[]), 1) });
            assert_eq!(AUDIO_FOCUS.current(), 3);
        }
        let reply = unsafe { make_reply(&transaction(target, 409, &[]), 1) };
        assert_eq!(reply.flags, 0);
        assert_eq!(&reply.parcel[..4], &0u32.to_ne_bytes());
        assert_eq!(&reply.parcel[4..8], &3i32.to_ne_bytes());
        let reply = unsafe { make_reply(&transaction(target, 203, &[]), 1) };
        assert_eq!(reply.flags, 0);
        assert_eq!(&reply.parcel[4..8], &1i32.to_ne_bytes());
        assert_eq!(AUDIO_FOCUS.current(), 0);
        AUDIO_FOCUS.request(1);
        let reply = unsafe { make_reply(&transaction(target, 307, &[]), 1) };
        assert_eq!(reply.flags, 0);
        assert_eq!(AUDIO_FOCUS.current(), 0);
        // Missing request arguments deny focus; they do not invent a gain.
        let reply = unsafe { make_reply(&transaction(target, 101, &[]), 1) };
        assert_eq!(reply.flags, 0);
        assert_eq!(&reply.parcel[4..8], &0i32.to_ne_bytes());
    }

    /// `Settings.NameValueCache` reads `Bundle.getString("value")` from the
    /// provider's `call` reply, so the reply has to be the Bundle installed for
    /// *that* key — an empty one makes every setting read return null.
    #[test]
    fn settings_answer_the_requested_key() {
        let key = "android_id";
        let value = vec![0xde, 0xad, 0xbe, 0xef];
        // SAFETY: both slices outlive the call.
        unsafe {
            __pengudroid_binder_set_setting(
                key.as_ptr(),
                key.len(),
                value.as_ptr(),
                value.len(),
            );
        }
        let request = |values: &[&str]| {
            let mut data = Vec::new();
            for value in values {
                let units: Vec<u16> = value.encode_utf16().collect();
                data.extend_from_slice(&(units.len() as u32).to_le_bytes());
                data.extend(units.iter().flat_map(|u| u.to_le_bytes()));
                data.extend_from_slice(&[0, 0]);
                while data.len() % 4 != 0 { data.push(0); }
            }
            data
        };
        let data = request(&["GET_secure", key]);
        let mut parcel = Vec::new();
        let mut objects = Vec::new();
        provider_reply(ICP_CALL, Some(&data), &mut parcel, &mut objects);
        assert_eq!(parcel, value);

        // A list prefix mentioning a known key must not select its scalar reply.
        let data = request(&["LIST_config", "android_id"]);
        let mut parcel = Vec::new();
        provider_reply(ICP_CALL, Some(&data), &mut parcel, &mut objects);
        assert_eq!(parcel, (-1i32).to_ne_bytes());

        // A key nobody installed must not borrow another key's Bundle.
        let data = request(&["GET_secure", "prefix_android_id_suffix"]);
        let mut parcel = Vec::new();
        provider_reply(ICP_CALL, Some(&data), &mut parcel, &mut objects);
        assert_ne!(parcel, value);
    }

    #[test]
    fn codec2_failed_dispatch_never_synthesizes_a_reply_and_preserves_native_bytes() {
        for code in [1, 2, 3, 4, 5] {
            let tr = transaction(handle_for("codec2"), code, &[]);
            assert_unknown_transaction(codec2_dispatch_reply(&tr, None));
        }
        // An opaque captured native reply must not acquire a fabricated status,
        // codec entry, binder handle or padding from this broker.
        let parcel = vec![0x11, 0x22, 0x33, 0x44, 0x55];
        let offsets = vec![4];
        let tr = transaction(handle_for("codec2"), 3, &[]);
        let reply = codec2_dispatch_reply(&tr, Some((parcel.clone(), offsets.clone())));
        assert_eq!(reply.flags, 0);
        assert_eq!(reply.parcel, parcel);
        assert_eq!(reply.objects, offsets);
    }

    #[test]
    fn codec2_unavailable_never_advertises_or_creates_placeholder_decoders() {
        const CHILD: &str = "PENGUDROID_BINDER_TEST_CODEC2_ABSENT";
        if std::env::var_os(CHILD).is_none() {
            // Isolate the opt-out from parallel tests. It short-circuits the
            // native loader: no Android service, framework or runtime executes.
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "binder::tests::codec2_unavailable_never_advertises_or_creates_placeholder_decoders", "--nocapture"])
                .env(CHILD, "1")
                .env("PENGUDROID_NO_CODEC2_SERVICE", "1")
                .env_remove("PENGUDROID_ROOT")
                .output().unwrap();
            assert!(output.status.success(), "{}\n{}",
                String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
            return;
        }
        assert!(!crate::codec2::available());
        let target = handle_for("codec2");
        let request = b"android.hardware.media.c2@1.0::IComponentStore\0";
        for code in [1, 2, 3, 4, 5, 9999, 0x0f43_484e] {
            for data in [&[][..], request.as_slice()] {
                assert_unknown_transaction(unsafe { make_reply(&transaction(target, code, data), 1) });
            }
        }
        assert!(!SERVICE_HANDLES.lock().unwrap().contains_key("codec2_component"));

        // Absence is a successful, typed discovery result, not a transport
        // error: Codec2Client retries a failed manifest query in a tight loop.
        // OMX and unknown interfaces must not receive fabricated handles either.
        for fq in [HIDL_COMPONENT_STORE, "android.hardware.media.omx@1.0::IOmxStore",
            "unsupported.media.c2@9.0::ICodec2"] {
            let transport = hidl_reply(0, HIDL_GET_TRANSPORT, HIDL_MANAGER_CHAIN[2], &[fq, "default"]);
            assert_eq!(transport.flags, 0);
            assert!(transport.objects.is_empty());
            assert_eq!(transport.parcel, [0; 8]); // status + padded Transport::EMPTY
            let get = hidl_reply(0, HIDL_GET, HIDL_MANAGER_CHAIN[2], &[fq, "default"]);
            assert_eq!(get.flags, 0);
            assert!(get.objects.is_empty());
            assert_eq!(get.parcel.len(), 4 + 24); // no AIDL stability word
            assert_eq!(&get.parcel[..4], &[0; 4]);
            assert_eq!(&get.parcel[4..8], &BINDER_TYPE_BINDER.to_ne_bytes());
            assert_eq!(&get.parcel[8..], &[0; 20]);
            for (code, token) in [(HIDL_LIST_BY_INTERFACE, HIDL_MANAGER_CHAIN[2]),
                (HIDL_LIST_MANIFEST_BY_INTERFACE, HIDL_MANAGER_CHAIN[0])] {
                assert!(decode_hidl_vec(&hidl_reply(0, code, token, &[fq])).is_empty());
            }
            assert!(!SERVICE_HANDLES.lock().unwrap().contains_key(fq));
        }
        assert_eq!(decode_hidl_vec(&hidl_reply(0, HIDL_INTERFACE_CHAIN, HIDL_BASE, &[])),
            HIDL_MANAGER_CHAIN);
    }

    fn hidl_reply(target: u32, code: u32, token: &str, args: &[&str]) -> Reply {
        let backing = HidlStrings::new(args);
        let mut data = token.as_bytes().to_vec();
        data.push(0);
        while data.len() % 4 != 0 { data.push(0); }
        let mut offsets = Vec::new();
        for (index, bytes) in backing.strings.iter().enumerate() {
            let parent = push_buffer_object(&mut data, &mut offsets, 0,
                &backing.elements[index] as *const HidlHeader as u64, 16, 0, 0);
            push_buffer_object(&mut data, &mut offsets, 1, bytes.as_ptr() as u64,
                bytes.len() as u64, parent, 0);
        }
        let mut tr = transaction(target, code, &data);
        tr.offsets_size = (offsets.len() * 8) as u64;
        tr.offsets_ptr = offsets.as_ptr() as u64;
        unsafe { make_reply(&tr, 1) }
    }

    fn hidl_buffer(reply: &Reply, record: usize) -> (u64, u64, u64, u64) {
        assert_eq!(reply.flags, 0);
        assert_eq!(&reply.parcel[..4], &[0; 4]);
        let at = 4 + record * 40;
        let bytes = &reply.parcel[at..at + 40];
        assert_eq!(&bytes[..4], &0x7074_2a85u32.to_ne_bytes());
        assert_eq!(&bytes[4..8], &u32::from(record != 0).to_ne_bytes());
        let word = |offset| u64::from_ne_bytes(bytes[offset..offset + 8].try_into().unwrap());
        let data = word(8);
        if data != 0 { assert!(reply.objects.contains(&(at as u64))); }
        else { assert!(!reply.objects.contains(&(at as u64))); }
        (data, word(16), word(24), word(32))
    }

    fn decode_hidl_vec(reply: &Reply) -> Vec<String> {
        let (root, length, _, _) = hidl_buffer(reply, 0);
        assert_ne!(root, 0);
        assert_eq!(length, 16);
        let header = unsafe { &*(root as *const HidlHeader) };
        assert_eq!(header.owns_buffer, 0);
        assert!(header.size <= 4);
        let count = header.size as usize;
        let (array, length, parent, offset) = hidl_buffer(reply, 1);
        assert_eq!((array, length, parent, offset), (header.data, (count * 16) as u64, 0, 0));
        assert_eq!(reply.parcel.len(), 4 + (2 + count) * 40);
        if count == 0 {
            assert_eq!(array, 0);
            assert_eq!(reply.objects, [4]);
            return Vec::new();
        }
        assert_eq!(reply.objects.len(), count + 2);
        let elements = unsafe { std::slice::from_raw_parts(array as *const HidlHeader, count) };
        elements.iter().enumerate().map(|(index, element)| {
            let (ptr, length, parent, offset) = hidl_buffer(reply, index + 2);
            assert_eq!((ptr, length, parent, offset),
                (element.data, element.size as u64 + 1, 1, (index * 16) as u64));
            assert_eq!(element.owns_buffer, 0);
            let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, length as usize) };
            assert_eq!(bytes.last(), Some(&0));
            std::str::from_utf8(&bytes[..bytes.len() - 1]).unwrap().to_owned()
        }).collect()
    }

    #[test]
    fn hidl_manager_self_lookup_routes_every_version_to_the_same_dispatcher() {
        let mut target = None;
        for version in &HIDL_MANAGER_CHAIN[..3] {
            let reply = hidl_reply(0, HIDL_GET, HIDL_MANAGER_CHAIN[2], &[version, "default"]);
            assert_eq!(reply.flags, 0);
            assert_eq!(reply.parcel.len(), 28); // status + HIDL binder, no stability
            assert_eq!(reply.objects, [4]);
            assert_eq!(&reply.parcel[4..8], &BINDER_TYPE_HANDLE.to_ne_bytes());
            let handle = u32::from_ne_bytes(reply.parcel[12..16].try_into().unwrap());
            assert_ne!(handle, 0);
            assert_eq!(*target.get_or_insert(handle), handle);
            assert_eq!(name_for_handle(handle).as_deref(), Some(HIDL_MANAGER_HANDLE));
            for at in [0, handle] {
                let chain = hidl_reply(at, HIDL_INTERFACE_CHAIN, HIDL_BASE, &[]);
                assert_eq!(decode_hidl_vec(&chain), HIDL_MANAGER_CHAIN);
                let ping = hidl_reply(at, HIDL_PING, HIDL_BASE, &[]);
                assert_eq!(ping.parcel, [0; 4]);
                assert!(ping.objects.is_empty());
                let transport = hidl_reply(at, HIDL_GET_TRANSPORT, HIDL_MANAGER_CHAIN[2],
                    &[version, "default"]);
                assert_eq!(transport.parcel, [0, 0, 0, 0, 1, 0, 0, 0]);
                let registered = hidl_reply(at, HIDL_LIST_BY_INTERFACE, HIDL_MANAGER_CHAIN[2], &[version]);
                assert_eq!(decode_hidl_vec(&registered), ["default"]);
                let declared = hidl_reply(at, HIDL_LIST_MANIFEST_BY_INTERFACE, HIDL_MANAGER_CHAIN[0], &[version]);
                assert_eq!(decode_hidl_vec(&declared), ["default"]);
            }
        }
    }

    #[test]
    fn hidl_descriptor_is_a_string_not_a_vector_or_aidl_descriptor() {
        let reply = hidl_reply(0, HIDL_INTERFACE_DESCRIPTOR, HIDL_BASE, &[]);
        let (header, length, _, _) = hidl_buffer(&reply, 0);
        assert_eq!(length, 16);
        let header = unsafe { &*(header as *const HidlHeader) };
        let (data, length, parent, offset) = hidl_buffer(&reply, 1);
        assert_eq!((data, length, parent, offset), (header.data, header.size as u64 + 1, 0, 0));
        let bytes = unsafe { std::slice::from_raw_parts(data as *const u8, length as usize) };
        assert_eq!(&bytes[..bytes.len() - 1], HIDL_MANAGER_CHAIN[0].as_bytes());
        assert_eq!(bytes.last(), Some(&0));
        assert_eq!(reply.objects, [4, 44]);
        assert_unknown_transaction(hidl_reply(0, INTERFACE_TRANSACTION, HIDL_BASE, &[]));
    }

    #[test]
    fn hidl_discovery_rejects_wrong_tokens_and_malformed_arguments() {
        for code in [HIDL_GET, HIDL_GET_TRANSPORT, HIDL_LIST_BY_INTERFACE,
            HIDL_LIST_MANIFEST_BY_INTERFACE, HIDL_INTERFACE_CHAIN, HIDL_PING] {
            assert_unknown_transaction(hidl_reply(0, code, "wrong.interface", &[]));
        }
        assert_unknown_transaction(hidl_reply(0, HIDL_GET, HIDL_MANAGER_CHAIN[2], &[]));
        assert_unknown_transaction(hidl_reply(0, HIDL_GET, HIDL_MANAGER_CHAIN[2], &[HIDL_MANAGER_CHAIN[0]]));
        assert_unknown_transaction(hidl_reply(0, HIDL_LIST_MANIFEST_BY_INTERFACE, HIDL_MANAGER_CHAIN[2],
            &[HIDL_MANAGER_CHAIN[0]]));
        for instance in ["", "missing", "default_extra"] {
            let reply = hidl_reply(0, HIDL_GET_TRANSPORT, HIDL_MANAGER_CHAIN[2],
                &[HIDL_MANAGER_CHAIN[0], instance]);
            assert_eq!(reply.parcel, [0; 8]);
        }
        assert_unknown_transaction(hidl_reply(0, 9999, HIDL_MANAGER_CHAIN[2], &[]));
    }

    #[test]
    fn hidl_manager_repeated_queries_reuse_finite_backing_storage() {
        for (code, token, args) in [
            (HIDL_INTERFACE_CHAIN, HIDL_BASE, &[][..]),
            (HIDL_INTERFACE_DESCRIPTOR, HIDL_BASE, &[][..]),
            (HIDL_LIST_MANIFEST_BY_INTERFACE, HIDL_MANAGER_CHAIN[0], &["unknown.interface"][..]),
            (HIDL_LIST_MANIFEST_BY_INTERFACE, HIDL_MANAGER_CHAIN[0], &HIDL_MANAGER_CHAIN[..1]),
        ] {
            let baseline = hidl_reply(0, code, token, args);
            for _ in 0..1024 {
                let reply = hidl_reply(0, code, token, args);
                // Identical embedded addresses as well as identical payloads:
                // no freshly leaked header/array/strings for every query.
                assert_eq!(reply.parcel, baseline.parcel);
                assert_eq!(reply.objects, baseline.objects);
            }
        }
    }

    #[test]
    fn unknown_transaction_is_a_transport_status_not_an_aidl_exception() {
        let _serial = CONTRACT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let data = service_request("android.gui.ISurfaceComposer", "");
        let tr = transaction(handle_for("SurfaceFlingerAIDL"), 9999, &data);
        let mut write = Vec::new();
        push_u32(&mut write, BC_TRANSACTION);
        push_transaction(&mut write, &tr);
        let mut read = vec![0u8; 256];
        let mut mapping = vec![0u8; MAP_SIZE as usize];
        let old_base = MAP_BASE.swap(mapping.as_mut_ptr() as usize, Ordering::Relaxed);
        let old_len = MAP_LEN.swap(mapping.len(), Ordering::Relaxed);
        let mut wr = WriteRead {
            write_size: write.len() as u64,
            write_buffer: write.as_ptr() as u64,
            read_size: read.len() as u64,
            read_buffer: read.as_mut_ptr() as u64,
            ..WriteRead::default()
        };
        let rc = unsafe { handle_write_read(&mut wr) };
        let response = unsafe { transaction_at(read.as_ptr() as u64, 8) };
        let status = response.data_ptr.checked_sub(mapping.as_ptr() as u64)
            .and_then(|at| mapping.get(at as usize..at as usize + 4))
            .map(|bytes| i32::from_ne_bytes(bytes.try_into().unwrap()));
        MAP_BASE.store(old_base, Ordering::Relaxed);
        MAP_LEN.store(old_len, Ordering::Relaxed);
        assert_eq!(rc, 0);
        assert_eq!(wr.write_consumed, write.len() as u64);
        assert_eq!(wr.read_consumed, 8 + std::mem::size_of::<TransactionData>() as u64);
        assert_eq!(&read[..4], &BR_TRANSACTION_COMPLETE.to_ne_bytes());
        assert_eq!(&read[4..8], &BR_REPLY.to_ne_bytes());
        assert_eq!(response.flags, TF_STATUS_CODE);
        assert_eq!(response.data_size, 4);
        assert_eq!(response.offsets_size, 0);
        assert_eq!(status, Some(UNKNOWN_TRANSACTION));
    }

    /// A one-way transaction must still be acknowledged.
    ///
    /// libbinder calls `waitForResponse(nullptr, nullptr)` for `TF_ONE_WAY` and
    /// finishes on `BR_TRANSACTION_COMPLETE`; emitting nothing leaves it looping
    /// in `talkWithDriver` forever.
    #[test]
    fn one_way_transactions_get_only_a_completion() {
        let mut write = Vec::new();
        write.extend_from_slice(&BC_TRANSACTION.to_ne_bytes());
        let tr = TransactionData {
            flags: TF_ONE_WAY,
            ..TransactionData::default()
        };
        // SAFETY: `TransactionData` is plain old data.
        let bytes = unsafe {
            std::slice::from_raw_parts(
                &tr as *const TransactionData as *const u8,
                std::mem::size_of::<TransactionData>(),
            )
        };
        write.extend_from_slice(bytes);

        let mut read = vec![0u8; 256];
        let mut wr = WriteRead {
            write_size: write.len() as u64,
            write_buffer: write.as_ptr() as u64,
            read_size: read.len() as u64,
            read_buffer: read.as_mut_ptr() as u64,
            ..WriteRead::default()
        };
        // SAFETY: both buffers outlive the call and are sized as declared.
        let rc = unsafe { handle_write_read(&mut wr) };
        assert_eq!(rc, 0);
        assert_eq!(wr.write_consumed, write.len() as u64);
        assert_eq!(wr.read_consumed, 4, "only the completion command");
        let cmd = u32::from_ne_bytes(read[0..4].try_into().unwrap());
        assert_eq!(cmd, BR_TRANSACTION_COMPLETE);
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

/// `android::hardware::Parcel::readEmbeddedBuffer(size, handle, parentHandle,
/// parentOffset, out)`.
///
/// The non-nullable twin of `readNullableEmbeddedBuffer`; it has its own
/// implementation, so it needs its own interposer to trace the reads.
#[no_mangle]
pub unsafe extern "C" fn _ZNK7android8hardware6Parcel18readEmbeddedBufferEmPmmmPPKv(
    this: *mut c_void,
    size: usize,
    handle: *mut usize,
    parent_handle: usize,
    parent_offset: usize,
    out: *mut *const c_void,
) -> i32 {
    type F = unsafe extern "C" fn(*mut c_void, usize, *mut usize, usize, usize, *mut *const c_void) -> i32;
    let next: Option<F> = crate::rootfs::next(b"_ZNK7android8hardware6Parcel18readEmbeddedBufferEmPmmmPPKv\0");
    let rc = match next {
        Some(f) => f(this, size, handle, parent_handle, parent_offset, out),
        None => -22,
    };
    if trace_enabled() {
        diag(&format!(
            "[binder]   readEmbeddedBuffer size={size} parent={parent_handle} parentOffset={parent_offset} rc={rc}\n"
        ));
    }
    rc
}

/// `android::hardware::Parcel::readNullableEmbeddedBuffer(size, handle,
/// parentOffset, offset, out)`.
///
/// Interposed only to trace the size the HIDL reader expects for each embedded
/// object; `PENGUDROID_TRACE_BINDER=1` prints it.
#[no_mangle]
pub unsafe extern "C" fn _ZNK7android8hardware6Parcel26readNullableEmbeddedBufferEmPmmmPPKv(
    this: *mut c_void,
    size: usize,
    handle: *mut usize,
    parent_offset: usize,
    offset: usize,
    out: *mut *const c_void,
) -> i32 {
    type F = unsafe extern "C" fn(*mut c_void, usize, *mut usize, usize, usize, *mut *const c_void) -> i32;
    let next: Option<F> = crate::rootfs::next(b"_ZNK7android8hardware6Parcel26readNullableEmbeddedBufferEmPmmmPPKv\0");
    let rc = match next {
        Some(f) => f(this, size, handle, parent_offset, offset, out),
        None => -22,
    };
    if trace_enabled() {
        diag(&format!(
            "[binder]   readNullableEmbeddedBuffer size={size} parentOffset={parent_offset} offset={offset} rc={rc}\n"
        ));
    }
    rc
}
