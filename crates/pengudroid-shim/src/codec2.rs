//! Host the real codec2 HIDL service in-process.
//!
//! Faking `IComponentStore`/`IComponent` replies by hand means re-implementing
//! the marshalling of `ComponentTraits`, `WorkBundle`, `Buffer`, `hidl_handle`
//! and friends — hundreds of lines that have to match the generated code byte
//! for byte. Instead we build the *real* service objects and let the generated
//! code do the marshalling:
//!
//! ```text
//! C2PlatformComponentStore   (libcodec2_vndk.so, dlopens libcodec2_soft_*.so)
//!   -> utils::ComponentStore (libcodec2_hidl@1.0.so, the IComponentStore impl)
//!     -> BnHwComponentStore  (android.hardware.media.c2@1.0.so, the binder stub)
//! ```
//!
//! A transaction on the `codec2` handle is replayed into a HIDL `Parcel` and
//! handed to `BnHwComponentStore::onTransact`; the reply parcel it fills in is
//! copied back out. The soft codec plugins then decode on the CPU, in this
//! process.
//!
//! `createComponent` answers with a `flat_binder_object` of type
//! `BINDER_TYPE_BINDER` whose cookie is the local `BnHwComponent`, so the
//! client's `unflatten_binder` hands it the object itself and every later
//! `IComponent` call is a direct virtual call — no binder round trip at all.
//! The same holds for the `IComponentListener` the client passes in: it is a
//! local object, so the decoder's `onWorkDone` calls it directly.

use std::ffi::{c_char, c_void};
use std::sync::OnceLock;

use crate::binder::push_buffer_object;

/// `android::hardware::Parcel` field offsets, measured from libhidlbase.
///
/// `Parcel::setData` stores the buffer at `0x28` and the size at `0x30`;
/// `readObject<T>` reads `mDataPos` at `0x40`, the offsets array at `0x48` and
/// its length at `0x50`; `ipcObjectsCount` reads `0x50`. The constructor zeroes
/// everything up to `0x78`.
const PARCEL_DATA: usize = 0x28;
const PARCEL_DATA_SIZE: usize = 0x30;
const PARCEL_DATA_POS: usize = 0x40;
const PARCEL_OBJECTS: usize = 0x48;
const PARCEL_OBJECTS_SIZE: usize = 0x50;
const PARCEL_OBJECTS_CAPACITY: usize = 0x58;
const PARCEL_NEXT_OBJECT_HINT: usize = 0x60;
/// Generous upper bound: the constructor writes up to `0x78`.
const PARCEL_BYTES: usize = 0x100;

/// `IBinder::onTransact` slot in the primary vtable.
///
/// `BHwBinder::transact` dispatches through `call *0x60(%rax)`, and
/// `BnHwComponentStore` overrides that slot.
const VTBL_ON_TRANSACT: usize = 0x60;

// --- a hand-built `std::function<void(Parcel&)>` ---------------------------
//
// The generated code hands the reply parcel to a `std::function` callback and
// only *there* are the embedded buffers still alive (the `hidl_vec` it points
// at is a local of the implementation call). Rust cannot construct a libc++
// `std::function`, so we build the two objects it is made of by hand.
//
// Measured from libhidlbase:
//   * `std::function` is 40 bytes: `{__f_@0, allocator@8, __buf_@0x10}`.
//   * `__f_` points at a `__func` object of 48 bytes: `{vtable@0, captures@8..}`.
//   * the vtable's address point holds `__clone()` at +0x10
//     (`BnHwComponentStore::onTransact` does `call *0x10(%rax)`) and
//     `operator()` at +0x30 (`_hidl_listComponents` does `call *0x30(%rax)`).

/// `__func` object: vtable pointer followed by the captures.
#[repr(C)]
struct FuncObj {
    vtable: *const usize,
    captures: [usize; 5],
}

/// `std::function<void(Parcel&)>`.
///
/// The `__f_` pointer sits at `0x20` for the one-argument signature
/// (`onTransact` reads `0x20(%r9)`) and at `0x28` for the two-argument one
/// (`utils::ComponentStore::listComponents` reads `0x28(%rsi)`), so both slots
/// carry it.
#[repr(C)]
struct StdFunction {
    buf: [usize; 4],
    f20: *const FuncObj,
    f28: *const FuncObj,
}

thread_local! {
    /// The reply parcel captured from inside the callback.
    static CAPTURED: std::cell::RefCell<Option<(Vec<u8>, Vec<u64>)>> =
        const { std::cell::RefCell::new(None) };
}

unsafe extern "C" fn func_noop() {}

/// `__clone()`: hand back a fresh object with the same vtable and captures.
unsafe extern "C" fn func_clone(this: *const FuncObj) -> *mut FuncObj {
    let copy = libc::malloc(std::mem::size_of::<FuncObj>()) as *mut FuncObj;
    if !copy.is_null() {
        std::ptr::copy_nonoverlapping(this, copy, 1);
    }
    copy
}

/// `__clone(void*)`: copy into the caller's buffer.
///
/// Called as `slot0(&buf, func)` — the destination comes first.
unsafe extern "C" fn func_clone_into(out: *mut FuncObj, this: *const FuncObj) -> *mut FuncObj {
    std::ptr::copy_nonoverlapping(this, out, 1);
    out
}

/// `operator()(Parcel&)`: copy the reply out while its buffers are still valid.
unsafe extern "C" fn func_operator(_this: *mut FuncObj, parcel: *const c_void) {
    let base = parcel as *const u8;
    let data = *(base.add(PARCEL_DATA) as *const *const u8);
    let len = *(base.add(PARCEL_DATA_SIZE) as *const usize);
    let offs = *(base.add(PARCEL_OBJECTS) as *const *const u64);
    let count = *(base.add(PARCEL_OBJECTS_SIZE) as *const usize);
    let mut bytes = if data.is_null() || len == 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(data, len).to_vec()
    };
    let objects = if offs.is_null() || count == 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(offs, count).to_vec()
    };
    stabilize(&mut bytes, &objects, inc_strong());
    if trace_enabled() {
        diag(&format!(
            "[codec2]   callback parcel={parcel:p} data={data:p} len={len} objects={count}\n"
        ));
        diag(&format!(
            "[codec2]   callback captured {} bytes, {} objects\n",
            bytes.len(),
            objects.len()
        ));
    }
    CAPTURED.with(|c| *c.borrow_mut() = Some((bytes, objects)));
}

/// Copy a `hidl_string` at `base + off` into process-lifetime memory.
///
/// The source lives in the service's frame, so it has to be copied while the
/// implementation call is still on the stack.
unsafe fn copy_hidl_string(base: *const u8, off: usize) -> (u64, u64) {
    let ptr = *(base.add(off) as *const *const u8);
    let len = *(base.add(off + 8) as *const u32) as usize;
    if ptr.is_null() || len == 0 {
        return crate::binder::leak_cstr("");
    }
    let bytes = std::slice::from_raw_parts(ptr, len);
    let text = String::from_utf8_lossy(bytes).into_owned();
    crate::binder::leak_cstr(&text)
}

/// Build the `listComponents` reply from the traits vector the implementation
/// handed us.
///
/// Layout (measured): `[hidl status][codec2 status][vec struct][traits array]
/// [name][mediaType][aliases array][alias strings]...`, with `ComponentTraits`
/// = `name`@0x00, `domain/kind/rank`@0x10, `mediaType`@0x20, `aliases`@0x30
/// (element size 16).
unsafe fn build_traits_reply(status: i32, vec: *const u8) -> (Vec<u8>, Vec<u64>) {
    let data = *(vec as *const *const u8);
    let count = *(vec.add(8) as *const u32) as usize;
    let mut parcel = Vec::new();
    let mut objects = Vec::new();
    parcel.extend_from_slice(&0i32.to_ne_bytes()); // hidl status
    parcel.extend_from_slice(&status.to_ne_bytes()); // codec2 status

    let traits: &'static [u8] = Box::leak(vec![0u8; count * 64].into_boxed_slice());
    let traits_ptr = traits.as_ptr() as *mut u8;

    let mut vec_struct = Vec::with_capacity(16);
    vec_struct.extend_from_slice(&(traits_ptr as u64).to_ne_bytes());
    vec_struct.extend_from_slice(&(count as u32).to_ne_bytes());
    vec_struct.extend_from_slice(&0u32.to_ne_bytes());
    let vec_struct: &'static [u8] = Box::leak(vec_struct.into_boxed_slice());

    let vec_idx =
        push_buffer_object(&mut parcel, &mut objects, 0, vec_struct.as_ptr() as u64, 16, 0, 0);
    let arr_idx = push_buffer_object(
        &mut parcel,
        &mut objects,
        1,
        traits_ptr as u64,
        (count * 64) as u64,
        vec_idx,
        0,
    );

    for i in 0..count {
        let src = data.add(i * 64);
        let dst = traits_ptr.add(i * 64);
        let (name_ptr, name_len) = copy_hidl_string(src, 0);
        let (mt_ptr, mt_len) = copy_hidl_string(src, 0x20);
        // domain/kind/rank are plain ints, copied verbatim.
        std::ptr::copy_nonoverlapping(src.add(0x10), dst.add(0x10), 16);
        *(dst as *mut u64) = name_ptr;
        *(dst.add(8) as *mut u32) = (name_len - 1) as u32;
        *(dst.add(0x20) as *mut u64) = mt_ptr;
        *(dst.add(0x28) as *mut u32) = (mt_len - 1) as u32;

        let alias_src = *(src.add(0x30) as *const *const u8);
        let alias_count = *(src.add(0x38) as *const u32) as usize;
        let alias_arr: &'static [u8] = Box::leak(vec![0u8; alias_count * 16].into_boxed_slice());
        let alias_ptr = alias_arr.as_ptr() as *mut u8;
        *(dst.add(0x30) as *mut u64) = alias_ptr as u64;
        *(dst.add(0x38) as *mut u32) = alias_count as u32;
        *(dst.add(0x3c) as *mut u32) = 0;

        push_buffer_object(
            &mut parcel,
            &mut objects,
            1,
            name_ptr,
            name_len,
            arr_idx,
            (i * 64) as u64,
        );
        push_buffer_object(
            &mut parcel,
            &mut objects,
            1,
            mt_ptr,
            mt_len,
            arr_idx,
            (i * 64 + 0x20) as u64,
        );
        let alias_idx = push_buffer_object(
            &mut parcel,
            &mut objects,
            1,
            alias_ptr as u64,
            (alias_count * 16) as u64,
            arr_idx,
            (i * 64 + 0x30) as u64,
        );
        for j in 0..alias_count {
            let (ptr, len) = copy_hidl_string(alias_src, j * 16);
            *(alias_ptr.add(j * 16) as *mut u64) = ptr;
            *(alias_ptr.add(j * 16 + 8) as *mut u32) = (len - 1) as u32;
            push_buffer_object(&mut parcel, &mut objects, 1, ptr, len, alias_idx, (j * 16) as u64);
        }
    }
    if trace_enabled() {
        diag(&format!("[codec2]   built traits reply for {count} codecs\n"));
    }
    (parcel, objects)
}

/// `operator()(Status, const hidl_vec<ComponentTraits>&)` for the direct call.
unsafe extern "C" fn traits_operator(_this: *mut FuncObj, status: *const i32, vec: *const u8) {
    let reply = build_traits_reply(*status, vec);
    CAPTURED.with(|c| *c.borrow_mut() = Some(reply));
}

/// `RefBase::incStrong(void const*)`, resolved once.
fn inc_strong() -> *mut c_void {
    static PTR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    use std::sync::atomic::Ordering;
    let existing = PTR.load(Ordering::Relaxed);
    if existing != 0 {
        return existing as *mut c_void;
    }
    let p = unsafe {
        sym(b"libhidlbase.so\0", b"_ZNK7android7RefBase9incStrongEPKv\0").unwrap_or(std::ptr::null_mut())
    };
    PTR.store(p as usize, Ordering::Relaxed);
    p
}

/// The vtable and the single `__func` object, built on first use.
///
/// One transaction is in flight at a time, so a single object suffices; the
/// captures are unused because the reply is stashed in `CAPTURED` instead.
static FUNC_OBJ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

unsafe fn func_obj() -> *const FuncObj {
    make_func(func_operator as usize)
}

/// Build a `__func` object whose `operator()` is `operator`.
unsafe fn make_func(operator: usize) -> *const FuncObj {
    let vtable = libc::malloc(7 * 8) as *mut usize;
    if vtable.is_null() {
        return std::ptr::null();
    }
    *vtable.add(0) = func_clone_into as usize; // __clone(void*)
    *vtable.add(1) = func_noop as usize;
    *vtable.add(2) = func_clone as usize; // __clone()
    *vtable.add(3) = func_noop as usize;
    *vtable.add(4) = func_noop as usize;
    *vtable.add(5) = func_noop as usize;
    *vtable.add(6) = operator; // operator()
    let obj = libc::malloc(std::mem::size_of::<FuncObj>()) as *mut FuncObj;
    if obj.is_null() {
        return std::ptr::null();
    }
    (*obj).vtable = vtable;
    (*obj).captures = [0; 5];
    obj
}

type ParcelCtor = unsafe extern "C" fn(*mut c_void);
type ParcelDtor = unsafe extern "C" fn(*mut c_void);
type SetData = unsafe extern "C" fn(*mut c_void, *const u8, usize) -> i32;
type SetDataPosition = unsafe extern "C" fn(*mut c_void, usize) -> i32;
type OnTransact = unsafe extern "C" fn(*mut c_void, u32, *const c_void, *mut c_void, u32) -> i32;
/// `BHwBinder::transact(code, data, reply, flags, std::function<void(Parcel&)>)`.
///
/// The `std::function` is 48 bytes and passed by invisible reference; a zeroed
/// buffer is a valid *empty* function, which is what a synchronous call wants
/// (the reply parcel is already filled in by the time `onTransact` returns).
type Transact = unsafe extern "C" fn(*mut c_void, u32, *const c_void, *mut c_void, u32, *const u8) -> i32;
type GetPlatformStore = unsafe extern "C" fn(*mut [u64; 2]);
type StoreCtor = unsafe extern "C" fn(*mut c_void, *const [u64; 2]);
type BnCtor = unsafe extern "C" fn(*mut c_void, *const *mut c_void);

struct Api {
    parcel_ctor: ParcelCtor,
    parcel_dtor: ParcelDtor,
    set_data: SetData,
    set_data_position: SetDataPosition,
    on_transact: Transact,
    store: *mut c_void,
    /// The `utils::ComponentStore` implementation behind the binder stub.
    store_impl: *mut c_void,
    /// `utils::ComponentStore::listComponents(std::function<...>)`.
    list_components: *mut c_void,
}

// SAFETY: the service objects are process-wide singletons; the codec2 HIDL
// service serialises its own state internally.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

static API: OnceLock<Option<Api>> = OnceLock::new();

fn trace_enabled() -> bool {
    std::env::var_os("PENGUDROID_TRACE_CODEC2").is_some()
}

unsafe fn diag(line: &str) {
    libc::write(2, line.as_ptr() as *const c_void, line.len());
}

/// Resolve `name` from the already-loaded `soname`.
unsafe fn sym(soname: &[u8], name: &[u8]) -> Option<*mut c_void> {
    let handle = libc::dlopen(soname.as_ptr() as *const c_char, libc::RTLD_NOW | libc::RTLD_GLOBAL);
    if handle.is_null() {
        if trace_enabled() {
            diag(&format!(
                "[codec2] dlopen {} failed\n",
                String::from_utf8_lossy(&soname[..soname.len() - 1])
            ));
        }
        return None;
    }
    let p = libc::dlsym(handle, name.as_ptr() as *const c_char);
    if p.is_null() {
        if trace_enabled() {
            diag(&format!(
                "[codec2] dlsym {} failed\n",
                String::from_utf8_lossy(&name[..name.len() - 1])
            ));
        }
        None
    } else {
        Some(p)
    }
}

/// Build the service objects on first use.
unsafe fn build() -> Option<Api> {
    let parcel_ctor = sym(b"libhidlbase.so\0", b"_ZN7android8hardware6ParcelC1Ev\0")?;
    let parcel_dtor = sym(b"libhidlbase.so\0", b"_ZN7android8hardware6ParcelD1Ev\0")?;
    let set_data = sym(b"libhidlbase.so\0", b"_ZN7android8hardware6Parcel7setDataEPKhm\0")?;
    let set_data_position =
        sym(b"libhidlbase.so\0", b"_ZNK7android8hardware6Parcel15setDataPositionEm\0")?;
    let get_platform_store =
        sym(b"libcodec2_vndk.so\0", b"_ZN7android31GetCodec2PlatformComponentStoreEv\0")?;
    let store_ctor = sym(
        b"libcodec2_hidl@1.0.so\0",
        b"_ZN7android8hardware5media2c24V1_05utils14ComponentStoreC1ERKNSt3__110shared_ptrI16C2ComponentStoreEE\0",
    )?;
    let bn_ctor = sym(
        b"android.hardware.media.c2@1.0.so\0",
        b"_ZN7android8hardware5media2c24V1_018BnHwComponentStoreC1ERKNS_2spINS3_15IComponentStoreEEE\0",
    )?;

    let transact = sym(
        b"libhidlbase.so\0",
        b"_ZN7android8hardware9BHwBinder8transactEjRKNS0_6ParcelEPS2_jNSt3__18functionIFvRS2_EEE\0",
    )?;
    let list_components = sym(
        b"libcodec2_hidl@1.0.so\0",
        b"_ZN7android8hardware5media2c24V1_05utils14ComponentStore14listComponentsENSt3__18functionIFvNS3_6StatusERKNS0_8hidl_vecINS3_15IComponentStore15ComponentTraitsEEEEEE\0",
    )?;

    // `GetCodec2PlatformComponentStore()` returns a `shared_ptr<C2ComponentStore>`
    // by value (sret), so the control block is a real one and the refcounts work.
    let get_platform_store: GetPlatformStore = std::mem::transmute(get_platform_store);
    let mut platform: [u64; 2] = [0, 0];
    get_platform_store(&mut platform);
    if platform[0] == 0 {
        if trace_enabled() {
            diag("[codec2] GetCodec2PlatformComponentStore returned null\n");
        }
        return None;
    }

    // `utils::ComponentStore` is a `RefBase` with a vtable at +0 and another at
    // +0xf8; 4 KiB is comfortably larger than the class.
    let store_ctor: StoreCtor = std::mem::transmute(store_ctor);
    let store = libc::calloc(1, 4096) as *mut c_void;
    if store.is_null() {
        return None;
    }
    store_ctor(store, &platform);

    // `BnHwComponentStore(const sp<IComponentStore>&)`; an `sp<T>` is just the
    // object pointer, and the constructor takes it by reference.
    let bn_ctor: BnCtor = std::mem::transmute(bn_ctor);
    let bn = libc::calloc(1, 4096) as *mut c_void;
    if bn.is_null() {
        return None;
    }
    let mut store_impl = store;
    bn_ctor(bn, &mut store_impl);

    let vtable = *(bn as *const *const usize);
    if vtable.is_null() {
        return None;
    }
    if trace_enabled() {
        diag(&format!(
            "[codec2] service up: platform={:#x} store={:#x} bn={:#x}\n",
            platform[0], store as usize, bn as usize
        ));
    }
    Some(Api {
        parcel_ctor: std::mem::transmute(parcel_ctor),
        parcel_dtor: std::mem::transmute(parcel_dtor),
        set_data: std::mem::transmute(set_data),
        set_data_position: std::mem::transmute(set_data_position),
        on_transact: std::mem::transmute(transact),
        store: bn,
        store_impl: store,
        list_components,
    })
}

fn api() -> Option<&'static Api> {
    // On by default: the real service answers `getConfigurable`,
    // `getPoolClientManager`, `listComponents` (53 codecs, `video/avc`
    // included) and `createInterface`. `PENGUDROID_NO_CODEC2_SERVICE=1` falls
    // back to the hand-written stub replies.
    if std::env::var_os("PENGUDROID_NO_CODEC2_SERVICE").is_some() {
        return None;
    }
    API.get_or_init(|| unsafe { build() }).as_ref()
}

/// Whether the in-process codec2 service is available.
pub(crate) fn available() -> bool {
    api().is_some()
}

/// `binder_buffer_object` type tag, as written by `Parcel::writeEmbeddedBuffer`.
const BINDER_TYPE_BUFFER: u32 = 0x7074_2a85;
/// `flat_binder_object` tag for a *local* binder (`BINDER_TYPE_BINDER`).
const BINDER_TYPE_BINDER: u32 = 0x7362_2a85;

/// Take a strong reference on a local binder the service put in a reply.
///
/// The real driver calls `acquire_binder_object` while the transaction is in
/// flight, so the object outlives the service's own `sp`. Our broker copies the
/// reply after `onTransact` returns, and the service's `sp` is released when we
/// destroy the reply parcel — the client's `unflatten_binder` then calls
/// `incStrong` on a dead object and faults (measured: `unflatten_binder+0x73`).
unsafe fn acquire_binder(cookie: u64, inc_strong: *mut c_void) {
    if cookie == 0 || inc_strong.is_null() {
        return;
    }
    let vtable = *(cookie as *const *const usize);
    if vtable.is_null() {
        return;
    }
    // `-0x18(%rax)` is the vtable's offset-to-top, which adjusts the pointer to
    // the `RefBase` subobject.
    let offset_to_top = *(vtable.sub(3)) as isize;
    let this = (cookie as isize + offset_to_top) as *mut c_void;
    let inc: unsafe extern "C" fn(*mut c_void, *const c_void) = std::mem::transmute(inc_strong);
    inc(this, cookie as *const c_void);
}

/// Copy every embedded buffer in a reply to process-lifetime memory.
///
/// A real binder driver copies each `binder_buffer_object`'s payload into the
/// receiver's address space. Our userspace broker only copies the parcel data
/// and the offsets array, so a buffer that the service built on its stack (the
/// `hidl_vec` struct, for instance) would be read after the frame is gone —
/// measured as `Buffer length 32 does not match expected size 15638207488`.
///
/// Objects are written parent first, so one pass suffices: copy the payload,
/// point the parent's copy at it, then rewrite the object's own `buffer` field.
unsafe fn stabilize(bytes: &mut [u8], objects: &[u64], inc_strong: *mut c_void) {
    for (i, &off) in objects.iter().enumerate() {
        let o = off as usize;
        if o + 4 > bytes.len() {
            continue;
        }
        let ty = u32::from_ne_bytes(bytes[o..o + 4].try_into().unwrap());
        if ty == BINDER_TYPE_BINDER {
            // `{type@0, flags@4, binder@8, cookie@0x10}` — 24 bytes, no
            // stability word.
            if o + 24 <= bytes.len() {
                let cookie = u64::from_ne_bytes(bytes[o + 16..o + 24].try_into().unwrap());
                acquire_binder(cookie, inc_strong);
            }
            continue;
        }
        if ty != BINDER_TYPE_BUFFER || o + 40 > bytes.len() {
            continue;
        }
        let buf = u64::from_ne_bytes(bytes[o + 8..o + 16].try_into().unwrap());
        let len = u64::from_ne_bytes(bytes[o + 16..o + 24].try_into().unwrap());
        let parent = u64::from_ne_bytes(bytes[o + 24..o + 32].try_into().unwrap());
        let parent_offset = u64::from_ne_bytes(bytes[o + 32..o + 40].try_into().unwrap());
        if buf == 0 || len == 0 {
            continue;
        }
        let copy = libc::malloc(len as usize) as *mut u8;
        if copy.is_null() {
            continue;
        }
        std::ptr::copy_nonoverlapping(buf as *const u8, copy, len as usize);
        if trace_enabled() {
            let head = std::slice::from_raw_parts(copy, (len as usize).min(32));
            diag(&format!(
                "[codec2]   obj{i} buf={buf:#x} len={len} parent={parent} parentOffset={parent_offset} head={:02x?}\n",
                head
            ));
        }
        // Point the parent's copy at the child, but only when the parent really
        // holds this child's pointer: a top-level object carries `parent = 0`
        // too, and patching it would overwrite its own first field.
        if (parent as usize) != i {
            let po = objects[parent as usize] as usize;
            if po + 40 <= bytes.len() {
                let pbuf = u64::from_ne_bytes(bytes[po + 8..po + 16].try_into().unwrap());
                let plen = u64::from_ne_bytes(bytes[po + 16..po + 24].try_into().unwrap());
                if pbuf != 0 && parent_offset + 8 <= plen {
                    let slot = (pbuf + parent_offset) as *mut u64;
                    if *slot == buf {
                        *slot = copy as u64;
                    }
                }
            }
        }
        bytes[o + 8..o + 16].copy_from_slice(&(copy as u64).to_ne_bytes());
    }
}

/// Replay one transaction into the real service.
///
/// Returns the reply parcel's data and object offsets, or `None` when the
/// service could not be built (the caller then falls back to the stub replies).
///
/// # Safety
/// `data` must be the request parcel's bytes; the object offsets must be
/// 4-byte aligned positions inside `data`.
pub(crate) unsafe fn dispatch(code: u32, data: &[u8], objects: &[u64]) -> Option<(Vec<u8>, Vec<u64>)> {
    let api = api()?;

    // `listComponents` writes its reply synchronously, so the `hidl_vec` it
    // points at is a dead local by the time `onTransact` returns. Call the
    // implementation directly instead: the traits vector is alive inside our
    // own callback, and we marshal the reply ourselves.
    if code == 3 {
        CAPTURED.with(|c| *c.borrow_mut() = None);
        let f = make_func(traits_operator as usize);
        if trace_enabled() {
            diag(&format!("[codec2]   direct listComponents func={f:p}\n"));
        }
        let func = StdFunction {
            buf: [0; 4],
            f20: f,
            f28: f,
        };
        // `Return<void> listComponents(std::function<...> cb)` returns a 16-byte
        // struct, so the first argument is the sret slot: (sret, this, &cb).
        let mut ret = [0u8; 16];
        let list: unsafe extern "C" fn(*mut u8, *mut c_void, *const StdFunction) =
            std::mem::transmute(api.list_components);
        list(ret.as_mut_ptr(), api.store_impl, &func);
        if let Some(reply) = CAPTURED.with(|c| c.borrow_mut().take()) {
            if trace_enabled() {
                diag(&format!(
                    "[codec2] code 3 -> direct call, {} bytes, {} objects\n",
                    reply.0.len(),
                    reply.1.len()
                ));
            }
            return Some(reply);
        }
    }

    // The request parcel: `setData` allocates and copies the bytes, then we
    // install the offsets array. `Parcel::~Parcel` frees both with `free`, so
    // the array has to come from `malloc`.
    let mut request = [0u8; PARCEL_BYTES];
    (api.parcel_ctor)(request.as_mut_ptr() as *mut c_void);
    if (api.set_data)(request.as_mut_ptr() as *mut c_void, data.as_ptr(), data.len()) != 0 {
        (api.parcel_dtor)(request.as_mut_ptr() as *mut c_void);
        return None;
    }
    let offsets = if objects.is_empty() {
        std::ptr::null_mut()
    } else {
        let p = libc::malloc(objects.len() * 8) as *mut u64;
        if p.is_null() {
            (api.parcel_dtor)(request.as_mut_ptr() as *mut c_void);
            return None;
        }
        std::ptr::copy_nonoverlapping(objects.as_ptr(), p, objects.len());
        p
    };
    let base = request.as_mut_ptr() as *mut u8;
    *(base.add(PARCEL_OBJECTS) as *mut *mut u64) = offsets;
    *(base.add(PARCEL_OBJECTS_SIZE) as *mut usize) = objects.len();
    *(base.add(PARCEL_OBJECTS_CAPACITY) as *mut usize) = objects.len();
    *(base.add(PARCEL_NEXT_OBJECT_HINT) as *mut usize) = 0;
    (api.set_data_position)(request.as_mut_ptr() as *mut c_void, 0);

    let mut reply = [0u8; PARCEL_BYTES];
    (api.parcel_ctor)(reply.as_mut_ptr() as *mut c_void);
    CAPTURED.with(|c| *c.borrow_mut() = None);
    let callback = StdFunction {
        buf: [0; 4],
        f20: func_obj(),
        f28: func_obj(),
    };
    let rc = (api.on_transact)(
        api.store,
        code,
        request.as_ptr() as *const c_void,
        reply.as_mut_ptr() as *mut c_void,
        0,
        &callback as *const StdFunction as *const u8,
    );
    let captured = CAPTURED.with(|c| c.borrow_mut().take());
    if trace_enabled() {
        diag(&format!("[codec2]   reply parcel={:p}\n", reply.as_ptr()));
    }

    let rbase = reply.as_ptr() as *const u8;
    let rdata = *(rbase.add(PARCEL_DATA) as *const *const u8);
    let rlen = *(rbase.add(PARCEL_DATA_SIZE) as *const usize);
    let roffs = *(rbase.add(PARCEL_OBJECTS) as *const *const u64);
    let rcount = *(rbase.add(PARCEL_OBJECTS_SIZE) as *const usize);
    let out = if let Some(captured) = captured {
        captured
    } else if rdata.is_null() {
        (Vec::new(), Vec::new())
    } else {
        let mut bytes = std::slice::from_raw_parts(rdata, rlen).to_vec();
        let offs = if roffs.is_null() || rcount == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(roffs, rcount).to_vec()
        };
        stabilize(&mut bytes, &offs, inc_strong());
        (bytes, offs)
    };

    (api.parcel_dtor)(reply.as_mut_ptr() as *mut c_void);
    (api.parcel_dtor)(request.as_mut_ptr() as *mut c_void);

    if trace_enabled() {
        diag(&format!(
            "[codec2] code {code} -> rc={rc} {} bytes, {} objects\n",
            out.0.len(),
            out.1.len()
        ));
        let mut hex = String::new();
        for (i, b) in out.0.iter().enumerate() {
            if i % 16 == 0 {
                hex.push_str(&format!("\n[codec2]   {i:04x}: "));
            }
            hex.push_str(&format!("{b:02x} "));
        }
        hex.push_str(&format!("\n[codec2]   objects={:?}\n", out.1));
        diag(&hex);
    }
    Some(out)
}
