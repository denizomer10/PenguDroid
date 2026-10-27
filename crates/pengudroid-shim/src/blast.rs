//! `android.graphics.BLASTBufferQueue`, hosted on a `BufferQueue` we own.
//!
//! `SurfaceView` is the only way an app can put video on screen, and since
//! Android 12 it gets its surface from a `BLASTBufferQueue`: the queue is the
//! producer side of the video layer and SurfaceFlinger is the consumer. There is
//! no SurfaceFlinger here, so the class is hosted at the **JNI boundary**, the
//! same way [`crate::audiotrack`] hosts `AudioTrack`.
//!
//! `libandroid_runtime` publishes the natives with
//! `AndroidRuntime::registerNativeMethods(env, "android/graphics/BLASTBufferQueue", …)`;
//! that call is interposed and the table is patched entry by entry, so
//! `BLASTBufferQueue.java` is untouched.
//!
//! # What replaces the native object
//!
//! * `android::BufferQueue::createBufferQueue` (exported by `libgui`) makes a
//!   real producer/consumer pair — the same call the real class makes;
//! * a `BufferItemConsumer` wraps the consumer end, which is what lets us
//!   *pull* frames instead of dropping them;
//! * `android::Surface` wraps the producer, and the Java `Surface(long)`
//!   constructor wraps *that*, so the app gets a genuine `Surface` whose
//!   `queueBuffer` lands in a queue we hold the consumer end of;
//! * `update()` only records the geometry: `Surface::setBuffersFormat` and
//!   `Surface::setBuffersUserDimensions` are what `dequeueBuffer` passes down to
//!   the consumer, so no SurfaceControl transaction is needed.
//!
//! # Where the frames go
//!
//! A consumer thread acquires each queued CPU buffer and sends its memfd to the
//! daemon for a `wl_shm` snapshot on a **`wl_subsurface` of the app's window**. That is the
//! Wayland equivalent of the extra layer SurfaceFlinger would composite: the
//! video is a child surface positioned inside the parent, so the compositor
//! composites it without a GPU context in the daemon. CPU buffers are copied
//! before acknowledgement because the decoder may immediately reuse them.
//!
//! `android.view.Surface.nativeGetFromBlastBufferQueue` is hosted too: the
//! framework implementation casts the queue pointer back to a real
//! `BLASTBufferQueue*` and calls `getIGraphicBufferProducer()` on it, which
//! dereferences our struct as if it were one (measured: SIGSEGV in
//! `RefBase::incStrong`).

use std::ffi::CStr;
use std::os::raw::{c_char, c_void};
use std::sync::Mutex;

use crate::ahardwarebuffer::{GetNativeBuffer, GRAPHIC_BUFFER_GET_NATIVE_BUFFER};
use crate::audiotrack::{jni_find_class, jni_slot, NativeMethod};

// ---------------------------------------------------------------------------
// JNI plumbing
// ---------------------------------------------------------------------------

const SLOT_NEW_GLOBAL_REF: usize = 21;
const SLOT_NEW_OBJECT_A: usize = 30;
const SLOT_GET_METHOD_ID: usize = 33;

/// `jvalue`: a union of every JNI argument type, 8 bytes wide.
#[repr(C)]
#[derive(Clone, Copy)]
union JValue {
    j: i64,
    l: *mut c_void,
}

unsafe fn jni_new_global_ref(env: *mut c_void, obj: *mut c_void) -> *mut c_void {
    let f: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
        std::mem::transmute(jni_slot(env, SLOT_NEW_GLOBAL_REF));
    f(env, obj)
}

unsafe fn jni_get_method_id(
    env: *mut c_void,
    class: *mut c_void,
    name: &CStr,
    signature: &CStr,
) -> *mut c_void {
    let f: unsafe extern "C" fn(
        *mut c_void,
        *mut c_void,
        *const c_char,
        *const c_char,
    ) -> *mut c_void = std::mem::transmute(jni_slot(env, SLOT_GET_METHOD_ID));
    f(env, class, name.as_ptr(), signature.as_ptr())
}

unsafe fn jni_new_object(
    env: *mut c_void,
    class: *mut c_void,
    method: *mut c_void,
    args: &[JValue],
) -> *mut c_void {
    let f: unsafe extern "C" fn(
        *mut c_void,
        *mut c_void,
        *mut c_void,
        *const JValue,
    ) -> *mut c_void = std::mem::transmute(jni_slot(env, SLOT_NEW_OBJECT_A));
    f(env, class, method, args.as_ptr())
}

// ---------------------------------------------------------------------------
// libgui / libui entry points
// ---------------------------------------------------------------------------

/// `android::BufferQueue::createBufferQueue(sp<IGraphicBufferProducer>*,
/// sp<IGraphicBufferConsumer>*, bool consumerIsSurfaceFlinger)`.
type CreateBufferQueue = unsafe extern "C" fn(*mut [u64; 2], *mut [u64; 2], bool);
/// `BufferItemConsumer(const sp<IGraphicBufferConsumer>&, uint64_t usage,
/// int bufferCount, bool controlledByApp)`.
type ItemConsumerCtor = unsafe extern "C" fn(*mut c_void, *const [u64; 2], u64, i32, bool);
type BufferItemCtor = unsafe extern "C" fn(*mut c_void);
type BufferItemDtor = unsafe extern "C" fn(*mut c_void);
/// `BufferItemConsumer::acquireBuffer(BufferItem*, nsecs_t, bool,
/// std::optional<std::function<void(const sp<GraphicBuffer>&)>>)`.
///
/// Android 17 added the trailing libc++ `optional<function<...>>`. A class with
/// a non-trivial copy constructor/destructor is passed by *invisible reference*
/// (the Itanium ABI), so the last argument is a pointer to the caller's
/// temporary. A zeroed 32-byte block is a valid disengaged libc++
/// `optional<function>`: `__engaged_` lives at byte 24 and stays 0, and the
/// destructor then does nothing.
type AcquireBuffer =
    unsafe extern "C" fn(*mut c_void, *mut c_void, i64, bool, *mut UnusedOptional) -> i32;
/// `BufferItemConsumer::releaseBuffer(const BufferItem&, const sp<Fence>&,
/// std::optional<std::function<void(const sp<GraphicBuffer>&)>>)`.
type ReleaseBuffer =
    unsafe extern "C" fn(*mut c_void, *const c_void, *const c_void, *mut UnusedOptional) -> i32;

/// One disengaged libc++ `optional<std::function<void(const sp<GraphicBuffer>&)>>`.
///
/// `std::function` is three pointers (24 bytes); libc++ stores the engaged flag
/// at the next byte, so the whole `optional` is 32 bytes with 8-byte alignment.
/// All-zero therefore means "not engaged".
#[repr(C, align(8))]
pub struct UnusedOptional(pub [u64; 4]);

impl UnusedOptional {
    fn empty() -> Self {
        UnusedOptional([0; 4])
    }
}
type SetDefaultSize = unsafe extern "C" fn(*mut c_void, u32, u32) -> i32;
type SetDefaultFormat = unsafe extern "C" fn(*mut c_void, i32) -> i32;
type SetConsumerUsage = unsafe extern "C" fn(*mut c_void, u64) -> i32;
type SetMaxAcquired = unsafe extern "C" fn(*mut c_void, i32) -> i32;
/// `Surface(const sp<IGraphicBufferProducer>&, bool, const sp<IBinder>&)`.
type SurfaceCtor =
    unsafe extern "C" fn(*mut c_void, *const [u64; 2], bool, *const [u64; 2]);
type SurfaceIntSetter = unsafe extern "C" fn(*mut c_void, i32) -> i32;
type SurfaceDimsSetter = unsafe extern "C" fn(*mut c_void, u32, u32) -> i32;
/// `BLASTBufferQueue::update(const sp<SurfaceControl>&, uint32_t width,
/// uint32_t height, int32_t format)`.
type BbqUpdate = unsafe extern "C" fn(*mut c_void, *const *mut c_void, u32, u32, i32);
/// `BLASTBufferQueue::BLASTBufferQueue(const std::string& name,
/// bool updateDestinationFrame)`.
///
/// A libc++ `std::string` is 24 bytes in both the old and the new ABI layouts,
/// and an all-zero short-string is a valid empty string, so the name is passed
/// as 24 zeroed bytes.
type BbqCtor = unsafe extern "C" fn(*mut c_void, *const [u8; 24], bool);
/// `RefBase::incStrong(const void*) const`.
type IncStrong = unsafe extern "C" fn(*mut c_void, *const c_void);

/// Resolve a symbol from a guest library (bypassing our own interpositions).
unsafe fn guest_sym(soname: &[u8], name: &[u8]) -> *mut c_void {
    let handle = libc::dlopen(
        soname.as_ptr() as *const c_char,
        libc::RTLD_NOW | libc::RTLD_GLOBAL,
    );
    if handle.is_null() {
        return std::ptr::null_mut();
    }
    libc::dlsym(handle, name.as_ptr() as *const c_char)
}

struct Gui {
    create_buffer_queue: CreateBufferQueue,
    item_consumer_ctor: ItemConsumerCtor,
    item_ctor: BufferItemCtor,
    item_dtor: BufferItemDtor,
    get_native_buffer: GetNativeBuffer,
    acquire: AcquireBuffer,
    release: ReleaseBuffer,
    set_default_size: SetDefaultSize,
    set_default_format: SetDefaultFormat,
    set_consumer_usage: SetConsumerUsage,
    set_max_acquired: SetMaxAcquired,
    surface_ctor: SurfaceCtor,
    set_buffers_format: SurfaceIntSetter,
    set_user_dimensions: SurfaceDimsSetter,
    inc_strong: IncStrong,
    bbq_ctor: BbqCtor,
    bbq_update: BbqUpdate,
    /// `android::Fence::NO_FENCE` — the `sp<Fence>` `releaseBuffer` expects.
    /// Held as an address: raw pointers would make `Gui` neither `Send` nor
    /// `Sync`, and the value never changes after load.
    no_fence: usize,
}

static GUI: std::sync::OnceLock<Option<Gui>> = std::sync::OnceLock::new();

fn gui() -> Option<&'static Gui> {
    GUI.get_or_init(|| unsafe {
        let create_buffer_queue = guest_sym(
            b"libgui.so\0",
            b"_ZN7android11BufferQueue17createBufferQueueEPNS_2spINS_22IGraphicBufferProducerEEEPNS1_INS_22IGraphicBufferConsumerEEEb\0",
        );
        let item_consumer_ctor = guest_sym(
            b"libgui.so\0",
            b"_ZN7android18BufferItemConsumerC1ERKNS_2spINS_22IGraphicBufferConsumerEEEmib\0",
        );
        let item_ctor = guest_sym(b"libgui.so\0", b"_ZN7android10BufferItemC1Ev\0");
        let item_dtor = guest_sym(b"libgui.so\0", b"_ZN7android10BufferItemD1Ev\0");
        let get_native_buffer = guest_sym(b"libui.so\0", GRAPHIC_BUFFER_GET_NATIVE_BUFFER);
        if get_native_buffer.is_null() {
            eprintln!("[blast] libui.so is missing Android 17 GraphicBuffer::getNativeBuffer() const; private-layout fallback is unsupported");
            return None;
        }
        let acquire = guest_sym(
            b"libgui.so\0",
            b"_ZN7android18BufferItemConsumer13acquireBufferEPNS_10BufferItemElbNSt3__18optionalINS3_8functionIFvRKNS_2spINS_13GraphicBufferEEEEEEEE\0",
        );
        let release = guest_sym(
            b"libgui.so\0",
            b"_ZN7android18BufferItemConsumer13releaseBufferERKNS_10BufferItemERKNS_2spINS_5FenceEEENSt3__18optionalINS9_8functionIFvRKNS4_INS_13GraphicBufferEEEEEEEE\0",
        );
        let set_default_size = guest_sym(
            b"libgui.so\0",
            b"_ZN7android12ConsumerBase20setDefaultBufferSizeEjj\0",
        );
        let set_default_format = guest_sym(
            b"libgui.so\0",
            b"_ZN7android12ConsumerBase22setDefaultBufferFormatEi\0",
        );
        let set_consumer_usage = guest_sym(
            b"libgui.so\0",
            b"_ZN7android12ConsumerBase20setConsumerUsageBitsEm\0",
        );
        let set_max_acquired = guest_sym(
            b"libgui.so\0",
            b"_ZN7android12ConsumerBase25setMaxAcquiredBufferCountEi\0",
        );
        let surface_ctor = guest_sym(
            b"libgui.so\0",
            b"_ZN7android7SurfaceC1ERKNS_2spINS_22IGraphicBufferProducerEEEbRKNS1_INS_7IBinderEEE\0",
        );
        let set_buffers_format = guest_sym(
            b"libgui.so\0",
            b"_ZN7android7Surface16setBuffersFormatEi\0",
        );
        let set_user_dimensions = guest_sym(
            b"libgui.so\0",
            b"_ZN7android7Surface24setBuffersUserDimensionsEjj\0",
        );
        let inc_strong = guest_sym(b"libutils.so\0", b"_ZNK7android7RefBase9incStrongEPKv\0");
        let bbq_ctor = guest_sym(
            b"libgui.so\0",
            b"_ZN7android16BLASTBufferQueueC1ERKNSt3__112basic_stringIcNS1_11char_traitsIcEENS1_9allocatorIcEEEEb\0",
        );
        let bbq_update = guest_sym(
            b"libgui.so\0",
            b"_ZN7android16BLASTBufferQueue6updateERKNS_2spINS_14SurfaceControlEEEjji\0",
        );
        if create_buffer_queue.is_null()
            || item_consumer_ctor.is_null()
            || item_ctor.is_null()
            || item_dtor.is_null()
            || acquire.is_null()
            || release.is_null()
            || set_default_size.is_null()
            || set_default_format.is_null()
            || set_consumer_usage.is_null()
            || set_max_acquired.is_null()
            || surface_ctor.is_null()
            || set_buffers_format.is_null()
            || set_user_dimensions.is_null()
            || inc_strong.is_null()
            || bbq_ctor.is_null()
            || bbq_update.is_null()
        {
            eprintln!("[blast] libgui/libutils is missing a required entry point");
            return None;
        }
        Some(Gui {
            create_buffer_queue: std::mem::transmute(create_buffer_queue),
            item_consumer_ctor: std::mem::transmute(item_consumer_ctor),
            item_ctor: std::mem::transmute(item_ctor),
            item_dtor: std::mem::transmute(item_dtor),
            get_native_buffer: std::mem::transmute(get_native_buffer),
            acquire: std::mem::transmute(acquire),
            release: std::mem::transmute(release),
            set_default_size: std::mem::transmute(set_default_size),
            set_default_format: std::mem::transmute(set_default_format),
            set_consumer_usage: std::mem::transmute(set_consumer_usage),
            set_max_acquired: std::mem::transmute(set_max_acquired),
            surface_ctor: std::mem::transmute(surface_ctor),
            set_buffers_format: std::mem::transmute(set_buffers_format),
            set_user_dimensions: std::mem::transmute(set_user_dimensions),
            inc_strong: std::mem::transmute(inc_strong),
            bbq_ctor: std::mem::transmute(bbq_ctor),
            bbq_update: std::mem::transmute(bbq_update),
            no_fence: guest_sym(b"libui.so\0", b"_ZN7android5Fence8NO_FENCEE\0") as usize,
        })
    })
    .as_ref()
}

// ---------------------------------------------------------------------------
// The hosted object
// ---------------------------------------------------------------------------

/// Bytes reserved for a `libgui` object we construct ourselves.
///
/// `Surface`, `BufferItemConsumer` and `BLASTBufferQueue` have no exported
/// `sizeof`. This existing reservation still needs target-build size/alignment
/// validation; the native-buffer accessor fixture does not validate it.
const OBJECT_SIZE: usize = 8192;

/// One hosted `BLASTBufferQueue`.
///
/// android17-release StrongPointer.h stores one object pointer in `sp<T>`;
/// `producer`/`consumer` reserve an extra, unused word, not a control block.
struct Blast {
    producer: [u64; 2],
    consumer: [u64; 2],
    /// `android::BufferItemConsumer*` — the consumer end, owned by us.
    item_consumer: *mut c_void,
    /// `android::Surface*` — the native half of the Java `Surface` we hand out.
    surface: *mut c_void,
    /// A **real** `android::BLASTBufferQueue*`.
    ///
    /// Android 17's hwui calls `syncNextTransaction`, `applyPendingTransactions`,
    /// `mergeWithNextTransaction`, `gatherPendingTransactions` and the two
    /// callback setters on whatever `BLASTBufferQueue.nativeCreate` returned, so
    /// that value has to be a genuine object with those methods' state. Its own
    /// buffer queue stays idle: the frames are still produced and consumed
    /// through `surface`/`item_consumer` above, because there is no
    /// SurfaceFlinger to release them.
    bbq: *mut c_void,
    /// Global reference to that Java `Surface`, created on first request.
    java_state: Mutex<JavaState>,
    stopped: std::sync::atomic::AtomicBool,
    /// The daemon surface the consumer thread presents frames on (0 until the
    /// first frame).
    video_surface: std::sync::atomic::AtomicU32,
    /// SurfaceView layout size, independent of the decoder's buffer dimensions.
    display_size: std::sync::atomic::AtomicU64,
}

// JNI metadata is serialized; the consumer reads only immutable native
// pointers and atomics. Native BufferQueue methods provide their own locking.
struct JavaState {
    java_surface: usize,
    width: u32,
    height: u32,
    format: i32,
}

/// Hosted objects, kept alive for the life of the process.
///
/// Java and hwui hold the real `BLASTBufferQueue*`; this table maps that native
/// handle to the separately allocated Rust state used by the consumer thread.
static BLASTS: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());

unsafe fn blast_at(ptr: i64) -> Option<&'static Blast> {
    if ptr == 0 {
        return None;
    }
    let state = BLASTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find_map(|&(native, state)| (native == ptr as usize).then_some(state))?;
    Some(&*(state as *const Blast))
}

/// `BLASTBufferQueue.nativeCreate(String name, boolean updateDestinationFrame)`
/// → `long`.
///
/// The signature is read back from the class at registration time (see
/// [`__pengudroid_register_blast_buffer_queue`]); this release passes only the
/// name and the flag, and the geometry arrives later through `nativeUpdate`.
unsafe extern "C" fn native_create(
    _env: *mut c_void,
    _class: *mut c_void,
    _name: *mut c_void,
    _update_destination_frame: u8,
) -> i64 {
    // A 1x1 default keeps `dequeueBuffer` legal until `update` arrives.
    let (width, height, format) = (1u32, 1u32, 0i32);
    let Some(gui) = gui() else {
        return 0;
    };

    // A real producer/consumer pair. `consumerIsSurfaceFlinger = false` keeps
    // the queue in "app" mode (no SurfaceFlinger-only buffer accounting).
    let mut producer = [0u64; 2];
    let mut consumer = [0u64; 2];
    (gui.create_buffer_queue)(&mut producer, &mut consumer, false);
    if producer[0] == 0 || consumer[0] == 0 {
        eprintln!("[blast] BufferQueue::createBufferQueue returned nothing");
        return 0;
    }

    // The consumer end is what we pull frames from. `USAGE_HW_TEXTURE |
    // USAGE_HW_COMPOSER` is what SurfaceFlinger would ask for; the allocator
    // ignores it, but the decoder reads it back.
    let usage: u64 = 0x100 | 0x200; // GRALLOC_USAGE_HW_TEXTURE | HW_COMPOSER
    // These are C++ objects whose real sizes are not exported. Over-allocating
    // is the only safe option: an undersized block corrupts the heap when the
    // constructor writes past it (measured: `sysmalloc: assertion failed`).
    let item_consumer = libc::calloc(1, OBJECT_SIZE);
    if item_consumer.is_null() {
        return 0;
    }
    (gui.item_consumer_ctor)(item_consumer, &consumer, usage, 4, false);
    // `ConsumerBase` is a `RefBase`, and `decStrong` on the last reference runs
    // `delete this` — which would `free()` a block we got from `calloc` and
    // leave `Blast::item_consumer` dangling (measured: SIGSEGV inside
    // `BufferItemConsumer::acquireBuffer`, jumping into `main_arena`). Take a
    // reference of our own so the object outlives every consumer of it.
    keep_alive(gui, "item_consumer", item_consumer, None);
    if std::env::var_os("PENGUDROID_TRACE_VIDEO").is_some() {
        // The vtable pointer is the cheapest proof that the constructor really
        // ran: `BufferItemConsumer::acquireBuffer` calls through it, and a
        // garbage value there is a SIGSEGV inside libgui.
        let vtable = *(item_consumer as *const usize);
        eprintln!(
            "[blast] item_consumer={:#x} vtable={vtable:#x} consumer={:#x}",
            item_consumer as usize, consumer[0] as usize
        );
    }
    (gui.set_default_size)(item_consumer, width, height);
    (gui.set_default_format)(item_consumer, format);
    (gui.set_consumer_usage)(item_consumer, usage);
    (gui.set_max_acquired)(item_consumer, 2);

    // The producer side, wrapped in a real `Surface`.
    let surface = libc::calloc(1, OBJECT_SIZE);
    if surface.is_null() {
        return 0;
    }
    (gui.surface_ctor)(surface, &producer, true, &EMPTY_SP);
    // Same reason as `item_consumer`: `Surface` is a `RefBase`, so the last
    // `decStrong` would `delete this` on a `calloc` block. The reference we take
    // here is the one `Blast::surface` relies on.
    keep_alive(gui, "surface", surface, Some(0));
    (gui.set_buffers_format)(surface, format);
    (gui.set_user_dimensions)(surface, width, height);

    // A real `BLASTBufferQueue`. The constructor only records its name; the
    // queue is created from `onFirstRef()` → `initialize()`, which `incStrong`
    // triggers. `initialize()` asks SurfaceFlinger for the maximum acquired
    // buffer count over binder, so the broker answers that call.
    let bbq = libc::calloc(1, OBJECT_SIZE);
    if bbq.is_null() {
        return 0;
    }
    let empty_name = [0u8; 24];
    (gui.bbq_ctor)(bbq, &empty_name, false);
    // `BLASTBufferQueue : public ConsumerBase::FrameAvailableListener`, and that
    // listener derives from `virtual RefBase`, so `RefBase` is *not* at offset 0.
    // `keep_alive` reads the vtable's vbase entry, and its `incStrong` is also
    // what triggers `onFirstRef()` → `initialize()` (which creates the queue and
    // asks the broker for the max acquired buffer count).
    if !keep_alive(gui, "bbq", bbq, None) {
        eprintln!("[blast] BLASTBufferQueue is not refcounted as expected");
        return 0;
    }

    let blast = Box::new(Blast {
        producer,
        consumer,
        item_consumer,
        surface,
        bbq,
        java_state: Mutex::new(JavaState { java_surface: 0, width, height, format }),
        stopped: std::sync::atomic::AtomicBool::new(false),
        video_surface: std::sync::atomic::AtomicU32::new(0),
        display_size: std::sync::atomic::AtomicU64::new(((width as u64) << 32) | height as u64),
    });
    let ptr = Box::into_raw(blast);
    // Keyed by the value Java (and therefore hwui) holds: the real queue.
    BLASTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((bbq as usize, ptr as usize));
    eprintln!(
        "[blast] created {width}x{height} fmt={format:#x} producer={:#x} surface={:#x} bbq={:#x}",
        producer[0] as usize, surface as usize, bbq as usize
    );
    start_consumer(ptr);
    bbq as i64
}

/// `BLASTBufferQueue.nativeGetSurface(long ptr, boolean includeHandle)` →
/// `android.view.Surface`.
///
/// The Java object is built once with `Surface(long)`, which is exactly how the
/// framework wraps a native `Surface*`.
unsafe extern "C" fn native_get_surface(
    env: *mut c_void,
    _class: *mut c_void,
    ptr: i64,
    _include_handle: u8,
) -> *mut c_void {
    let Some(blast) = blast_at(ptr) else {
        return std::ptr::null_mut();
    };
    let mut state = blast.java_state.lock().unwrap_or_else(|e| e.into_inner());
    if blast.stopped.load(std::sync::atomic::Ordering::Acquire) {
        return std::ptr::null_mut();
    }
    if state.java_surface != 0 {
        return state.java_surface as *mut c_void;
    }
    let surface_class = jni_find_class(env, c"android/view/Surface");
    if surface_class.is_null() {
        eprintln!("[blast] android/view/Surface not found");
        return std::ptr::null_mut();
    }
    let ctor = jni_get_method_id(env, surface_class, c"<init>", c"(J)V");
    if ctor.is_null() {
        eprintln!("[blast] Surface(long) constructor missing");
        return std::ptr::null_mut();
    }
    // `Surface(long)` does not take a reference; the Java object owns one.
    if let Some(gui) = gui() {
        keep_alive(gui, "surface(handout)", blast.surface, Some(0));
    }
    let args = [JValue {
        j: blast.surface as i64,
    }];
    let local = jni_new_object(env, surface_class, ctor, &args);
    if local.is_null() {
        eprintln!("[blast] new Surface(long) threw");
        return std::ptr::null_mut();
    }
    state.java_surface = jni_new_global_ref(env, local) as usize;
    eprintln!("[blast] handed out a Surface for {:#x}", blast.surface as usize);
    state.java_surface as *mut c_void
}

/// `BLASTBufferQueue.nativeUpdate(long ptr, long surfaceControl, long width,
/// long height, int format)`.
///
/// The real implementation also queues a SurfaceControl transaction; there is no
/// SurfaceFlinger to receive it. The geometry is what the producer needs, and
/// `Surface` carries it into `dequeueBuffer` itself.
unsafe extern "C" fn native_update(
    _env: *mut c_void,
    _class: *mut c_void,
    ptr: i64,
    surface_control: i64,
    width: i64,
    height: i64,
    format: i32,
) {
    let Some(gui) = gui() else {
        return;
    };
    let Some(blast) = blast_at(ptr) else {
        eprintln!("[blast] update: unknown queue {ptr:#x}");
        return;
    };
    eprintln!("[blast] update: enter ptr={ptr:#x} sc={surface_control:#x} {width}x{height} fmt={format:#x}");
    let mut state = blast.java_state.lock().unwrap_or_else(|e| e.into_inner());
    if blast.stopped.load(std::sync::atomic::Ordering::Acquire) { return; }
    let (w, h) = (width.max(1) as u32, height.max(1) as u32);

    // Drive the real queue too: hwui reads its geometry back and uses it for the
    // sync-transaction path. `update` takes `const sp<SurfaceControl>&`, so the
    // argument is a pointer to one pointer (an `sp` is a single word here) and
    // the assignment inside the callee takes the reference.
    if !blast.bbq.is_null() && surface_control != 0 {
        let sc = surface_control as *mut c_void;
        eprintln!("[blast] update: calling real BLASTBufferQueue::update");
        (gui.bbq_update)(blast.bbq, &sc, w, h, format);
        eprintln!("[blast] update: real update returned");
    }

    if format != state.format {
        (gui.set_buffers_format)(blast.surface, format);
        (gui.set_default_format)(blast.item_consumer, format);
        state.format = format;
    }
    if w != state.width || h != state.height {
        (gui.set_user_dimensions)(blast.surface, w, h);
        (gui.set_default_size)(blast.item_consumer, w, h);
        state.width = w;
        state.height = h;
    }
    blast.display_size.store(((w as u64) << 32) | h as u64, std::sync::atomic::Ordering::Release);
    eprintln!("[blast] update {w}x{h} fmt={format:#x}");
}

/// `BLASTBufferQueue.nativeDestroy(long ptr)`.
///
/// Keep native storage alive while Java may hold its Surface, but stop polling
/// and retire the daemon layer. The consumer owns layer cleanup, so destruction
/// never races a pending present or blocks the Java UI thread on IPC.
unsafe extern "C" fn native_destroy(_env: *mut c_void, _class: *mut c_void, ptr: i64) {
    if let Some(blast) = blast_at(ptr) {
        let _state = blast.java_state.lock().unwrap_or_else(|e| e.into_inner());
        blast.stopped.store(true, std::sync::atomic::Ordering::Release);
    }
}

/// The remaining natives are pure SurfaceFlinger bookkeeping.
unsafe extern "C" fn native_noop(
    _env: *mut c_void,
    _class: *mut c_void,
    _a: i64,
    _b: i64,
    _c: i64,
    _d: i64,
) {
}

unsafe extern "C" fn native_zero(
    _env: *mut c_void,
    _class: *mut c_void,
    _a: i64,
    _b: i64,
    _c: i64,
    _d: i64,
) -> i64 {
    0
}

/// `android.view.Surface.nativeGetFromBlastBufferQueue(long surfacePtr, long
/// queuePtr)` → `long`.
///
/// `Surface.copyFrom(BLASTBufferQueue)` calls this and stores the result in
/// `mNativeObject`. The framework implementation casts `queuePtr` to a real
/// `BLASTBufferQueue*` and calls `getIGraphicBufferProducer()` on it; our
/// object is not one, so the call is hosted and answers with the `Surface` we
/// already built.
unsafe extern "C" fn surface_from_blast(
    _env: *mut c_void,
    _class: *mut c_void,
    _surface_ptr: i64,
    queue_ptr: i64,
) -> i64 {
    let Some(blast) = blast_at(queue_ptr) else {
        eprintln!("[blast] nativeGetFromBlastBufferQueue: unknown queue {queue_ptr:#x}");
        return 0;
    };
    if blast.stopped.load(std::sync::atomic::Ordering::Acquire) { return 0; }
    // The Java `Surface` takes ownership of this reference and releases it in
    // `nativeRelease`, so hand out a new one.
    if let Some(gui) = gui() {
        keep_alive(gui, "surface(copyFrom)", blast.surface, Some(0));
    }
    eprintln!(
        "[blast] Surface.copyFrom(queue) -> {:#x}",
        blast.surface as usize
    );
    blast.surface as i64
}

/// Identity used for every `incStrong`/`decStrong` pair we make.
static OWNER_ID: u8 = 0;

/// The `RefBase` subobject of a `libgui` object.
///
/// android17-release declares ConsumerBase's RefBase virtual and Surface's
/// RefBase non-virtual. The v[-3] lookup below is still private C++ ABI coupling,
/// not an ANativeWindowBuffer offset, and needs target-build validation.
unsafe fn refbase_of(obj: *mut c_void) -> *mut c_void {
    if obj.is_null() {
        return obj;
    }
    let vptr = *(obj as *const *const u8);
    if vptr.is_null() {
        return obj;
    }
    let off = *(vptr.sub(24) as *const i32);
    (obj as *mut u8).offset(off as isize) as *mut c_void
}

/// Take a strong reference on a `libgui` object we constructed ourselves.
///
/// Without it the last `decStrong` (Java's `nativeRelease`) runs `delete this`
/// on a block that came from `calloc` and leaves our raw pointer dangling —
/// measured as `Surface::hook_perform` aborting inside `lock_shared()` on freed
/// memory. Returns false (and logs) when the object has no `mRefs`, which means
/// the constructor did not run and calling `incStrong` would fault.
unsafe fn keep_alive(gui: &Gui, what: &str, obj: *mut c_void, fixed_offset: Option<i32>) -> bool {
    let vptr = *(obj as *const *const u8);
    // Remaining private C++ ABI dependencies: Surface's RefBase at +0, the
    // virtual-base entry at v[-3], and RefBase::mRefs at +8 below. The Android 17
    // inheritance declarations are source-confirmed, but the native-buffer
    // fixture does not establish these target-build offsets.
    let off = match fixed_offset {
        Some(off) => off,
        None if vptr.is_null() => 0,
        None => *(vptr.sub(24) as *const i32),
    };
    // A vbase offset outside the object we allocated is not a `RefBase` at all;
    // calling `incStrong` there faults. Bail instead of guessing.
    let trace = std::env::var_os("PENGUDROID_TRACE_VIDEO").is_some();
    if off < 0 || off as usize >= OBJECT_SIZE - 16 {
        if trace {
            eprintln!("[blast] {what}: implausible RefBase offset {off}; skipping");
        }
        return false;
    }
    let rb = (obj as *mut u8).offset(off as isize) as *mut c_void;
    let refs = *(rb.add(8) as *const usize);
    if refs == 0 {
        eprintln!("[blast] {what}: no mRefs at {rb:p}; not taking a reference");
        return false;
    }
    if trace {
        eprintln!("[blast] {what}: RefBase at +{off} ({rb:p}) mRefs={refs:#x}");
    }
    (gui.inc_strong)(rb, &OWNER_ID as *const u8 as *const c_void);
    true
}



// ---------------------------------------------------------------------------
// Frame consumer → daemon
// ---------------------------------------------------------------------------

/// `DRM_FORMAT_*` fourcc for an Android `HAL_PIXEL_FORMAT_*`, plus bytes per
/// pixel. The daemon imports the buffer as a linear dma-buf, so the fourcc has
/// to describe the byte order the decoder actually wrote.
fn fourcc_for(format: i32) -> (u32, u32) {
    match format {
        4 => (0x3631_4752, 2),          // RGB_565  -> DRM_FORMAT_RGB565
        1 => (0x3432_4241, 4),          // RGBA_8888 -> DRM_FORMAT_ABGR8888
        0x22 => (0x3432_4258, 4),       // RGBX_8888 -> DRM_FORMAT_XBGR8888
        0x16 => (0x4834_4241, 8),       // RGBA_F16 -> DRM_FORMAT_ABGR16161616F
        _ => (0x3432_4241, 4),
    }
}

fn video_origin() -> (i32, i32) {
    let read = |name: &str| -> i32 {
        std::env::var(name)
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0)
    };
    (read("PENGUDROID_VIDEO_X"), read("PENGUDROID_VIDEO_Y"))
}

/// Layer IPC stays on the consumer thread, including layout replacement and
/// final destruction. Decoder buffer size is independent of this layout.
#[derive(Default)]
struct VideoLayer {
    id: u32,
    layout: (u32, u32),
}

impl VideoLayer {
    fn retire(&mut self, conn: &pengudroid_socket::Connection) -> Result<(), String> {
        use pengudroid_proto::Message;
        if self.id == 0 { return Ok(()); }
        conn.send_message(&Message::DestroySurface { surface_id: self.id }, &[])
            .map_err(|e| e.to_string())?;
        match conn.recv_message().map_err(|e| e.to_string())?.0 {
            Message::Ack => { self.id = 0; Ok(()) }
            reply => Err(format!("DestroySurface: {reply:?}")),
        }
    }

    fn ensure(&mut self, conn: &pengudroid_socket::Connection,
        layout: (u32, u32), x: i32, y: i32) -> Result<(), String> {
        use pengudroid_proto::Message;
        if self.id != 0 && self.layout == layout { return Ok(()); }
        self.retire(conn)?;
        conn.send_message(&Message::CreateSubsurface {
            parent_pid: std::process::id() as i32, x, y,
            width: layout.0, height: layout.1,
        }, &[]).map_err(|e| e.to_string())?;
        match conn.recv_message().map_err(|e| e.to_string())?.0 {
            Message::SubsurfaceCreated { surface_id } => {
                self.id = surface_id;
                self.layout = layout;
                eprintln!("[blast] video surface {surface_id} at {x},{y} layout={}x{}", layout.0, layout.1);
                Ok(())
            }
            reply => Err(format!("CreateSubsurface: {reply:?}")),
        }
    }
}

/// Start the thread that drains the queue and presents to the daemon.
unsafe fn start_consumer(blast: *mut Blast) {
    let addr = blast as usize;
    std::thread::Builder::new()
        .name("pengudroid-video".into())
        .spawn(move || consumer_loop(addr))
        .ok();
}

fn consumer_loop(addr: usize) {
    use pengudroid_proto::{default_socket_path, Message, Role, PROTO_VERSION};
    use pengudroid_socket::Connection;

    let trace = std::env::var_os("PENGUDROID_TRACE_VIDEO").is_some();
    let Some(gui) = gui() else { return };
    let blast = unsafe { &*(addr as *const Blast) };

    // The daemon may not be up yet when the first `SurfaceView` appears; retry
    // for a while rather than giving up on the first refusal.
    let mut conn = None;
    for _ in 0..50 {
        if blast.stopped.load(std::sync::atomic::Ordering::Acquire) {
            if trace { eprintln!("[blast] video: consumer stopped"); }
            return;
        }
        if let Ok(c) = Connection::connect_endpoint(&default_socket_path()) {
            let hello = Message::Hello {
                version: PROTO_VERSION,
                role: Role::Guest,
                pid: std::process::id() as i32,
            };
            if c.send_message(&hello, &[]).is_ok() {
                if let Ok((Message::Welcome { .. }, _)) = c.recv_message() {
                    conn = Some(c);
                    break;
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let Some(conn) = conn else {
        eprintln!("[blast] video: daemon unavailable; frames will be dropped");
        return;
    };

    // Opaque BufferItem storage, constructed once and assigned by libgui.
    // This existing 4 KiB capacity also needs target-build sizeof validation.
    let mut item = vec![0u64; 512];
    let item_ptr = item.as_mut_ptr() as *mut c_void;
    unsafe { (gui.item_ctor)(item_ptr) };
    if trace {
        let vtable = unsafe { *(blast.item_consumer as *const usize) };
        eprintln!(
            "[blast] video: consumer thread sees item_consumer={:#x} vtable={vtable:#x}",
            blast.item_consumer as usize
        );
    }

    let (x, y) = video_origin();
    let mut layer = VideoLayer::default();
    let mut announced = false;
    let mut presented = 0u64;

    while !blast.stopped.load(std::sync::atomic::Ordering::Acquire) {
        let rc = unsafe {
            let mut unused = UnusedOptional::empty();
            (gui.acquire)(blast.item_consumer, item_ptr, 0, true, &mut unused)
        };
        if rc != 0 {
            // `NO_BUFFER_AVAILABLE`/`TIMED_OUT` — nothing queued yet.
            std::thread::sleep(std::time::Duration::from_millis(5));
            continue;
        }
        // android17-release BufferItem.h: mGraphicBuffer is the first member,
        // Flattenable<BufferItem> is empty, and StrongPointer.h's sp<T> holds a
        // single T*. No private GraphicBuffer fields are read here.
        let gb = unsafe { *(item.as_ptr() as *const *mut c_void) };

        if !gb.is_null() {
            let handle = unsafe { crate::ahardwarebuffer::graphicbuffer_handle(gb, gui.get_native_buffer) };
            if !handle.is_null() {
                let h = unsafe { &*handle };
                if h.num_fds >= 1 {
                    let fd = h.data[0];
                    let stride = h.data[1].max(1) as u32;
                    let format = h.data[2];
                    let width = h.data[3].max(1) as u32;
                    let height = h.data[4].max(1) as u32;
                    let (_fourcc, bpp) = fourcc_for(format);

                    let display = blast.display_size.load(std::sync::atomic::Ordering::Acquire);
                    let layout = (((display >> 32) as u32).max(1), (display as u32).max(1));
                    let surface_id = match layer.ensure(&conn, layout, x, y) {
                        Ok(()) => layer.id,
                        Err(e) => {
                            eprintln!("[blast] video: layer setup failed: {e}");
                            std::thread::sleep(std::time::Duration::from_millis(150));
                            0
                        }
                    };
                    blast.video_surface.store(surface_id, std::sync::atomic::Ordering::Release);

                    if surface_id != 0 {
                        // Our GraphicBuffer allocator returns a CPU memfd, not
                        // a DMA_BUF fd. Importing it through linux-dmabuf kills
                        // the compositor connection. Snapshot via wl_shm instead.
                        let msg = Message::PresentShm {
                            surface_id, width, height, stride: stride * bpp,
                            format: format as u32,
                        };
                        match conn.send_message(&msg, &[fd]).and_then(|_| conn.recv_message()) {
                            Ok((Message::Ack, _)) => {
                                presented += 1;
                                if let Ok(prefix) = std::env::var("PENGUDROID_DUMP_VIDEO") {
                                    let every: u64 = std::env::var("PENGUDROID_DUMP_VIDEO_EVERY")
                                        .ok()
                                        .and_then(|v| v.trim().parse().ok())
                                        .unwrap_or(30);
                                    if presented % every == 0 || presented == 1 {
                                        unsafe { dump_rgb565(
                                            &prefix,
                                            fd,
                                            width,
                                            height,
                                            stride,
                                            format,
                                            presented,
                                        ) };
                                    }
                                }
                                if !announced {
                                    announced = true;
                                    eprintln!(
                                        "[blast] video: first software frame submitted ({width}x{height} format={format})"
                                    );
                                } else if trace && presented % 30 == 0 {
                                    eprintln!("[blast] video: {presented} frames");
                                }
                            }
                            Ok((Message::Error { message, .. }, _)) => {
                                eprintln!("[blast] video: present rejected: {message}");
                                layer.id = 0;
                                blast.video_surface.store(0, std::sync::atomic::Ordering::Release);
                            }
                            Err(e) => {
                                eprintln!("[blast] video: present failed: {e}");
                                layer.id = 0;
                                blast.video_surface.store(0, std::sync::atomic::Ordering::Release);
                            }
                            Ok(_) => {}
                        }
                    }
                }
            } else if trace {
                eprintln!("[blast] video: no supported shim-owned native handle for {gb:p}");
            }
        }

        // Let libgui read mSlot from its own BufferItem; there is no Rust slot
        // offset to maintain. The acquired item keeps the borrowed handle/fd
        // alive through PresentShm's snapshot ACK above. An empty sp<Fence> is
        // not Fence::NO_FENCE: pass the real static to addReleaseFenceLocked.
        // sp<Fence> is one pointer in Android 17. Pass its address opaquely;
        // do not form a two-word Rust reference over the guest's static.
        let no_fence = if gui.no_fence == 0 {
            EMPTY_SP.as_ptr() as *const c_void
        } else {
            gui.no_fence as *const c_void
        };
        unsafe {
            let mut unused = UnusedOptional::empty();
            (gui.release)(blast.item_consumer, item_ptr, no_fence, &mut unused)
        };
    }
    if let Err(e) = layer.retire(&conn) {
        eprintln!("[blast] video: layer cleanup failed: {e}");
    }
    blast.video_surface.store(0, std::sync::atomic::Ordering::Release);
    unsafe { (gui.item_dtor)(item_ptr) };
    if trace { eprintln!("[blast] video: consumer stopped"); }
}

/// Write one queued video buffer as a binary PPM (P6).
///
/// This is the honest check that the frames are real: the daemon composites the
/// subsurface, but a screenshot needs a compositor-specific tool, while the
/// buffer itself is a plain memfd we can read.
unsafe fn dump_rgb565(
    prefix: &str,
    fd: i32,
    width: u32,
    height: u32,
    stride: u32,
    format: i32,
    index: u64,
) {
    let bpp: usize = if format == 4 { 2 } else { 4 };
    let size = stride as usize * height as usize * bpp;
    let mut raw = vec![0u8; size];
    let dup = libc::dup(fd);
    if dup < 0 {
        return;
    }
    let mut got = 0usize;
    while got < size {
        let n = libc::pread(
            dup,
            raw[got..].as_mut_ptr() as *mut c_void,
            size - got,
            got as libc::off_t,
        );
        if n <= 0 {
            break;
        }
        got += n as usize;
    }
    libc::close(dup);
    if got < size {
        return;
    }
    let mut out = Vec::with_capacity(size / bpp * 3 + 32);
    out.extend_from_slice(format!("P6\n{width} {height}\n255\n").as_bytes());
    for y in 0..height as usize {
        for x in 0..width as usize {
            let o = y * stride as usize * bpp + x * bpp;
            if format == 4 {
                let v = u16::from_le_bytes([raw[o], raw[o + 1]]);
                let r = ((v >> 11) & 0x1f) as u8;
                let g = ((v >> 5) & 0x3f) as u8;
                let b = (v & 0x1f) as u8;
                out.push((r << 3) | (r >> 2));
                out.push((g << 2) | (g >> 4));
                out.push((b << 3) | (b >> 2));
            } else {
                out.push(raw[o]);
                out.push(raw[o + 1]);
                out.push(raw[o + 2]);
            }
        }
    }
    let path = format!("{prefix}-{index}.ppm");
    if std::fs::write(&path, out).is_ok() {
        eprintln!("[blast] video: wrote {path}");
    }
}

/// An empty `sp<Fence>`: `releaseBuffer` accepts it and treats it as "no fence".
static EMPTY_SP: [u64; 2] = [0, 0];

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// The `class → method → implementation` entries this module owns.
pub(crate) fn override_for(class: &str, method: &str) -> Option<*mut c_void> {
    if class != "android/graphics/BLASTBufferQueue" {
        return None;
    }
    let f: *mut c_void = match method {
        "nativeCreate" => native_create as *mut _,
        "nativeGetSurface" => native_get_surface as *mut _,
        "nativeUpdate" => native_update as *mut _,
        "nativeDestroy" => native_destroy as *mut _,
        "nativeGetLastAcquiredFrameNum" => native_zero as *mut _,
        "nativeIsSameSurfaceControl" => native_zero as *mut _,
        _ => native_noop as *mut _,
    };
    Some(f)
}

/// Whether this module hosts `class`.
pub(crate) fn hosts(class: &str) -> bool {
    class == "android/graphics/BLASTBufferQueue"
}

// `register_android_graphics_BLASTBufferQueue` lives *inside*
// `libandroid_runtime`, and `RegisterMethodsOrDie` is statically linked into it,
// so neither call goes through the PLT and the `registerNativeMethods` hook in
// [`crate::audiotrack`] never sees this class. The host therefore registers it
// itself: the CLI skips the real `register_android_graphics_BLASTBufferQueue`
// and calls this instead.
//
// The signatures are read back from the class with reflection rather than
// hard-coded, because `RegisterNatives` rejects the whole table on a single
// mismatch and the exact strings differ between releases.

const SLOT_CALL_OBJECT_METHOD_A: usize = 36;
const SLOT_GET_ARRAY_LENGTH: usize = 171;
const SLOT_GET_OBJECT_ARRAY_ELEMENT: usize = 173;
const SLOT_GET_STRING_UTF_CHARS: usize = 169;
const SLOT_RELEASE_STRING_UTF_CHARS: usize = 170;
const SLOT_REGISTER_NATIVES: usize = 215;

unsafe fn jni_call_object(
    env: *mut c_void,
    obj: *mut c_void,
    method: *mut c_void,
    args: &[JValue],
) -> *mut c_void {
    let f: unsafe extern "C" fn(
        *mut c_void,
        *mut c_void,
        *mut c_void,
        *const JValue,
    ) -> *mut c_void = std::mem::transmute(jni_slot(env, SLOT_CALL_OBJECT_METHOD_A));
    f(env, obj, method, args.as_ptr())
}

unsafe fn jni_array_length(env: *mut c_void, array: *mut c_void) -> i32 {
    let f: unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32 =
        std::mem::transmute(jni_slot(env, SLOT_GET_ARRAY_LENGTH));
    f(env, array)
}

unsafe fn jni_array_get(env: *mut c_void, array: *mut c_void, index: i32) -> *mut c_void {
    let f: unsafe extern "C" fn(*mut c_void, *mut c_void, i32) -> *mut c_void =
        std::mem::transmute(jni_slot(env, SLOT_GET_OBJECT_ARRAY_ELEMENT));
    f(env, array, index)
}

unsafe fn jni_string(env: *mut c_void, s: *mut c_void) -> String {
    if s.is_null() {
        return String::new();
    }
    let get: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut u8) -> *const c_char =
        std::mem::transmute(jni_slot(env, SLOT_GET_STRING_UTF_CHARS));
    let release: unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_char) =
        std::mem::transmute(jni_slot(env, SLOT_RELEASE_STRING_UTF_CHARS));
    let mut is_copy: u8 = 0;
    let p = get(env, s, &mut is_copy);
    if p.is_null() {
        return String::new();
    }
    let out = CStr::from_ptr(p).to_string_lossy().into_owned();
    release(env, s, p);
    out
}

/// JNI type descriptor for a `java.lang.Class` name.
fn descriptor(name: &str) -> String {
    match name {
        "void" => "V".into(),
        "boolean" => "Z".into(),
        "byte" => "B".into(),
        "char" => "C".into(),
        "short" => "S".into(),
        "int" => "I".into(),
        "long" => "J".into(),
        "float" => "F".into(),
        "double" => "D".into(),
        other if other.starts_with('[') => other.replace('.', "/"),
        other => format!("L{};", other.replace('.', "/")),
    }
}

/// Register this module's natives for `android.graphics.BLASTBufferQueue`.
///
/// # Safety
/// `env` must be a valid `JNIEnv*` for the current thread.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_register_blast_buffer_queue(env: *mut c_void) -> i32 {
    let class = jni_find_class(env, c"android/graphics/BLASTBufferQueue");
    if class.is_null() {
        eprintln!("[blast] android/graphics/BLASTBufferQueue not found");
        return -1;
    }
    let class_cls = jni_find_class(env, c"java/lang/Class");
    let method_cls = jni_find_class(env, c"java/lang/reflect/Method");
    if class_cls.is_null() || method_cls.is_null() {
        return -1;
    }
    let get_declared = jni_get_method_id(
        env,
        class_cls,
        c"getDeclaredMethods",
        c"()[Ljava/lang/reflect/Method;",
    );
    let get_name = jni_get_method_id(env, method_cls, c"getName", c"()Ljava/lang/String;");
    let get_params = jni_get_method_id(
        env,
        method_cls,
        c"getParameterTypes",
        c"()[Ljava/lang/Class;",
    );
    let get_return = jni_get_method_id(env, method_cls, c"getReturnType", c"()Ljava/lang/Class;");
    let class_get_name = jni_get_method_id(env, class_cls, c"getName", c"()Ljava/lang/String;");
    if get_declared.is_null()
        || get_name.is_null()
        || get_params.is_null()
        || get_return.is_null()
        || class_get_name.is_null()
    {
        return -1;
    }
    let methods = jni_call_object(env, class, get_declared, &[]);
    if methods.is_null() {
        return -1;
    }

    // `RegisterNatives` keeps the pointers, so the strings must outlive the call.
    let mut names: Vec<std::ffi::CString> = Vec::new();
    let mut sigs: Vec<std::ffi::CString> = Vec::new();
    let mut table: Vec<NativeMethod> = Vec::new();
    for i in 0..jni_array_length(env, methods) {
        let m = jni_array_get(env, methods, i);
        if m.is_null() {
            continue;
        }
        let name = jni_string(env, jni_call_object(env, m, get_name, &[]));
        if !name.starts_with("native") {
            continue;
        }
        let Some(fn_ptr) = override_for("android/graphics/BLASTBufferQueue", &name) else {
            continue;
        };
        let mut sig = String::from("(");
        let params = jni_call_object(env, m, get_params, &[]);
        if !params.is_null() {
            for j in 0..jni_array_length(env, params) {
                let p = jni_array_get(env, params, j);
                let pname = jni_string(env, jni_call_object(env, p, class_get_name, &[]));
                sig.push_str(&descriptor(&pname));
            }
        }
        sig.push(')');
        let ret = jni_call_object(env, m, get_return, &[]);
        let rname = jni_string(env, jni_call_object(env, ret, class_get_name, &[]));
        sig.push_str(&descriptor(&rname));

        let Ok(cname) = std::ffi::CString::new(name.clone()) else {
            continue;
        };
        let Ok(csig) = std::ffi::CString::new(sig.clone()) else {
            continue;
        };
        names.push(cname);
        sigs.push(csig);
        table.push(NativeMethod {
            name: names.last().unwrap().as_ptr(),
            signature: sigs.last().unwrap().as_ptr(),
            fn_ptr,
        });
        eprintln!("[blast] hosting {name}{sig}");
    }
    if table.is_empty() {
        eprintln!("[blast] no natives to host");
        return -1;
    }
    let register: unsafe extern "C" fn(*mut c_void, *mut c_void, *const NativeMethod, i32) -> i32 =
        std::mem::transmute(jni_slot(env, SLOT_REGISTER_NATIVES));
    let rc = register(env, class, table.as_ptr(), table.len() as i32);
    eprintln!("[blast] registered {} native(s) (rc={rc})", table.len());
    rc
}

/// Override `android.view.Surface.nativeGetFromBlastBufferQueue`.
///
/// The real table is registered by `register_android_view_Surface` (which the
/// CLI still calls, because every other `Surface` native is needed); this runs
/// afterwards and replaces the single entry. `RegisterNatives` accepts a
/// partial table and overwrites existing entries, so nothing else is touched.
///
/// # Safety
/// `env` must be a valid `JNIEnv*` for the current thread.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_register_surface_overrides(env: *mut c_void) -> i32 {
    let class = jni_find_class(env, c"android/view/Surface");
    if class.is_null() {
        eprintln!("[blast] android/view/Surface not found");
        return -1;
    }
    let name = c"nativeGetFromBlastBufferQueue";
    let sig = c"(JJ)J";
    let table = [NativeMethod {
        name: name.as_ptr(),
        signature: sig.as_ptr(),
        fn_ptr: surface_from_blast as *mut c_void,
    }];
    let register: unsafe extern "C" fn(*mut c_void, *mut c_void, *const NativeMethod, i32) -> i32 =
        std::mem::transmute(jni_slot(env, SLOT_REGISTER_NATIVES));
    let rc = register(env, class, table.as_ptr(), table.len() as i32);
    eprintln!("[blast] hosted Surface.nativeGetFromBlastBufferQueue (rc={rc})");
    rc
}

#[cfg(test)]
mod tests {
    use super::*;
    use pengudroid_proto::Message;
    use pengudroid_socket::Connection;
    use std::os::unix::net::UnixStream;

    fn pair() -> (Connection, Connection) {
        let (a, b) = UnixStream::pair().unwrap();
        let (a, b) = (Connection::from_stream(a), Connection::from_stream(b));
        a.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
        b.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
        (a, b)
    }

    #[test]
    fn layout_change_retires_old_layer_before_creating_replacement() {
        let (client, server) = pair();
        let peer = std::thread::spawn(move || {
            for (expected, response) in [
                (Message::CreateSubsurface { parent_pid: std::process::id() as i32,
                    x: 3, y: 4, width: 720, height: 405 }, Message::SubsurfaceCreated { surface_id: 10 }),
                (Message::DestroySurface { surface_id: 10 }, Message::Ack),
                (Message::CreateSubsurface { parent_pid: std::process::id() as i32,
                    x: 3, y: 4, width: 1280, height: 720 }, Message::SubsurfaceCreated { surface_id: 11 }),
                (Message::DestroySurface { surface_id: 11 }, Message::Ack),
            ] {
                assert_eq!(server.recv_message().unwrap().0, expected);
                server.send_message(&response, &[]).unwrap();
            }
        });
        let mut layer = VideoLayer::default();
        layer.ensure(&client, (720, 405), 3, 4).unwrap();
        // Ordinary video frames must reuse their layer, with no IPC here.
        layer.ensure(&client, (720, 405), 3, 4).unwrap();
        layer.ensure(&client, (1280, 720), 3, 4).unwrap();
        assert_eq!(layer.id, 11);
        layer.retire(&client).unwrap();
        layer.retire(&client).unwrap();
        peer.join().unwrap();
        assert_eq!(layer.id, 0);
    }

    #[test]
    fn refused_replacement_does_not_reuse_retired_surface() {
        let (client, server) = pair();
        let peer = std::thread::spawn(move || {
            assert_eq!(server.recv_message().unwrap().0, Message::DestroySurface { surface_id: 9 });
            server.send_message(&Message::Ack, &[]).unwrap();
            for response in [
                Message::Error { code: -2, message: "no window yet".into() },
                Message::SubsurfaceCreated { surface_id: 12 },
            ] {
                assert!(matches!(server.recv_message().unwrap().0,
                    Message::CreateSubsurface { width: 1280, height: 720, .. }));
                server.send_message(&response, &[]).unwrap();
            }
        });
        let mut layer = VideoLayer { id: 9, layout: (720, 405) };
        assert!(layer.ensure(&client, (1280, 720), 0, 0).is_err());
        assert_eq!(layer.id, 0);
        layer.ensure(&client, (1280, 720), 0, 0).unwrap();
        assert_eq!(layer.id, 12);
        peer.join().unwrap();
    }

    #[test]
    fn native_destroy_stops_registered_queue_and_rejects_surface_handout() {
        let blast = Box::leak(Box::new(Blast {
            producer: [0; 2], consumer: [0; 2],
            item_consumer: std::ptr::null_mut(), surface: std::ptr::null_mut(),
            bbq: 0x1234usize as *mut c_void,
            java_state: Mutex::new(JavaState { java_surface: 0, width: 1, height: 1, format: 0 }),
            stopped: std::sync::atomic::AtomicBool::new(false),
            video_surface: std::sync::atomic::AtomicU32::new(0),
            display_size: std::sync::atomic::AtomicU64::new(0),
        }));
        let ptr = blast.bbq as i64;
        BLASTS.lock().unwrap().push((ptr as usize, blast as *const Blast as usize));
        unsafe {
            assert!(std::ptr::eq(blast_at(ptr).unwrap(), blast));
            native_destroy(std::ptr::null_mut(), std::ptr::null_mut(), ptr);
            native_destroy(std::ptr::null_mut(), std::ptr::null_mut(), ptr);
            assert!(native_get_surface(std::ptr::null_mut(), std::ptr::null_mut(), ptr, 0).is_null());
            assert_eq!(surface_from_blast(std::ptr::null_mut(), std::ptr::null_mut(), 0, ptr), 0);
        }
        assert!(blast.stopped.load(std::sync::atomic::Ordering::Acquire));
        BLASTS.lock().unwrap().retain(|(native, _)| *native != ptr as usize);
        // Match production storage lifetime: readers may still hold a reference.
    }
}
