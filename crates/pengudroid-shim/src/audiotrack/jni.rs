//! The standard JNI C table only; no ART, Java-object, or private C++ layouts.
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};

pub(super) type Env = *mut c_void;
pub(super) type Object = *mut c_void;
pub(super) type Id = *mut c_void;

#[repr(C)]
pub(crate) struct NativeMethod {
    pub(crate) name: *const c_char,
    pub(crate) signature: *const c_char,
    pub(crate) fn_ptr: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) union Value {
    pub(super) i: i32,
    pub(super) l: Object,
}

pub(crate) unsafe fn slot(env: Env, index: usize) -> *const c_void {
    *(*(env as *const *const *const c_void)).add(index)
}

macro_rules! call {
    ($env:expr, $index:expr, ($($arg:expr),*) : ($($ty:ty),*) -> $ret:ty) => {{
        let f: unsafe extern "C" fn(Env, $($ty),*) -> $ret =
            std::mem::transmute(slot($env, $index));
        f($env, $($arg),*)
    }};
}

pub(crate) unsafe fn find_class(env: Env, name: &CStr) -> Object {
    call!(env, 6, (name.as_ptr()): (*const c_char) -> Object)
}
pub(super) unsafe fn exception_check(env: Env) -> bool {
    call!(env, 228, (): () -> u8) != 0
}
pub(super) unsafe fn delete_local_ref(env: Env, object: Object) {
    if !object.is_null() { call!(env, 23, (object): (Object) -> ()); }
}
pub(super) unsafe fn delete_global_ref(env: Env, object: Object) {
    if !object.is_null() { call!(env, 22, (object): (Object) -> ()); }
}
pub(super) unsafe fn new_global_ref(env: Env, object: Object) -> Object {
    call!(env, 21, (object): (Object) -> Object)
}
pub(super) unsafe fn is_same(env: Env, left: Object, right: Object) -> bool {
    call!(env, 24, (left, right): (Object, Object) -> u8) != 0
}
pub(super) unsafe fn is_instance(env: Env, object: Object, class: Object) -> bool {
    !object.is_null() && call!(env, 32, (object, class): (Object, Object) -> u8) != 0
}
pub(super) unsafe fn method(env: Env, class: Object, name: &CStr, sig: &CStr) -> Id {
    call!(env, 33, (class, name.as_ptr(), sig.as_ptr()):
        (Object, *const c_char, *const c_char) -> Id)
}
pub(super) unsafe fn field(env: Env, class: Object, name: &CStr, sig: &CStr) -> Id {
    call!(env, 94, (class, name.as_ptr(), sig.as_ptr()):
        (Object, *const c_char, *const c_char) -> Id)
}
pub(super) unsafe fn get_long(env: Env, object: Object, id: Id) -> i64 {
    call!(env, 101, (object, id): (Object, Id) -> i64)
}
pub(super) unsafe fn set_long(env: Env, object: Object, id: Id, value: i64) {
    call!(env, 110, (object, id, value): (Object, Id, i64) -> ())
}
pub(super) unsafe fn set_int(env: Env, object: Object, id: Id, value: i32) {
    call!(env, 109, (object, id, value): (Object, Id, i32) -> ())
}
pub(super) unsafe fn call_object(env: Env, object: Object, id: Id, args: &[Value]) -> Object {
    call!(env, 36, (object, id, args.as_ptr()): (Object, Id, *const Value) -> Object)
}
pub(super) unsafe fn call_int(env: Env, object: Object, id: Id, args: &[Value]) -> i32 {
    call!(env, 51, (object, id, args.as_ptr()): (Object, Id, *const Value) -> i32)
}
pub(super) unsafe fn new_object(env: Env, class: Object, id: Id) -> Object {
    call!(env, 30, (class, id, std::ptr::null()): (Object, Id, *const Value) -> Object)
}
pub(super) unsafe fn array_length(env: Env, array: Object) -> i32 {
    call!(env, 171, (array): (Object) -> i32)
}
pub(super) unsafe fn object_element(env: Env, array: Object, index: i32) -> Object {
    call!(env, 173, (array, index): (Object, i32) -> Object)
}
pub(super) unsafe fn new_int_array(env: Env, size: i32) -> Object {
    call!(env, 179, (size): (i32) -> Object)
}
pub(super) unsafe fn get_ints(env: Env, array: Object, start: i32, out: &mut [i32]) {
    call!(env, 203, (array, start, out.len() as i32, out.as_mut_ptr()):
        (Object, i32, i32, *mut i32) -> ())
}
pub(super) unsafe fn get_bytes(env: Env, array: Object, start: i32, out: &mut [u8]) {
    call!(env, 200, (array, start, out.len() as i32, out.as_mut_ptr().cast()):
        (Object, i32, i32, *mut i8) -> ())
}
pub(super) unsafe fn get_shorts(env: Env, array: Object, start: i32, out: &mut [i16]) {
    call!(env, 202, (array, start, out.len() as i32, out.as_mut_ptr()):
        (Object, i32, i32, *mut i16) -> ())
}
pub(super) unsafe fn get_floats(env: Env, array: Object, start: i32, out: &mut [f32]) {
    call!(env, 205, (array, start, out.len() as i32, out.as_mut_ptr()):
        (Object, i32, i32, *mut f32) -> ())
}
pub(super) unsafe fn set_ints(env: Env, array: Object, values: &[i32]) {
    call!(env, 211, (array, 0, values.len() as i32, values.as_ptr()):
        (Object, i32, i32, *const i32) -> ())
}
pub(super) unsafe fn set_longs(env: Env, array: Object, values: &[i64]) {
    call!(env, 212, (array, 0, values.len() as i32, values.as_ptr()):
        (Object, i32, i32, *const i64) -> ())
}
pub(super) unsafe fn direct_capacity(env: Env, buffer: Object) -> i64 {
    call!(env, 231, (buffer): (Object) -> i64)
}
pub(super) unsafe fn direct_address(env: Env, buffer: Object) -> *const u8 {
    call!(env, 230, (buffer): (Object) -> Object).cast()
}
pub(super) unsafe fn register_natives(env: Env, class: Object, methods: &[NativeMethod]) -> i32 {
    call!(env, 215, (class, methods.as_ptr(), methods.len() as i32):
        (Object, *const NativeMethod, i32) -> i32)
}
pub(super) unsafe fn java_vm(env: Env) -> Result<usize, ()> {
    let mut vm: *mut c_void = std::ptr::null_mut();
    let rc = call!(env, 219, (&mut vm): (*mut *mut c_void) -> i32);
    if rc != 0 || vm.is_null() || exception_check(env) { Err(()) } else { Ok(vm as usize) }
}
pub(super) unsafe fn sdk_int(env: Env) -> Result<i32, ()> {
    let class = find_class(env, c"android/os/Build$VERSION");
    if class.is_null() || exception_check(env) { return Err(()); }
    let id = call!(env, 144, (class, c"SDK_INT".as_ptr(), c"I".as_ptr()):
        (Object, *const c_char, *const c_char) -> Id);
    if id.is_null() || exception_check(env) { delete_local_ref(env, class); return Err(()); }
    let value = call!(env, 150, (class, id): (Object, Id) -> i32);
    delete_local_ref(env, class);
    if exception_check(env) { Err(()) } else { Ok(value) }
}

pub(super) unsafe fn string(env: Env, object: Object) -> Result<String, ()> {
    if object.is_null() || exception_check(env) { return Err(()); }
    let ptr = call!(env, 169, (object, std::ptr::null_mut()):
        (Object, *mut u8) -> *const c_char);
    if ptr.is_null() || exception_check(env) { return Err(()); }
    // Class/method names and descriptors are ASCII in this contract.
    let value = CStr::from_ptr(ptr).to_str().map(str::to_owned).map_err(|_| ());
    call!(env, 170, (object, ptr): (Object, *const c_char) -> ());
    value
}

pub(super) unsafe fn throw(env: Env, class: &CStr, message: &str) {
    if env.is_null() || exception_check(env) { return; }
    let kind = find_class(env, class);
    if kind.is_null() || exception_check(env) { return; }
    let message = CString::new(message).expect("audio exception contains no NUL");
    call!(env, 14, (kind, message.as_ptr()): (Object, *const c_char) -> i32);
    delete_local_ref(env, kind);
}

/// Bounds every temporary reference made by reflection/setup, on every exit.
pub(super) struct LocalFrame(Env);
impl LocalFrame {
    pub(super) unsafe fn new(env: Env, capacity: i32) -> Result<Self, ()> {
        if call!(env, 19, (capacity): (i32) -> i32) != 0 || exception_check(env) {
            Err(())
        } else { Ok(Self(env)) }
    }
}
impl Drop for LocalFrame {
    fn drop(&mut self) {
        unsafe { call!(self.0, 20, (std::ptr::null_mut()): (Object) -> Object); }
    }
}
