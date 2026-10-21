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

/// Offset of the transfer area used for reply parcels.
static REPLY_OFFSET: AtomicUsize = AtomicUsize::new(0);

/// Handles handed out to the guest for brokered services.
static SERVICE_HANDLES: std::sync::LazyLock<std::sync::Mutex<std::collections::BTreeMap<String, u32>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::BTreeMap::new()));
static NEXT_HANDLE: AtomicU32 = AtomicU32::new(1);

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

/// `IDisplayManager` transaction code for `getDisplayInfo(int)`.
const IDM_GET_DISPLAY_INFO: u32 = 1;

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
fn provider_reply(code: u32, parcel: &mut Vec<u8>, objects: &mut Vec<u64>) {
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
            let bundle = EMPTY_BUNDLE.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if bundle.is_empty() {
                parcel.extend_from_slice(&(-1i32).to_ne_bytes()); // null Bundle
            } else {
                parcel.extend_from_slice(&bundle);
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
            // `[result][outFrames][outMergedConfiguration][outSurfaceControl]
            //  [insetsState][activeControls][bundle]`. Every parcelable is
            // written as absent so `ViewRootImpl` keeps the objects it
            // pre-allocated; the array still has to match `InsetsState.SIZE`
            // exactly, and the bundle is a null `Bundle` (-1).
            parcel.extend_from_slice(&0u32.to_ne_bytes()); // result
            parcel.extend_from_slice(&0u32.to_ne_bytes()); // outFrames absent
            parcel.extend_from_slice(&0u32.to_ne_bytes()); // merged config absent
            // `outSurfaceControl`: present, so `ViewRootImpl` has something to
            // draw into. `SurfaceControl.readFromParcel` reads the Java-side
            // header (name + size + presence) and then
            // `SurfaceControl::readFromParcel`'s
            // `[client][handle][layerId][transformHint][w][h][format]`.
            parcel.extend_from_slice(&1u32.to_ne_bytes()); // present
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
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid `timespec` for `clock_gettime` to fill.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    let now = (ts.tv_sec as i64) * 1_000_000_000 + ts.tv_nsec as i64;
    event[16..24].copy_from_slice(&now.to_ne_bytes());
    // `vsync.count` is the frame number; a monotonically increasing counter is
    // what `Choreographer` expects.
    static FRAME: AtomicU32 = AtomicU32::new(0);
    let frame = FRAME.fetch_add(1, Ordering::Relaxed);
    event[24..28].copy_from_slice(&frame.to_ne_bytes());
    // `frameInterval` = 16.67 ms (60 Hz), the only timeline entry.
    event[32..40].copy_from_slice(&16_666_666i64.to_ne_bytes());

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
        unsafe { diag(&format!("[binder]   -> vsync frame {frame} dropped={dropped}\n")) };
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
    VSYNC_SEND_FDS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(ours);
    Some((fds[0], fds[1]))
}

/// Whether `data` contains `s` encoded as UTF-16LE (a `String16` payload).
fn parcel_contains_utf16(data: &[u8], s: &str) -> bool {
    let needle: Vec<u8> = s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    !needle.is_empty() && data.windows(needle.len()).any(|w| w == needle.as_slice())
}

/// Reverse lookup of a handle, for diagnostics.
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
    if tr.target_handle == handle_for("settings_provider") {
        provider_reply(tr.code, &mut parcel, &mut objects);
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
        if cmd == BC_TRANSACTION {
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
        if cmd == BC_REPLY && trace_enabled() {
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
