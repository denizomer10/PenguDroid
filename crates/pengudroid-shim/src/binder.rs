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
const BC_ACQUIRE: u32 = 0x4004_6302;
const BC_RELEASE: u32 = 0x4004_6303;
const BC_DECREFS: u32 = 0x4004_6304;
const BC_INCREFS: u32 = 0x4004_6305;
const BC_ACQUIRE_DONE: u32 = 0x4008_6306;
const BC_RELEASE_DONE: u32 = 0x4008_6307;
const BC_INCREFS_DONE: u32 = 0x4008_6308;
const BC_FREE_BUFFER: u32 = 0x4008_6309;
const BC_ATTEMPT_ACQUIRE: u32 = 0x4004_630b;
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

/// Transaction flag: no reply is expected.
const TF_ONE_WAY: u32 = 0x1;

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

/// Write a `hidl_string` the way libhidlbase does.
///
/// Measured from a live `getTransport` request: a string is a 40-byte struct
/// `[u32 tag][u32 heap][u64 data][u32 size][20 bytes inline]` followed by the
/// string bytes **inline in the parcel** (that is what `writeEmbeddedToParcel`
/// does). The reader takes the size from the struct and then reads `size + 1`
/// bytes in place, so the pointer field is irrelevant.
fn push_hidl_string(parcel: &mut Vec<u8>, s: &str) {
    const TAG: u32 = 0x7074_2a85;
    parcel.extend_from_slice(&TAG.to_ne_bytes());
    parcel.extend_from_slice(&1u32.to_ne_bytes()); // heap
    parcel.extend_from_slice(&0u64.to_ne_bytes()); // data pointer: unused
    parcel.extend_from_slice(&(s.len() as u32).to_ne_bytes());
    parcel.extend_from_slice(&[0u8; 20]);
    parcel.extend_from_slice(s.as_bytes());
    parcel.push(0);
    while parcel.len() % 4 != 0 {
        parcel.push(0);
    }
}

/// Read a `hidl_string` from `data` at `*pos`.
fn read_hidl_string(data: &[u8], pos: &mut usize) -> Option<String> {
    if *pos + 4 > data.len() {
        return None;
    }
    let len = u32::from_ne_bytes(data[*pos..*pos + 4].try_into().ok()?) as usize;
    *pos += 4;
    if *pos + len > data.len() {
        return None;
    }
    let s = String::from_utf8_lossy(&data[*pos..*pos + len]).into_owned();
    *pos += len;
    // Skip the NUL and the padding to the next four-byte boundary.
    *pos += 1;
    while *pos % 4 != 0 {
        *pos += 1;
    }
    Some(s)
}

/// Answer the HIDL calls that reach the hardware service manager (handle 0).
///
/// `libhidlbase` first pings the manager and then asks for its interface chain;
/// both have to succeed before `defaultServiceManager1_1()` returns anything,
/// and without it `Codec2Client` aborts with "Hardware service manager is not
/// running".
unsafe fn hidl_service_manager_reply(
    code: u32,
    parcel: &mut Vec<u8>,
    objects: &mut Vec<u64>,
    tr: &TransactionData,
) {
    if trace_enabled() && tr.data_size > 0 && tr.data_ptr != 0 {
        let n = (tr.data_size as usize).min(256);
        let data = std::slice::from_raw_parts(tr.data_ptr as *const u8, n);
        // The parcel starts with the interface token (a NUL-terminated string
        // padded to four bytes), then 40-byte `hidl_string` blocks:
        // `[u32 hash][u32 flag][u64 data][u32 size][20 bytes inline]`.
        let mut off = 0usize;
        while off < data.len() && data[off] != 0 {
            off += 1;
        }
        off = (off + 1 + 3) & !3;
        let mut line = format!("[binder]   hidl args from {off}:\n");
        while off + 40 <= data.len() {
            let block = &data[off..off + 40];
            let flag = u32::from_ne_bytes(block[4..8].try_into().unwrap());
            let ptr = u64::from_ne_bytes(block[8..16].try_into().unwrap()) as usize;
            let size = u32::from_ne_bytes(block[16..20].try_into().unwrap()) as usize;
            let text = if ptr != 0 && size > 0 && size < 256 {
                let bytes = std::slice::from_raw_parts(ptr as *const u8, size);
                String::from_utf8_lossy(bytes).into_owned()
            } else {
                String::new()
            };
            line.push_str(&format!("      flag={flag} size={size} text={text:?}\n"));
            off += 40;
        }
        diag(&line);
    }
    match code {
        PING_TRANSACTION => {
            // `make_reply` already wrote the status word for this handle.
        }
        // `IServiceManager::getTransport(fqName, name)` returns a `Transport`
        // enum, which HIDL serialises as a single byte (`readUint8`).
        3 => {
            parcel.push(1); // Transport::HWBINDER (EMPTY is 0)
            if trace_enabled() {
                diag("[binder]   -> hidl getTransport: hwbinder\n");
            }
        }
        INTERFACE_TRANSACTION => {
            // `make_reply` already wrote the status word for this handle.
            let chain = [
                "android.hidl.manager@1.0::IServiceManager",
                "android.hidl.manager@1.1::IServiceManager",
                "android.hidl.base@1.0::IBase",
            ];
            parcel.extend_from_slice(&(chain.len() as u32).to_ne_bytes());
            for name in chain {
                push_hidl_string(parcel, name);
            }
        }
        // `IServiceManager::get(fqName, name)`.
        1 => {
            let fq = hidl_arg_string(tr, 0).unwrap_or_default();
            // `make_reply` already wrote the status word for this handle.
            let handle = if fq.contains("media.c2") || fq.contains("ICodec2") {
                handle_for("codec2")
            } else {
                handle_for(fq.as_str())
            };
            push_flat_handle(parcel, objects, handle);
            if trace_enabled() {
                diag(&format!("[binder]   -> hidl get({fq}) handle {handle}\n"));
            }
        }
        // `IServiceManager::listManifestByInterface(fqName)` (code 5 on the
        // wire; HIDL 1.2 numbers it 13).
        //
        // `Codec2Client::GetServiceNames()` calls this with
        // `IComponentStore::descriptor` and expects the *instance names* back
        // (`vec<hidl_string>`), not `InstanceDebugInfo` records.
        5 | 13 => {
            // `make_reply` already wrote the status word for this handle.
            // `hidl_vec<hidl_string>`: a 16-byte vec struct, a 16-byte element
            // array, and the string bytes — each one a `binder_buffer_object`
            // recorded in the offsets array.
            // `verifyBufferObject` requires `flags == nullable` (always 1 here)
            // and an exact `length`, and it checks that the parent's buffer
            // holds the child's pointer at `parentOffset`.
            let (data_ptr, data_len) = leak_cstr("default");
            let mut arr = Vec::with_capacity(16);
            arr.extend_from_slice(&data_ptr.to_ne_bytes()); // mData
            arr.extend_from_slice(&(data_len - 1).to_ne_bytes()); // mSize
            let arr: &'static [u8] = Box::leak(arr.into_boxed_slice());

            let mut vec = Vec::with_capacity(16);
            vec.extend_from_slice(&(arr.as_ptr() as u64).to_ne_bytes()); // mData
            vec.extend_from_slice(&1u32.to_ne_bytes()); // mSize
            vec.extend_from_slice(&0u32.to_ne_bytes()); // padding
            let vec: &'static [u8] = Box::leak(vec.into_boxed_slice());

            // `readBuffer` verifies `flags == 0`; the embedded reads verify 1.
            let vec_idx = push_buffer_object(parcel, objects, 0, vec.as_ptr() as u64, 16, 0, 0);
            let arr_idx =
                push_buffer_object(parcel, objects, 1, arr.as_ptr() as u64, 16, vec_idx, 0);
            push_buffer_object(parcel, objects, 1, data_ptr, data_len, arr_idx, 0);
            if trace_enabled() {
                diag(&format!("[binder]   -> hidl listManifestByInterface (code {code}): [default]\n"));
            }
        }
        _ => {
            // `make_reply` already wrote the status word for this handle.
        }
    }
    if trace_enabled() {
        diag(&format!("[binder]   -> hidl manager code {code:#x}\n"));
    }
}

/// `android.os.IServiceManager` transaction codes.
const ISM_GET_SERVICE: u32 = 1;
const ISM_CHECK_SERVICE: u32 = 2;

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
const ATM_GET_ACTIVITY_CLIENT_CONTROLLER: u32 = 18;

/// `IActivityManager` transaction code for `getContentProvider`.
///
/// Arguments are `(IApplicationThread, String opPackageName, String auth,
/// int userId, boolean stable)`; `Activity.attach` needs the `settings`
/// provider and NPEs on a null `IContentProvider` without it.
const IAM_GET_CONTENT_PROVIDER: u32 = 23;

/// `IActivityManager` transaction code for `startService` / `startForegroundService`.
///
/// Measured from a live transaction: the parcel carries the app package and
/// `org.schabi.newpipe.player.PlayerService` (plus the
/// `should_start_foreground_extra` key for the foreground variant).
const IAM_START_SERVICE: u32 = 27;

/// `IActivityManager` transaction code for `bindService`/`bindIsolatedService`.
///
/// Same component as [`IAM_START_SERVICE`] but without the intent extras.
const IAM_BIND_SERVICE: u32 = 30;

/// `ISessionManager` transaction code for `createSession`.
///
/// From `android-13.0.0_r1`'s `ISessionManager.aidl`: `createSession` is the
/// first method, so it is `FIRST_CALL_TRANSACTION` (1).
const ISM_CREATE_SESSION: u32 = 1;

/// `ISession` transaction code for `getController`.
///
/// Second method in `ISession.aidl` (`sendEvent` comes first).
const ISESSION_GET_CONTROLLER: u32 = 2;

/// `ISession` transaction code for `getBinderForSetQueue`.
///
/// Twelfth method in `ISession.aidl` (`sendEvent`, `getController`, then nine
/// setters). `MediaSession.setQueue` calls `ParcelableListBinder.send` on the
/// binder it returns, and that helper dereferences it without a null check.
const ISESSION_GET_BINDER_FOR_SET_QUEUE: u32 = 12;

/// `IMediaMetricsManager` transaction code for `getPlaybackSessionId(int)`.
///
/// Second method in `android-13.0.0_r1`'s `IMediaMetricsManager.aidl`
/// (`reportPlaybackMetrics` comes first).
const IMM_GET_PLAYBACK_SESSION_ID: u32 = 2;

/// `IUserManager` transaction code for `isUserUnlocked(int)`.
const IUM_IS_USER_UNLOCKED: u32 = 79;

/// `IUserManager` transaction code for `isUserUnlockingOrUnlocked(int)`.
///
/// `ContextImpl.getSharedPreferences` refuses credential-encrypted storage
/// until this reports true, which is what NewPipe hits on `onCreate`.
const IUM_IS_USER_UNLOCKING_OR_UNLOCKED: u32 = 71;

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

/// `ISurfaceComposer` transaction code for `createConnection`.
///
/// Like `createDisplayEventConnection` this is a hand-written interface, so the
/// reply is a bare `strongBinder` with no status word.
const ISC_CREATE_CONNECTION: u32 = 2;

/// Legacy `ISurfaceComposer::authenticateSurfaceTexture(IGraphicBufferConsumer)`.
///
/// Measured, not guessed: `TextureView` triggers it right after
/// `SurfaceUtils: connecting to surface … (reconnect)`, and the transaction is
/// `code=9` on the `android.ui.ISurfaceComposer` handle. Code 8 is
/// `setTransactionState` (it also carries binders, which is what made 8 look
/// plausible at first).
const ISC_AUTHENTICATE_SURFACE: u32 = 9;

/// `ISurfaceComposerClient` transaction code for `createSurface`.
///
/// `ISurfaceComposerClient` is hand-written too, and `callRemote` reads the out
/// parameters *first* and the status word *last*:
/// `[handle][gbp][layerId][transformHint][status]`.
const ISCC_CREATE_SURFACE: u32 = 1;

/// `ISurfaceComposer` transaction code for `getPhysicalDisplayIds`.
///
/// hwui's `EglManager::initialize` aborts with "Failed to acquire physical
/// displays for WCG support!" unless this returns at least one id. The method
/// is AIDL-generated (the legacy tag 34 is deprecated). The trace shows a
/// no-argument transaction on the `SurfaceFlinger` handle at code 54, but
/// answering it with a `long[]` did not clear the abort, so the code still has
/// to be pinned down.
#[allow(dead_code)]
const ISC_GET_PHYSICAL_DISPLAY_IDS: u32 = 3;

/// The deprecated legacy tag for the same method; both are answered because the
/// trace shows calls at 3 and 54 and the abort only clears when the right one
/// returns a non-empty array.
const ISC_GET_PHYSICAL_DISPLAY_IDS_LEGACY: u32 = 54;

/// Third candidate: the trace shows a no-argument call at 5 immediately before
/// the WCG abort. It turned out to be `getPhysicalDisplayToken` instead.
#[allow(dead_code)]
const ISC_GET_PHYSICAL_DISPLAY_IDS_ALT: u32 = 5;

/// Legacy `android.ui.ISurfaceComposer.getCompositionPreference`.
const ISC_GET_COMPOSITION_PREFERENCE: u32 = 27;

/// Legacy `android.ui.ISurfaceComposer.getDynamicDisplayInfo`.
const ISC_GET_DYNAMIC_DISPLAY_INFO: u32 = 55;

/// Legacy `android.ui.ISurfaceComposer.getStaticDisplayInfo`.
const ISC_LEGACY_GET_STATIC_DISPLAY_INFO: u32 = 8;

/// `ISurfaceComposer.getPhysicalDisplayToken(long id)` (AIDL).
const ISC_GET_PHYSICAL_DISPLAY_TOKEN: u32 = 5;

/// `ISurfaceComposer` transaction code for `createDisplayEventConnection`.
///
/// `ISurfaceComposer` is a hand-written binder interface (`libs/gui/
/// ISurfaceComposer.cpp`), not AIDL, so its reply carries **no status word**:
/// the client's first read is `reply.readStrongBinder()` at offset 0.
const ISC_CREATE_DISPLAY_EVENT_CONNECTION: u32 = 4;

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

/// `IWindowSession` transaction codes, read out of this framework.jar with
/// `--dump-class android.view.IWindowSession$Stub`.
const IWS_ADD_TO_DISPLAY_AS_USER: u32 = 2;
const IWS_REMOVE: u32 = 4;
const IWS_RELAYOUT: u32 = 5;
const IWS_PREPARE_TO_REPLACE_WINDOWS: u32 = 7;
const IWS_SET_INSETS: u32 = 9;
const IWS_FINISH_DRAWING: u32 = 10;
const IWS_SET_IN_TOUCH_MODE: u32 = 11;
const IWS_GET_IN_TOUCH_MODE: u32 = 12;
const IWS_PERFORM_HAPTIC_FEEDBACK: u32 = 13;
const IWS_ON_RECTANGLE_ON_SCREEN_REQUESTED: u32 = 27;
const IWS_GET_WINDOW_ID: u32 = 28;
const IWS_POKE_DRAW_LOCK: u32 = 29;
const IWS_UPDATE_POINTER_ICON: u32 = 32;
const IWS_UPDATE_TAP_EXCLUDE_REGION: u32 = 33;
const IWS_UPDATE_INPUT_CHANNEL: u32 = 38;
const IWS_CLEAR_TOUCHABLE_REGION: u32 = 42;

/// The reply to `IWindowSession.addToDisplayAsUser`, built by the host with the
/// framework's own `Parcel` writer.
static WINDOW_REPLY: std::sync::LazyLock<std::sync::Mutex<Vec<u8>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

/// `InsetsState.SIZE`, the number of inset types this framework knows about.
///
/// `ViewRootImpl` passes `mTempControls`, an `InsetsSourceControl[SIZE]`, as the
/// `out InsetsSourceControl[]` argument, and the AIDL proxy reads it with
/// `readTypedArray(T[], Creator)`, which throws "bad array lengths" unless the
/// count matches exactly.
static INSETS_SIZE: AtomicU32 = AtomicU32::new(0);

/// The server ends of the input channels handed to the client.
static INPUT_SERVER_FDS: std::sync::LazyLock<std::sync::Mutex<Vec<RawFd>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

/// `IDisplayEventConnection` transaction code for `setVsyncRate`.
const IDEC_SET_VSYNC_RATE: u32 = 2;

/// `IDisplayEventConnection` transaction code for `requestNextVsync` (oneway).
const IDEC_REQUEST_NEXT_VSYNC: u32 = 3;

/// Size of one `DisplayEventReceiver::Event` on this ABI.
///
/// `Header` is `{u32 type; u64 displayId; i64 timestamp}` (24 bytes, both
/// 64-bit fields 8-aligned), and the union's largest member is `VSync`:
/// `{u32 count; VsyncEventData}` where `VsyncEventData` is
/// `{i64 frameInterval; u32 preferredFrameTimelineIndex; FrameTimeline[7]}`
/// with `FrameTimeline` = three `i64`s — 192 bytes. 24 + 192 = 216.
const DISPLAY_EVENT_SIZE: usize = 216;
/// `DisplayEventReceiver::DISPLAY_EVENT_VSYNC`.
///
/// Android 13 spells the event types as `fourcc(...)` tags, and `fourcc` packs
/// them big-endian: `'v' << 24 | 's' << 16 | 'y' << 8 | 'n'`. The value was
/// read out of `libgui`'s own comparison in `processPendingEvents`.
const DISPLAY_EVENT_VSYNC: u32 = 0x7673_796e;

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
/// `reply.getString(<key>)`. There is no provider here, so the host installs a
/// marshalled `Bundle` per key with [`__pengudroid_binder_set_setting`].
///
/// A few keys are load-bearing rather than cosmetic: Roblox reads
/// `Settings.Secure.getString(cr, "android_id")` in `ActivitySplash.onCreate`
/// and keeps the result for the whole process, so returning null there is a
/// device that has no identity at all.
static SETTING_ENTRIES: std::sync::LazyLock<std::sync::Mutex<Vec<(String, Vec<u8>)>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

/// `IThermalService` transaction codes, read out of the framework's own
/// `android.os.IThermalService$Stub` static fields (`--dump-class`).
///
/// `PowerManager.addThermalStatusListener` throws
/// `RuntimeException("Listener failed to set")` when the boolean reply is
/// false, and `ActivityNativeMain.onCreate` treats that as fatal: Roblox asks
/// for a thermal listener unconditionally, so an empty reply (which reads back
/// as false) kills the activity before it ever lays out.
const ITHERMAL_REGISTER_THERMAL_EVENT_LISTENER: u32 = 1;
const ITHERMAL_REGISTER_THERMAL_EVENT_LISTENER_WITH_TYPE: u32 = 2;
const ITHERMAL_GET_CURRENT_TEMPERATURES: u32 = 4;
const ITHERMAL_GET_CURRENT_TEMPERATURES_WITH_TYPE: u32 = 5;
const ITHERMAL_REGISTER_THERMAL_STATUS_LISTENER: u32 = 6;
const ITHERMAL_UNREGISTER_THERMAL_STATUS_LISTENER: u32 = 7;
const ITHERMAL_GET_CURRENT_THERMAL_STATUS: u32 = 8;
const ITHERMAL_GET_CURRENT_COOLING_DEVICES: u32 = 9;
const ITHERMAL_GET_CURRENT_COOLING_DEVICES_WITH_TYPE: u32 = 10;
const ITHERMAL_GET_THERMAL_HEADROOM: u32 = 11;

/// `IDisplayManager` transaction code for `getDisplayInfo(int)`.
const IDM_GET_DISPLAY_INFO: u32 = 1;

/// A serialized `DisplayInfo` for the default display.
static DISPLAY_INFO: std::sync::LazyLock<std::sync::Mutex<Vec<u8>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

/// The marshalled `ClientWindowFrames` + `MergedConfiguration` prefix of an
/// `IWindowSession.relayout` reply.
///
/// Built by the host with the framework's own `Parcel.writeTypedObject`, because
/// the field order inside `ClientWindowFrames` is a framework detail and getting
/// it wrong shifts every following out-parameter.
static RELAYOUT_FRAMES: std::sync::LazyLock<std::sync::Mutex<Vec<u8>>> =
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
    if cmd & 0x4000_0000 == 0 {
        // `_IO('c', nr)`: no payload (`BC_ENTER_LOOPER`, `BC_EXIT_LOOPER`).
        return Some(0);
    }
    let size = ((cmd >> 16) & 0x3fff) as u64;
    // A sane upper bound: `binder_transaction_data_sg` is the largest at 72.
    (size <= 4096).then_some(size)
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
            // `Settings.NameValueCache` calls with the key as the `arg`
            // argument and reads `bundle.getString(key)`; `getStringsForPrefix`
            // asks for a prefix instead and expects a `HashMap` under "value".
            // Match the parcel against the keys the host installed rather than
            // trying to parse the AIDL argument order.
            let matched = data.and_then(|data| {
                let entries = SETTING_ENTRIES.lock().unwrap_or_else(|e| e.into_inner());
                entries
                    .iter()
                    .find(|(key, _)| parcel_contains_utf16(data, key))
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
    match code {
        IWS_ADD_TO_DISPLAY_AS_USER => {
            add_to_display_reply(parcel, objects);
        }
        IWS_GET_IN_TOUCH_MODE | IWS_PERFORM_HAPTIC_FEEDBACK => {
            parcel.extend_from_slice(&0u32.to_ne_bytes()); // status
            parcel.extend_from_slice(&0u32.to_ne_bytes()); // false / no feedback
        }
        IWS_RELAYOUT => {
            // `[result][ClientWindowFrames][MergedConfiguration][SurfaceControl]
            //  [InsetsState][InsetsSourceControl[]][Bundle]`, in the order
            // `IWindowSession$Stub$Proxy.relayout` declares its `out` parameters
            // (`--dump-class android.view.IWindowSession$Stub$Proxy`, now printing
            // signatures).
            //
            // The first two out-parameters are marshalled by the host with the
            // framework's own `Parcel.writeTypedObject`: the field order inside
            // `ClientWindowFrames` is a framework detail, and shifting it by one
            // word makes the client read `MergedConfiguration` out of the
            // `SurfaceControl` bytes. An absent `ClientWindowFrames` used to leave
            // `ViewRootImpl`'s frame at 0x0, so every view measured to nothing and
            // `draw()` skipped hwui entirely.
            parcel.extend_from_slice(&0u32.to_ne_bytes()); // result
            {
                let prefix = RELAYOUT_FRAMES.lock().unwrap_or_else(|e| e.into_inner());
                if prefix.is_empty() {
                    // Both absent; `ViewRootImpl` keeps what it pre-allocated.
                    parcel.extend_from_slice(&0u32.to_ne_bytes());
                    parcel.extend_from_slice(&0u32.to_ne_bytes());
                } else {
                    parcel.extend_from_slice(&prefix);
                }
                if trace_enabled() {
                    // SAFETY: `diag` only writes to stderr.
                    unsafe {
                        diag(&format!(
                            "[binder]   -> relayout reply: frames prefix {} byte(s){}\n",
                            prefix.len(),
                            if prefix.is_empty() { " (ABSENT!)" } else { "" }
                        ));
                    }
                }
            }
            // `outSurfaceControl`: present, so `ViewRootImpl` has something to
            // draw into. `SurfaceControl.readFromParcel` reads the Java-side
            // header (name + size + presence) and then
            // `SurfaceControl::readFromParcel`'s
            // `[client][handle][layerId][transformHint][w][h][format]`.
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
            parcel.extend_from_slice(&0u32.to_ne_bytes()); // insets state absent
            let size = INSETS_SIZE.load(Ordering::Relaxed) as usize;
            parcel.extend_from_slice(&(size as u32).to_ne_bytes());
            for _ in 0..size {
                parcel.extend_from_slice(&0u32.to_ne_bytes()); // null element
            }
            parcel.extend_from_slice(&(-1i32).to_ne_bytes()); // null Bundle
        }
        IWS_GET_WINDOW_ID => {
            parcel.extend_from_slice(&0u32.to_ne_bytes()); // status
            push_flat_handle(parcel, objects, handle_for("window_token"));
        }
        IWS_REMOVE
        | IWS_PREPARE_TO_REPLACE_WINDOWS
        | IWS_SET_INSETS
        | IWS_FINISH_DRAWING
        | IWS_SET_IN_TOUCH_MODE
        | IWS_ON_RECTANGLE_ON_SCREEN_REQUESTED
        | IWS_POKE_DRAW_LOCK
        | IWS_UPDATE_POINTER_ICON
        | IWS_UPDATE_TAP_EXCLUDE_REGION
        | IWS_UPDATE_INPUT_CHANNEL
        | IWS_CLEAR_TOUCHABLE_REGION => {
            parcel.extend_from_slice(&0u32.to_ne_bytes()); // status only
        }
        _ => {}
    }
    if trace_enabled() {
        unsafe { diag(&format!("[binder]   -> window session code {code}\n")) };
    }
}

/// Build the reply to `IWindowSession.addToDisplayAsUser`.
///
/// The status word is already in `parcel`. Everything after it mirrors what
/// `IWindowSession$Stub` writes:
///
/// ```text
/// int result                       ADD_OKAY
/// InputChannel outInputChannel     presence + [hasPtr][fd][String8 name]
/// InsetsState insetsState          presence 0 (client keeps its own)
/// InsetsSourceControl[] controls   count == InsetsState.SIZE, all null
/// Rect attachedFrame               presence 0 (client keeps its own)
/// float[] sizeCompatScale          one element, 1.0
/// ```
///
/// The `InputChannel` has to be built here rather than in the host because it
/// carries a file descriptor, and `Parcel.marshall()` cannot express one.
fn add_to_display_reply(parcel: &mut Vec<u8>, objects: &mut Vec<u64>) {
    parcel.extend_from_slice(&0u32.to_ne_bytes()); // result: ADD_OKAY

    match input_channel_pair() {
        Some((client, server)) => {
            INPUT_SERVER_FDS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(server);
            if std::env::var_os("PENGUDROID_NO_INPUT_CHANNEL").is_some() {
                parcel.extend_from_slice(&0u32.to_ne_bytes()); // absent
            } else {
                parcel.extend_from_slice(&1u32.to_ne_bytes()); // present
                parcel.extend_from_slice(&1u32.to_ne_bytes()); // InputChannel.mPtr != 0
                // `InputChannel::writeToParcel` writes the connection token, then
                // the name as UTF-16, then the descriptor — in that order.
                parcel.extend_from_slice(&[0u8; 24]); // null token binder
                parcel.extend_from_slice(&string16("pengudroid"));
                push_flat_fd(parcel, objects, client);
            }
        }
        None => {
            parcel.extend_from_slice(&0u32.to_ne_bytes()); // absent
        }
    }

    // The out-parameters are read in the order the AIDL declares them.
    parcel.extend_from_slice(&0u32.to_ne_bytes()); // InsetsState absent

    let size = INSETS_SIZE.load(Ordering::Relaxed) as usize;
    parcel.extend_from_slice(&(size as u32).to_ne_bytes());
    for _ in 0..size {
        parcel.extend_from_slice(&0u32.to_ne_bytes()); // null element
    }

    parcel.extend_from_slice(&0u32.to_ne_bytes()); // attachedFrame absent

    parcel.extend_from_slice(&1u32.to_ne_bytes()); // float[1]
    parcel.extend_from_slice(&1.0f32.to_ne_bytes());
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

/// Deliver one vsync event on every open `BitTube`.
///
/// The client's `DisplayEventReceiver::getEvents` does a `recv` on the socket
/// and divides the byte count by `sizeof(Event)`, so each `send` has to be
/// exactly one whole event.
fn send_vsync() {
    let mut event = [0u8; DISPLAY_EVENT_SIZE];
    event[0..4].copy_from_slice(&DISPLAY_EVENT_VSYNC.to_ne_bytes());
    // `displayId` stays 0 (the default display).

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
    const FRAME_INTERVAL: i64 = 16_666_666;
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
        if previous != 0 && vsync_time - previous < FRAME_INTERVAL / 2 {
            return;
        }
        LAST_SEND.store(vsync_time, Ordering::Relaxed);
    }
    let frame = FRAME.fetch_add(1, Ordering::Relaxed);

    event[16..24].copy_from_slice(&vsync_time.to_ne_bytes());
    // `vsync.count` is the frame number; a monotonically increasing counter is
    // what `Choreographer` expects.
    event[24..28].copy_from_slice(&frame.to_ne_bytes());

    // `VsyncEventData`: `{frameInterval, preferredFrameTimelineIndex, timelines[7]}`
    // where a timeline is `{vsyncId, expectedPresentationTime, deadline}`.
    // Leaving the timelines zeroed is what makes `Choreographer` treat every
    // frame as already late, so the first entry is filled in and marked
    // preferred.
    const TIMELINES_OFFSET: usize = 48;
    const TIMELINE_SIZE: usize = 24;
    event[32..40].copy_from_slice(&FRAME_INTERVAL.to_ne_bytes());
    event[40..44].copy_from_slice(&0u32.to_ne_bytes()); // preferredFrameTimelineIndex
    let vsync_id = frame as i64;
    let expected = vsync_time + FRAME_INTERVAL;
    let base = TIMELINES_OFFSET;
    event[base..base + 8].copy_from_slice(&vsync_id.to_ne_bytes());
    event[base + 8..base + 16].copy_from_slice(&expected.to_ne_bytes());
    event[base + 16..base + 24].copy_from_slice(&expected.to_ne_bytes());
    // The remaining six entries stay zeroed; `preferredFrameTimelineIndex` is 0.
    let _ = TIMELINE_SIZE;

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
    if trace_enabled() {
        unsafe {
            diag(&format!(
                "[binder]   -> vsync frame {frame} ts={vsync_time} dropped={dropped}\n"
            ))
        };
    }
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
                send_vsync();
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

/// Reverse lookup of a handle, for diagnostics.
/// Whether `handle` is the media metrics service, under either of the two
/// names it is known by (`media.metrics` is the servicemanager name,
/// `media_metrics` the one clients request through `Context`).
fn is_media_metrics_handle(handle: u32) -> bool {
    matches!(
        name_for_handle(handle).as_deref(),
        Some("media.metrics") | Some("media_metrics")
    )
}

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
/// that lives under it. An `Intent` writes `package` right before its
/// `ComponentName`, so this pair identifies the target service. The package's
/// offset is returned too: the `Intent` is unmarshalled from that point.
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
/// Android 13's `Intent.writeToParcel` writes its strings with `writeString8`
/// (`[u32 len including NUL][bytes][pad to 4]`), and a null is either `-1` or
/// `0` depending on the field. All writes are 4-byte aligned, so stepping back
/// in 4-byte steps finds the length word.
fn string8_start_ending_at(data: &[u8], end: usize) -> Option<usize> {
    if end < 4 || end > data.len() {
        return None;
    }
    let word = u32::from_ne_bytes([data[end - 4], data[end - 3], data[end - 2], data[end - 1]]);
    if word == u32::MAX || word == 0 {
        return Some(end - 4);
    }
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
            if (1..=1024).contains(&len) && start + 4 + len <= data.len() {
                if (start + 4 + len + 3) & !3 == end {
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

/// Offset of the `Intent` that contains the component package at
/// `package_offset`.
///
/// `Intent.writeToParcel` (Android 13) writes, in order:
/// `action` (string8), `data` (`Uri`, either a string8 or an int `0`), `type`
/// (string8), `identifier` (string8), `flags` (int), `package` (string8), then
/// the `ComponentName` as two `string16`s. The package this function is given is
/// the second of those (the component's), so walking back over the six fields
/// before it lands on `action` — the first byte of the `Intent`, and therefore
/// the offset `Parcel.unmarshall` wants.
fn intent_start(data: &[u8], package_offset: usize) -> Option<usize> {
    let package = string8_start_ending_at(data, package_offset)?;
    let flags = package.checked_sub(4)?;
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
    for start in [12usize, 0] {
        if data.len() < start + 8 {
            continue;
        }
        let len = u32::from_ne_bytes([
            data[start],
            data[start + 1],
            data[start + 2],
            data[start + 3],
        ]) as usize;
        if len == 0 || len > 512 || start + 4 + len * 2 > data.len() {
            continue;
        }
        let text = decode_utf16le(&data[start + 4..start + 4 + len * 2]);
        if text.chars().all(|c| c.is_ascii_graphic() || c == '.' || c == '/') {
            return Some(text);
        }
    }
    None
}

/// Whether a request was addressed to the legacy `android.ui.ISurfaceComposer`.
///
/// The same handle and the same code number can belong to either interface: the
/// AIDL proxy writes `android.gui.ISurfaceComposer` as its token and the legacy
/// one writes `android.ui.ISurfaceComposer`. Only the token tells them apart,
/// and only the AIDL replies start with a `Status` word.
fn is_legacy_surface_composer(data: &[u8]) -> bool {
    matches!(first_string16(data).as_deref(), Some(t) if t.contains("android.ui.ISurfaceComposer"))
}

/// Build a flattened `ui::DynamicDisplayInfo` by asking libui to serialise a
/// zeroed instance.
///
/// Hand-writing the blob is error-prone (it is a chain of nested vectors and
/// optionals), but the library exports both `getFlattenedSize` and `flatten`, so
/// a zeroed instance round-trips into a blob that `unflatten` accepts.
fn dynamic_display_info_blob() -> Option<Vec<u8>> {
    let handle = unsafe { libc::dlopen(c"libui.so".as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
    if handle.is_null() {
        return None;
    }
    let size_fn = unsafe {
        libc::dlsym(
            handle,
            c"_ZNK7android2ui18DynamicDisplayInfo16getFlattenedSizeEv".as_ptr(),
        )
    };
    let flatten_fn = unsafe {
        libc::dlsym(handle, c"_ZNK7android2ui18DynamicDisplayInfo7flattenEPvm".as_ptr())
    };
    if size_fn.is_null() || flatten_fn.is_null() {
        return None;
    }
    // Generous, zeroed storage for the struct itself.
    let mut instance = vec![0u8; 1024];
    // `ADisplay_acquirePhysicalDisplays` rejects a display whose
    // `supportedDisplayModes` vector is empty (-ENODEV), so give it one mode.
    // `sizeof(ui::DisplayMode)` is 0x38 (from `vector::__push_back_slow_path`).
    let mut mode = vec![0u8; 0x38];
    mode[0..4].copy_from_slice(&1u32.to_ne_bytes()); // id
    mode[4..12].copy_from_slice(&16_666_666u64.to_ne_bytes()); // vsync period (ns)
    mode[0x0c..0x10].copy_from_slice(&1280u32.to_ne_bytes()); // width
    mode[0x10..0x14].copy_from_slice(&720u32.to_ne_bytes()); // height
    mode[0x14..0x18].copy_from_slice(&60.0f32.to_ne_bytes()); // refresh rate
    let mode_addr = mode.as_ptr() as u64;
    instance[0..8].copy_from_slice(&mode_addr.to_ne_bytes());
    instance[8..16].copy_from_slice(&(mode_addr + 0x38).to_ne_bytes());
    instance[16..24].copy_from_slice(&(mode_addr + 0x38).to_ne_bytes());
    // `activeDisplayModeId` sits at offset 0x18 in the struct.
    instance[0x18..0x1c].copy_from_slice(&1u32.to_ne_bytes());
    // `supportedColorModes` is the vector at offset 0x20; an empty one leaves
    // hwui with pixel format 0 ("Unreachable: unsupported pixel format: 0").
    // `ui::ColorMode::SRGB` is 1.
    let mut color_mode = vec![0u8; 4];
    color_mode[0..4].copy_from_slice(&1i32.to_ne_bytes());
    let cm_addr = color_mode.as_ptr() as u64;
    instance[0x20..0x28].copy_from_slice(&cm_addr.to_ne_bytes());
    instance[0x28..0x30].copy_from_slice(&(cm_addr + 4).to_ne_bytes());
    instance[0x30..0x38].copy_from_slice(&(cm_addr + 4).to_ne_bytes());
    type SizeFn = unsafe extern "C" fn(*const u8) -> usize;
    type FlattenFn = unsafe extern "C" fn(*const u8, *mut u8, usize) -> i32;
    let size_of: SizeFn = unsafe { std::mem::transmute(size_fn) };
    let flatten: FlattenFn = unsafe { std::mem::transmute(flatten_fn) };
    let size = unsafe { size_of(instance.as_ptr()) };
    if size == 0 || size > 4096 {
        return None;
    }
    let mut blob = vec![0u8; size];
    let rc = unsafe { flatten(instance.as_ptr(), blob.as_mut_ptr(), size) };
    if rc != 0 {
        return None;
    }
    Some(blob)
}

/// Whether a reply starts with an AIDL `Status` word.
///
/// Almost every interface here is AIDL, whose generated proxy reads the status
/// before anything else. `ISurfaceComposer` is the exception: it is a
/// hand-written binder interface, and the methods that return a binder read it
/// straight from offset 0. Writing a status word there shifts every object by
/// four bytes, `Parcel::finishUnflattenBinder` cannot find the object in the
/// offset table, and `readStrongBinder` returns null.
fn reply_has_status(handle: u32, code: u32) -> bool {
    if is_surface_flinger(handle) {
        // Two different interfaces share the name: the legacy hand-written
        // `android.ui.ISurfaceComposer` (asked for as "SurfaceFlinger") and the
        // AIDL `android.gui.ISurfaceComposer` (asked for as
        // "SurfaceFlingerAIDL"). Only the AIDL one starts its replies with a
        // `Status` word; for the legacy one a status word shifts every field by
        // four bytes, so `readInt64Vector` reads the status (0) as the element
        // count and every reply looks empty.
        let aidl = matches!(name_for_handle(handle).as_deref(), Some("SurfaceFlingerAIDL"));
        if !aidl {
            return false;
        }
        // `createConnection` and `createDisplayEventConnection` are legacy
        // methods that also end in `reply.readStrongBinder()` with no status.
        return !matches!(
            code,
            ISC_CREATE_CONNECTION | ISC_CREATE_DISPLAY_EVENT_CONNECTION
        );
    }
    if handle == handle_for("surface_composer_client") {
        // `callRemote` reads the out parameters first and the status last.
        return false;
    }
    if handle == handle_for("media.resource_manager") && code == INTERFACE_TRANSACTION {
        // `BpBinder::getInterfaceDescriptor` (`libbinder.so` `0x5ef60`) calls
        // `transact` and then `Parcel::readString16()` straight away, with no
        // status word in front of the descriptor.
        return false;
    }
    true
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
    if trace_enabled() && is_surface_flinger(tr.target_handle) && tr.data_size > 0 && tr.data_ptr != 0 {
        let data = std::slice::from_raw_parts(tr.data_ptr as *const u8, tr.data_size as usize);
        let token = first_string16(data).unwrap_or_else(|| "?".into());
        let has_binder = data
            .windows(4)
            .any(|w| u32::from_ne_bytes([w[0], w[1], w[2], w[3]]) == BINDER_TYPE_HANDLE);
        diag(&format!(
            "[binder]   SF code={} token={token} binder={has_binder}\n",
            tr.code
        ));
    }
    if is_surface_flinger(tr.target_handle) && tr.code == ISC_GET_COMPOSITION_PREFERENCE {
        // Legacy `getCompositionPreference`: `[status][dataspace][format][renderIntent]`.
        // hwui's `DeviceInfo::updateDisplayInfo` asserts "Unreachable: unsupported
        // pixel format" unless the format is 1 (RGBA_8888) or 0x16.
        parcel.extend_from_slice(&0i32.to_ne_bytes()); // status
        // The client reads five int32s. `ADisplay_acquirePhysicalDisplays` copies
        // the third payload into `ADisplay+0x0c` (dataspace) and the fourth into
        // `ADisplay+0x10` (the pixel format that
        // `ADisplay_getPreferredWideColorFormat` reports), so the fourth carries
        // RGBA_8888.
        parcel.extend_from_slice(&0i32.to_ne_bytes()); // 1st payload
        parcel.extend_from_slice(&1i32.to_ne_bytes()); // 2nd payload
        parcel.extend_from_slice(&0i32.to_ne_bytes()); // 3rd payload -> dataspace
        parcel.extend_from_slice(&1i32.to_ne_bytes()); // 4th payload -> format RGBA_8888
        if trace_enabled() {
            diag("[binder]   -> getCompositionPreference (RGBA_8888)\n");
        }
    }
    if is_surface_flinger(tr.target_handle) && tr.code == ISC_GET_DYNAMIC_DISPLAY_INFO {
        // Legacy `getDynamicDisplayInfo`: `[int32 status][int32 size][blob]`.
        // `ADisplay_acquirePhysicalDisplays` returns -ENODATA when this fails,
        // which leaves hwui with no display at all.
        match dynamic_display_info_blob() {
            Some(blob) => {
                parcel.extend_from_slice(&0i32.to_ne_bytes());
                parcel.extend_from_slice(&(blob.len() as i32).to_ne_bytes());
                parcel.extend_from_slice(&blob);
                if trace_enabled() {
                    diag(&format!("[binder]   -> getDynamicDisplayInfo ({} bytes)\n", blob.len()));
                }
            }
            None => {
                diag("[binder]   getDynamicDisplayInfo: libui flatten unavailable\n");
            }
        }
    }
    if is_surface_flinger(tr.target_handle) && tr.code == ISC_GET_PHYSICAL_DISPLAY_IDS {
        let legacy = tr.data_size > 0
            && tr.data_ptr != 0
            && is_legacy_surface_composer(std::slice::from_raw_parts(
                tr.data_ptr as *const u8,
                tr.data_size as usize,
            ));
        if legacy {
            // Legacy `android.ui.ISurfaceComposer.getStaticDisplayInfo`:
            // `[int32 status][flattened ui::StaticDisplayInfo]`. The flattened
            // form, read out of `StaticDisplayInfo::unflatten` and
            // `DeviceProductInfo::unflatten`, is
            //   [4B connected][4B density][1B secure]
            //   [u64 nameLen][name][u32 year][u64 modelLen][model][u64][u32]
            //   [4B]
            // `ADisplay_acquirePhysicalDisplays` drops a display whose static
            // info cannot be read, and hwui then indexes `displays[0]` of an
            // empty array.
            parcel.extend_from_slice(&0i32.to_ne_bytes()); // status
            // The client reads `status`, then the flattened *size*, then the blob
            // (`readInt32`, `readInt32`, `readInplace(size)`, `unflatten`).
            let mut blob: Vec<u8> = Vec::new();
            blob.extend_from_slice(&1i32.to_ne_bytes()); // connected
            blob.extend_from_slice(&420.0f32.to_ne_bytes()); // density
            blob.push(0); // secure
            blob.extend_from_slice(&0u64.to_ne_bytes()); // product name length
            blob.extend_from_slice(&0u32.to_ne_bytes()); // manufacture year
            blob.extend_from_slice(&0u64.to_ne_bytes()); // model length
            blob.extend_from_slice(&0u64.to_ne_bytes());
            blob.extend_from_slice(&0u32.to_ne_bytes());
            blob.extend_from_slice(&0u32.to_ne_bytes()); // trailing field
            parcel.extend_from_slice(&(blob.len() as i32).to_ne_bytes());
            parcel.extend_from_slice(&blob);
            if trace_enabled() {
                diag("[binder]   -> legacy getStaticDisplayInfo (connected, 420 dpi)\n");
            }
        } else {
            // AIDL `long[] getPhysicalDisplayIds()`: `[int32 count][int64 id...]`.
            parcel.extend_from_slice(&1i32.to_ne_bytes());
            parcel.extend_from_slice(&0i64.to_ne_bytes());
            if trace_enabled() {
                diag("[binder]   -> getPhysicalDisplayIds = [0]\n");
            }
        }
    }
    if is_surface_flinger(tr.target_handle) && tr.code == ISC_GET_PHYSICAL_DISPLAY_TOKEN {
        // AIDL `IBinder getPhysicalDisplayToken(long id)`: `[status][strongBinder]`.
        // `ADisplay_acquirePhysicalDisplays` skips a display whose token is null,
        // and hwui's `DeviceInfo::updateDisplayInfo` then indexes `displays[0]`
        // of an empty array and faults.
        push_flat_handle(&mut parcel, &mut objects, handle_for("display_token"));
        if trace_enabled() {
            diag("[binder]   -> display_token handle\n");
        }
    }
    if tr.target_handle == 0
        && (tr.code == ISM_GET_SERVICE || tr.code == ISM_CHECK_SERVICE)
        && !is_hidl_request(tr)
    {
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
    if tr.target_handle == handle_for("codec2") {
        // The real codec2 service objects are hosted in this process (see
        // `codec2.rs`), so the generated HIDL code does the marshalling for us.
        // The stub replies below are only the fallback for when the service
        // libraries cannot be loaded.
        //
        // `IBase` methods (codes above `0x0F000000`) are answered by the stub:
        // `interfaceChain` writes its reply synchronously, so the `hidl_vec` it
        // points at is a dead local by the time `onTransact` returns. The
        // methods that matter (`createComponent`, `listComponents`) hand the
        // reply to the callback instead, which is where `codec2.rs` captures it.
        if tr.data_ptr != 0 && tr.code < 0x0F00_0000 {
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
            if let Some((bytes, offs)) = crate::codec2::dispatch(tr.code, data, &offsets) {
                if trace_enabled() {
                    diag(&format!(
                        "[binder]   -> codec2 service code {} ({} bytes, {} objects)\n",
                        tr.code,
                        bytes.len(),
                        offs.len()
                    ));
                }
                return Reply {
                    parcel: bytes,
                    objects: offs,
                };
            }
        }
        // The first call on a HIDL proxy is `interfaceChain`; the client uses it
        // to decide whether the proxy really implements the interface it asked
        // for. Its code is a hash, not a small ordinal.
        if tr.code == 0x0F43_484E {
            let chain = [
                "android.hardware.media.c2@1.0::IComponentStore",
                "android.hardware.media.c2@1.0::ICodec2",
                "android.hidl.base@1.0::IBase",
            ];
            push_hidl_string_vec(&mut parcel, &mut objects, &chain);
        } else if tr.code == 3 {
            // `IComponentStore::listComponents()` -> `vec<ComponentTraits>`.
            // Measured from writeEmbeddedToParcel(ComponentTraits) (0x5de70):
            // name @0x00, mediaType @0x10, domain/kind/rank @0x20, the aliases
            // vector @0x30 and a third string @0x40 (80 bytes total).
            let (name_ptr, name_len) = leak_cstr("c2.android.avc.decoder");
            let (mt_ptr, mt_len) = leak_cstr("video/avc");
            let mut info = Vec::with_capacity(64);
            info.extend_from_slice(&name_ptr.to_ne_bytes());
            info.extend_from_slice(&(name_len - 1).to_ne_bytes());
            info.extend_from_slice(&1u32.to_ne_bytes()); // Domain::VIDEO
            info.extend_from_slice(&0u32.to_ne_bytes()); // Kind::DECODER
            info.extend_from_slice(&0u32.to_ne_bytes()); // rank
            info.extend_from_slice(&0u32.to_ne_bytes()); // padding
            info.extend_from_slice(&mt_ptr.to_ne_bytes());
            info.extend_from_slice(&(mt_len - 1).to_ne_bytes());
            let (al_ptr, al_len) = leak_cstr("OMX.google.h264.decoder");
            let mut alias_arr = Vec::with_capacity(16);
            alias_arr.extend_from_slice(&al_ptr.to_ne_bytes());
            alias_arr.extend_from_slice(&(al_len - 1).to_ne_bytes());
            let alias_arr: &'static [u8] = Box::leak(alias_arr.into_boxed_slice());
            info.extend_from_slice(&(alias_arr.as_ptr() as u64).to_ne_bytes()); // aliases.mData
            info.extend_from_slice(&1u64.to_ne_bytes()); // aliases.mSize
            let info: &'static [u8] = Box::leak(info.into_boxed_slice());

            let mut vec = Vec::with_capacity(16);
            vec.extend_from_slice(&(info.as_ptr() as u64).to_ne_bytes());
            vec.extend_from_slice(&1u32.to_ne_bytes());
            vec.extend_from_slice(&0u32.to_ne_bytes());
            let vec: &'static [u8] = Box::leak(vec.into_boxed_slice());

            // `listComponents() generates (Status, vec<ComponentTraits>)`.
            // The reply is `[hidl status][codec2 status][vec struct][elements]`:
            // the client reads the HIDL `Status`, then a second int32 that is
            // the codec2 `V1_0::Status` handed to the callback (measured: a
            // non-zero value here makes `Codec2Client::_listComponents` log
            // "call failed: BAD_STATE"), then the 16-byte vec struct and the
            // element array. The element count comes from the vec struct, not
            // from this word.
            parcel.extend_from_slice(&0i32.to_ne_bytes()); // codec2 Status::OK
            let vec_idx =
                push_buffer_object(&mut parcel, &mut objects, 0, vec.as_ptr() as u64, 16, 0, 0);
            let arr_idx =
                push_buffer_object(&mut parcel, &mut objects, 1, info.as_ptr() as u64, 64, vec_idx, 0);
            push_buffer_object(&mut parcel, &mut objects, 1, name_ptr, name_len, arr_idx, 0);
            push_buffer_object(&mut parcel, &mut objects, 1, mt_ptr, mt_len, arr_idx, 32);
            // The parent check compares `*(u64*)(parentBuffer + parentOffset)`
            // with the child's buffer, so the aliases vector's buffer is its
            // data pointer, not the address of the struct field.
            // `readNullableEmbeddedBuffer(size*16, &handle, i*64, i*64+0x30)`
            // reads the element array itself, so there is no separate object
            // for the vector struct.
            let alias_arr_idx = push_buffer_object(
                &mut parcel,
                &mut objects,
                1,
                alias_arr.as_ptr() as u64,
                16,
                arr_idx,
                0x30,
            );
            push_buffer_object(&mut parcel, &mut objects, 1, al_ptr, al_len, alias_arr_idx, 0);
        } else if tr.code == 1 {
            // `createComponent(hidl_string name, sp<IComponentListener>,
            // sp<IClientManager>) generates (Status, sp<IComponent>)`. The
            // client reads the status and then the binder, so the object sits
            // at offset 4 (measured: "read object ... at offset 4").
            push_flat_handle(&mut parcel, &mut objects, handle_for("codec2_component"));
        } else {
            // `getConfigurable`, `getPoolClientManager` and `createInterface`
            // all return a binder; handing back the store's own handle keeps
            // the client's `static_cast<sp<...>>` from dereferencing a null.
            // `createInterface` is code 2 and the client's `mDataPos` is 8 when
            // it reads the object, so that reply carries a padding word.
            if tr.code == 2 {
                parcel.extend_from_slice(&0u32.to_ne_bytes());
            }
            push_flat_handle(&mut parcel, &mut objects, handle_for("codec2"));
        }
        if trace_enabled() {
            diag(&format!("[binder]   -> codec2 call code {}\n", tr.code));
        }
    }
    if tr.target_handle == 0 && is_hidl_request(tr) {
        unsafe { hidl_service_manager_reply(tr.code, &mut parcel, &mut objects, tr) };
    }
    if tr.target_handle == handle_for("activity_task") && tr.code == ATM_GET_ACTIVITY_CLIENT_CONTROLLER {
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
    if tr.target_handle == handle_for("activity")
        && (tr.code == IAM_START_SERVICE || tr.code == IAM_BIND_SERVICE)
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
                if tr.code == IAM_START_SERVICE { "startService" } else { "bindService" }
            ));
        }
        if tr.code == IAM_START_SERVICE {
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
            note_service_request(&pkg, &cls, tr.code, data, intent_offset);
        }
    }
    if tr.target_handle == handle_for("activity") && tr.code == IAM_GET_CONTENT_PROVIDER {
        let info = PROVIDER_INFO.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if !info.is_empty() {
            // `ContentProviderHolder` layout, measured by marshalling one with
            // null binders: [ProviderInfo][provider][connection][int][int].
            // `readTypedObject` supplies the presence int, so it goes first.
            parcel.extend_from_slice(&1u32.to_ne_bytes());
            parcel.extend_from_slice(&info);
            let handle = handle_for("settings_provider");
            push_flat_handle(&mut parcel, &mut objects, handle);
            push_flat_handle(&mut parcel, &mut objects, handle);
            parcel.extend_from_slice(&0u32.to_ne_bytes()); // noReleaseNeeded
            parcel.extend_from_slice(&0u32.to_ne_bytes()); // mLocal
            if trace_enabled() {
                diag(&format!("[binder]   -> ContentProviderHolder ({} bytes)\n", info.len()));
            }
        }
    }
    if is_media_metrics_handle(tr.target_handle) && tr.code == IMM_GET_PLAYBACK_SESSION_ID {
        // `MediaMetricsManager.createPlaybackSession` wraps this reply in a
        // `PlaybackSession`, whose constructor
        // `AnnotationValidations.validate(NonNull.class, null, id)` rejects a null
        // id with an NPE. ExoPlayer 2.18 registers such a listener unconditionally
        // on API 31+, so without an id `Player.initPlayback` dies and no playback
        // session is ever created.
        //
        // The handle has to be matched by *name* and not by
        // `handle_for("media.metrics")`: ServiceManager registers the service as
        // `media.metrics` but every Java client asks for it through
        // `Context.MEDIA_METRICS_SERVICE`, which is `media_metrics` — the handle
        // the app transacts with is the one the service manager minted for the
        // underscore name. Keying the reply on the dotted name silently produced
        // an empty reply, and `Parcel.readString16` then returned null.
        parcel.extend_from_slice(&string16(&format!(
            "pengudroid-playback-{}",
            NEXT_HANDLE.fetch_add(1, Ordering::Relaxed)
        )));
        if trace_enabled() {
            diag("[binder]   -> media.metrics playback session id\n");
        }
    }
    if tr.target_handle == handle_for("media_session") && tr.code == ISM_CREATE_SESSION {
        // `MediaSessionManager.createSession` mints the framework session that
        // every `MediaSessionCompat` on API 29+ wraps; `MediaSession`'s
        // constructor immediately dereferences it (`ISession.getController`),
        // so returning nothing makes NewPipe's `PlayerService.onCreate` die
        // with an NPE. A pair of empty binder handles is enough for the
        // constructor and for every later `setFlags`/`setMetadata`/... call,
        // which are void and succeed with no result.
        push_flat_handle(&mut parcel, &mut objects, handle_for("media_session_session"));
        if trace_enabled() {
            diag("[binder]   -> createSession (fake ISession)\n");
        }
    }
    if tr.target_handle == handle_for("media_session_session")
        && tr.code == ISESSION_GET_CONTROLLER
    {
        push_flat_handle(
            &mut parcel,
            &mut objects,
            handle_for("media_session_controller"),
        );
        if trace_enabled() {
            diag("[binder]   -> ISession.getController (fake ISessionController)\n");
        }
    }
    if tr.target_handle == handle_for("media_session_session")
        && tr.code == ISESSION_GET_BINDER_FOR_SET_QUEUE
    {
        // `MediaSession.setQueue` hands this binder to
        // `ParcelableListBinder.send`, which calls `binder.transact(...)` with
        // no null check, so a null result is an NPE in the middle of
        // `MediaSessionConnector.onEvents`. The transaction it sends is
        // fire-and-forget for our purposes: an empty reply satisfies libbinder.
        push_flat_handle(
            &mut parcel,
            &mut objects,
            handle_for("media_session_queue"),
        );
        if trace_enabled() {
            diag("[binder]   -> ISession.getBinderForSetQueue (fake queue binder)\n");
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
    if tr.target_handle == handle_for("user")
        && (tr.code == IUM_IS_USER_UNLOCKED || tr.code == IUM_IS_USER_UNLOCKING_OR_UNLOCKED)
    {
        // Without this the framework logs "Failed to find provider info for
        // settings (user not unlocked)" and refuses the provider lookup, and
        // `getSharedPreferences` throws for credential-encrypted storage.
        parcel.extend_from_slice(&1u32.to_ne_bytes());
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
    if is_surface_flinger(tr.target_handle) && tr.code == ISC_CREATE_CONNECTION {
        push_flat_handle(
            &mut parcel,
            &mut objects,
            handle_for("surface_composer_client"),
        );
        if trace_enabled() {
            diag("[binder]   -> surface_composer_client handle\n");
        }
    }
    if tr.target_handle == handle_for("surface_composer_client")
        && tr.code == ISCC_CREATE_SURFACE
    {
        // `[handle][gbp][layerId][transformHint][status]`.
        push_flat_handle(&mut parcel, &mut objects, handle_for("surface_handle"));
        push_flat_handle(&mut parcel, &mut objects, handle_for("graphic_buffer_producer"));
        parcel.extend_from_slice(&1i32.to_ne_bytes()); // layerId
        parcel.extend_from_slice(&0u32.to_ne_bytes()); // transformHint
        parcel.extend_from_slice(&0i32.to_ne_bytes()); // status: NO_ERROR
        if trace_enabled() {
            diag("[binder]   -> createSurface handle + gbp\n");
        }
    }
    if is_surface_flinger(tr.target_handle) && tr.code == ISC_AUTHENTICATE_SURFACE {
        // `TextureView` asks SurfaceFlinger to vouch for its `SurfaceTexture`
        // before the first frame. An empty reply makes `Parcel::readBool` fail
        // and the framework logs
        // "authenticateSurfaceTexture: error retrieving result: No data available".
        // There is no SurfaceFlinger to authenticate against, and the consumer
        // is ours, so the answer is simply "yes".
        parcel.extend_from_slice(&1i32.to_ne_bytes());
        if trace_enabled() {
            diag("[binder]   -> authenticateSurfaceTexture = true\n");
        }
    }
    if is_surface_flinger(tr.target_handle) && tr.code == ISC_CREATE_DISPLAY_EVENT_CONNECTION {
        // No status word: the client's first read is `readStrongBinder()`.
        push_flat_handle(
            &mut parcel,
            &mut objects,
            handle_for("display_event_connection"),
        );
        if trace_enabled() {
            diag("[binder]   -> display_event_connection handle\n");
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
        // `setVsyncRate` / `requestNextVsync` / `getLatestVsyncEventData` are
        // either oneway or not needed for the first frame; answer with a bare
        // status so a synchronous caller does not block.
        if tr.code != IDEC_STEAL_RECEIVE_CHANNEL {
            parcel.extend_from_slice(&0u32.to_ne_bytes());
        }
        if tr.code == IDEC_REQUEST_NEXT_VSYNC {
            // `ViewRootImpl` schedules its first traversal from a vsync, so
            // without this the window is added but never laid out and
            // `relayout` is never called.
            send_vsync();
        }
        if tr.code == IDEC_SET_VSYNC_RATE {
            send_vsync();
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
            && tr.code == IWS_ADD_TO_DISPLAY_AS_USER
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
        if trace_enabled() && tr.code == IWS_RELAYOUT && tr.data_size > 0 && tr.data_ptr != 0 {
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
            diag(&format!(
                "[binder]   relayout win={win:#x} data={} {}\n",
                data.len(),
                ints.join(" ")
            ));
        }
        window_session_reply(tr.code, &mut parcel, &mut objects);
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
        // A handheld that is never throttled is the honest model here: there is
        // no thermal zone to read on the host, and a "none" status is what a
        // cool device reports. The listener registrations must answer `true` or
        // the framework throws instead of returning.
        match tr.code {
            ITHERMAL_REGISTER_THERMAL_EVENT_LISTENER
            | ITHERMAL_REGISTER_THERMAL_EVENT_LISTENER_WITH_TYPE
            | ITHERMAL_REGISTER_THERMAL_STATUS_LISTENER
            | ITHERMAL_UNREGISTER_THERMAL_STATUS_LISTENER => {
                parcel.extend_from_slice(&1u32.to_ne_bytes());
            }
            ITHERMAL_GET_CURRENT_THERMAL_STATUS => {
                parcel.extend_from_slice(&0u32.to_ne_bytes()); // THERMAL_STATUS_NONE
            }
            ITHERMAL_GET_CURRENT_TEMPERATURES
            | ITHERMAL_GET_CURRENT_TEMPERATURES_WITH_TYPE
            | ITHERMAL_GET_CURRENT_COOLING_DEVICES
            | ITHERMAL_GET_CURRENT_COOLING_DEVICES_WITH_TYPE => {
                parcel.extend_from_slice(&0u32.to_ne_bytes()); // empty typed array
            }
            ITHERMAL_GET_THERMAL_HEADROOM => {
                parcel.extend_from_slice(&0.0f32.to_ne_bytes());
            }
            _ => {}
        }
        if trace_enabled() {
            diag(&format!("[binder]   -> thermal code {}\n", tr.code));
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
    Reply { parcel, objects }
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
unsafe fn incoming_transaction(target_weak: u64, cookie: u64, code: u32, parcel: &[u8]) -> Option<Vec<u8>> {
    let reply = Reply {
        parcel: parcel.to_vec(),
        objects: Vec::new(),
    };
    let (addr, offsets_addr) = place_reply(&reply)?;
    let tr = TransactionData {
        target_handle: target_weak as u32,
        _pad0: (target_weak >> 32) as u32,
        cookie,
        code,
        flags: 0,
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
    let mut off = 0u64;
    while off + 4 <= wr.write_size {
        let cmd = *((wr.write_buffer + off) as *const u32);
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
        if (cmd == BC_REPLY || cmd == BC_REPLY_SG) && trace_enabled() {
            // The app answering a transaction we sent it: dump the parcel so the
            // Java exception (if any) is readable.
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
        off += size;
    }
    // The driver consumes the whole write buffer even for commands we do not
    // interpret; leaving it unconsumed makes libbinder report -EINVAL.
    wr.write_consumed = wr.write_size;

    let mut out: Vec<u8> = Vec::new();

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
                if let Some(bytes) = unsafe { incoming_transaction(weak, cookie, code, &probe_parcel()) } {
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

    for entry in &pending {
        push_u32(&mut out, BR_TRANSACTION_COMPLETE);
        let Some(reply) = entry else { continue };
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

/// `void __pengudroid_binder_set_insets_size(int size)`
///
/// # Safety
/// Plain integer setter; safe to call from any thread.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_binder_set_insets_size(size: c_int) {
    INSETS_SIZE.store(size.max(0) as u32, Ordering::Relaxed);
}

/// `void __pengudroid_binder_set_window_reply(const uint8_t *bytes, size_t len)`
///
/// # Safety
/// `ptr` must point to `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_binder_set_window_reply(ptr: *const u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    let bytes = std::slice::from_raw_parts(ptr, len).to_vec();
    *WINDOW_REPLY.lock().unwrap_or_else(|e| e.into_inner()) = bytes;
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

fn note_service_request(package: &str, class: &str, code: u32, data: &[u8], intent_offset: usize) {
    let bind = code == IAM_BIND_SERVICE;
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

/// `void __pengudroid_binder_set_relayout_frames(const uint8_t *bytes, size_t len)`
///
/// # Safety
/// `ptr` must point to `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_binder_set_relayout_frames(ptr: *const u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    let bytes = std::slice::from_raw_parts(ptr, len).to_vec();
    *RELAYOUT_FRAMES.lock().unwrap_or_else(|e| e.into_inner()) = bytes;
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

    /// `PowerManager.addThermalStatusListener` throws
    /// `RuntimeException("Listener failed to set")` unless
    /// `IThermalService.registerThermalStatusListener` answers `true`, and
    /// Roblox's `ActivityNativeMain.onCreate` treats that as fatal. An empty
    /// reply reads back as `false`, so the boolean has to be written.
    #[test]
    fn thermal_listeners_register_successfully() {
        let tr = TransactionData {
            target_handle: handle_for("thermalservice"),
            code: ITHERMAL_REGISTER_THERMAL_STATUS_LISTENER,
            ..TransactionData::default()
        };
        // SAFETY: `make_reply` only reads the transaction descriptor; this one
        // carries no parcel payload.
        let reply = unsafe { make_reply(&tr, 1) };
        assert_eq!(reply.parcel.len(), 8, "status word plus the boolean");
        assert_eq!(&reply.parcel[0..4], &0u32.to_ne_bytes());
        assert_eq!(&reply.parcel[4..8], &1u32.to_ne_bytes());

        // The status query must answer "not throttled" rather than nothing.
        let tr = TransactionData {
            target_handle: handle_for("thermalservice"),
            code: ITHERMAL_GET_CURRENT_THERMAL_STATUS,
            ..TransactionData::default()
        };
        // SAFETY: as above.
        let reply = unsafe { make_reply(&tr, 1) };
        assert_eq!(reply.parcel.len(), 8);
        assert_eq!(&reply.parcel[4..8], &0u32.to_ne_bytes());
    }

    /// `MediaSession.setQueue` passes `ISession.getBinderForSetQueue()`'s result
    /// straight to `ParcelableListBinder.send`, which calls `transact` on it with
    /// no null check: an empty reply is an NPE while the player is being built.
    #[test]
    fn media_session_answers_a_queue_binder() {
        let tr = TransactionData {
            target_handle: handle_for("media_session_session"),
            code: ISESSION_GET_BINDER_FOR_SET_QUEUE,
            ..TransactionData::default()
        };
        // SAFETY: `make_reply` only reads the transaction descriptor; this one
        // carries no parcel payload.
        let reply = unsafe { make_reply(&tr, 1) };
        assert_eq!(&reply.parcel[0..4], &0u32.to_ne_bytes(), "status word");
        assert_eq!(
            u32::from_ne_bytes(reply.parcel[4..8].try_into().unwrap()),
            BINDER_TYPE_HANDLE
        );
        assert_eq!(reply.objects.len(), 1, "one flat_binder_object");
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
        for name in ["media_metrics", "media.metrics"] {
            let tr = TransactionData {
                target_handle: handle_for(name),
                code: IMM_GET_PLAYBACK_SESSION_ID,
                ..TransactionData::default()
            };
            // SAFETY: `make_reply` only reads the transaction descriptor; this
            // one carries no parcel payload.
            let reply = unsafe { make_reply(&tr, 1) };
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

    /// `Settings.NameValueCache` reads `Bundle.getString(key)` from the
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
        let data: Vec<u8> = key.encode_utf16().flat_map(|u| u.to_ne_bytes()).collect();
        let mut parcel = Vec::new();
        let mut objects = Vec::new();
        provider_reply(ICP_CALL, Some(&data), &mut parcel, &mut objects);
        assert_eq!(parcel, value);

        // A key nobody installed must not borrow another key's Bundle.
        let data: Vec<u8> = "no_such_setting"
            .encode_utf16()
            .flat_map(|u| u.to_ne_bytes())
            .collect();
        let mut parcel = Vec::new();
        provider_reply(ICP_CALL, Some(&data), &mut parcel, &mut objects);
        assert_ne!(parcel, value);
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
