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
//! What replaces the native object is deliberately small:
//!
//! * `android::BLASTBufferQueue::createBufferQueue` (exported by `libgui`) makes
//!   a real `BufferQueue` — the same call the real class makes;
//! * `android::Surface` wraps the producer, and the Java `Surface(long)`
//!   constructor wraps *that*, so the app gets a genuine `Surface` whose
//!   `queueBuffer` lands in a queue we hold the consumer end of;
//! * `update()` only records the geometry: `Surface::setBuffersFormat` and
//!   `Surface::setBuffersUserDimensions` are what `dequeueBuffer` passes down to
//!   the consumer, so no SurfaceControl transaction is needed.
//!
//! The consumer end is kept in [`Blast::consumer`] for the compositor. Until the
//! compositor pulls from it, frames are produced and dropped — which is still a
//! large step up from "the app has no surface at all".

use std::ffi::CStr;
use std::os::raw::{c_char, c_void};
use std::sync::Mutex;

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

unsafe fn jni_new_global_ref(
    env: *mut c_void,
    obj: *mut c_void,
) -> *mut c_void {
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
// libgui entry points
// ---------------------------------------------------------------------------

type CreateBufferQueue = unsafe extern "C" fn(*mut [u64; 2], *mut [u64; 2]);
type SurfaceCtor =
    unsafe extern "C" fn(*mut c_void, *const [u64; 2], bool, *const [u64; 2]);
type SurfaceIntSetter = unsafe extern "C" fn(*mut c_void, i32) -> i32;
type SurfaceDimsSetter = unsafe extern "C" fn(*mut c_void, u32, u32) -> i32;

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
    surface_ctor: SurfaceCtor,
    set_buffers_format: SurfaceIntSetter,
    set_user_dimensions: SurfaceDimsSetter,
}

static GUI: std::sync::OnceLock<Option<Gui>> = std::sync::OnceLock::new();

fn gui() -> Option<&'static Gui> {
    GUI.get_or_init(|| unsafe {
        let create_buffer_queue = guest_sym(
            b"libgui.so\0",
            b"_ZN7android16BLASTBufferQueue17createBufferQueueEPNS_2spINS_22IGraphicBufferProducerEEEPNS1_INS_22IGraphicBufferConsumerEEE\0",
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
        if create_buffer_queue.is_null()
            || surface_ctor.is_null()
            || set_buffers_format.is_null()
            || set_user_dimensions.is_null()
        {
            eprintln!("[blast] libgui is missing a required entry point");
            return None;
        }
        Some(Gui {
            create_buffer_queue: std::mem::transmute(create_buffer_queue),
            surface_ctor: std::mem::transmute(surface_ctor),
            set_buffers_format: std::mem::transmute(set_buffers_format),
            set_user_dimensions: std::mem::transmute(set_user_dimensions),
        })
    })
    .as_ref()
}

// ---------------------------------------------------------------------------
// The hosted object
// ---------------------------------------------------------------------------

/// One hosted `BLASTBufferQueue`.
///
/// `producer`/`consumer` are `sp<>` pairs (object pointer + control block), the
/// same layout `libgui` uses, so they can be handed straight back to it.
struct Blast {
    producer: [u64; 2],
    consumer: [u64; 2],
    /// `android::Surface*` — the native half of the Java `Surface` we hand out.
    surface: *mut c_void,
    /// Global reference to that Java `Surface`, created on first request.
    java_surface: *mut c_void,
    width: u32,
    height: u32,
    format: i32,
}

// SAFETY: the pointers are only ever used from the thread that owns the object,
// and the registry mutex serialises creation and lookup.
unsafe impl Send for Blast {}

/// Hosted objects, kept alive for the life of the process.
///
/// The address of the `Box` is the `long mNativeObject` the Java side holds, so
/// it must not move; `Box` gives us that.
static BLASTS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

unsafe fn blast_at(ptr: i64) -> Option<&'static mut Blast> {
    if ptr == 0 {
        return None;
    }
    Some(&mut *(ptr as *mut Blast))
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
    let (width, height, format) = (1i32, 1i32, 0i32);
    let Some(gui) = gui() else {
        return 0;
    };
    let mut producer = [0u64; 2];
    let mut consumer = [0u64; 2];
    (gui.create_buffer_queue)(&mut producer, &mut consumer);
    if producer[0] == 0 || consumer[0] == 0 {
        eprintln!("[blast] createBufferQueue returned nothing");
        return 0;
    }
    // `Surface(const sp<IGraphicBufferProducer>&, bool controlledByApp, const
    // sp<IBinder>&)`. 512 bytes is comfortably larger than the class.
    let surface = libc::calloc(1, 512);
    if surface.is_null() {
        return 0;
    }
    let no_binder = [0u64; 2];
    (gui.surface_ctor)(surface, &producer, true, &no_binder);
    (gui.set_buffers_format)(surface, format);
    (gui.set_user_dimensions)(surface, width.max(1) as u32, height.max(1) as u32);

    let blast = Box::new(Blast {
        producer,
        consumer,
        surface,
        java_surface: std::ptr::null_mut(),
        width: width.max(1) as u32,
        height: height.max(1) as u32,
        format,
    });
    let ptr = Box::into_raw(blast);
    BLASTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(ptr as usize);
    eprintln!(
        "[blast] created {width}x{height} fmt={format:#x} producer={:#x} consumer={:#x}",
        producer[0], consumer[0]
    );
    ptr as i64
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
    if !blast.java_surface.is_null() {
        return blast.java_surface;
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
    let args = [JValue {
        j: blast.surface as i64,
    }];
    let local = jni_new_object(env, surface_class, ctor, &args);
    if local.is_null() {
        eprintln!("[blast] new Surface(long) threw");
        return std::ptr::null_mut();
    }
    blast.java_surface = jni_new_global_ref(env, local);
    eprintln!("[blast] handed out a Surface for {:#x}", blast.surface as usize);
    blast.java_surface
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
    _surface_control: i64,
    width: i64,
    height: i64,
    format: i32,
) {
    let Some(gui) = gui() else {
        return;
    };
    let Some(blast) = blast_at(ptr) else {
        return;
    };
    let (w, h) = (width.max(1) as u32, height.max(1) as u32);
    if format != blast.format {
        (gui.set_buffers_format)(blast.surface, format);
        blast.format = format;
    }
    if w != blast.width || h != blast.height {
        (gui.set_user_dimensions)(blast.surface, w, h);
        blast.width = w;
        blast.height = h;
    }
    eprintln!("[blast] update {w}x{h} fmt={format:#x}");
}

/// `BLASTBufferQueue.nativeDestroy(long ptr)`.
///
/// The object is process-lifetime: the Java side may still hold the `Surface`
/// we handed out, and freeing the queue under it would fault.
unsafe extern "C" fn native_destroy(_env: *mut c_void, _class: *mut c_void, _ptr: i64) {}

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

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------
//
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
