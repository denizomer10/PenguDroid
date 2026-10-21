//! A minimal JNI binding used by the ART backend.
//!
//! ART hands back a `JNIEnv*`, which is a pointer to a struct whose first member
//! is a pointer to a table of function pointers (`JNINativeInterface`). The
//! order of that table is fixed by the JNI specification, so instead of
//! mirroring a 230-field struct we index the table with the standard slot
//! numbers. [`JniEnv::version`] is used as a self-check: slot 4 must return
//! `JNI_VERSION_1_6`, which catches an off-by-one base.

use std::ffi::c_void;

/// JNI object references are opaque pointers.
pub type JObject = *mut c_void;
/// `jclass`
pub type JClass = JObject;
/// `jmethodID`
pub type JMethodId = *mut c_void;
/// `jfieldID`
pub type JFieldId = *mut c_void;
/// `jvalue`
#[repr(C)]
#[derive(Clone, Copy)]
pub union JValue {
    pub z: u8,
    pub b: i8,
    pub c: u16,
    pub s: i16,
    pub i: i32,
    pub j: i64,
    pub f: f32,
    pub d: f64,
    pub l: JObject,
}

pub const JNI_VERSION_1_6: i32 = 0x0001_0006;
pub const JNI_OK: i32 = 0;
pub const JNI_ERR: i32 = -1;

/// Slot numbers inside `JNINativeInterface` (see the JNI specification).
mod slot {
    pub const GET_VERSION: usize = 4;
    pub const FIND_CLASS: usize = 6;
    pub const THROW: usize = 13;
    pub const THROW_NEW: usize = 14;
    pub const EXCEPTION_OCCURRED: usize = 15;
    pub const EXCEPTION_DESCRIBE: usize = 16;
    pub const EXCEPTION_CLEAR: usize = 17;
    pub const EXCEPTION_CHECK: usize = 228;
    pub const PUSH_LOCAL_FRAME: usize = 19;
    pub const POP_LOCAL_FRAME: usize = 20;
    pub const NEW_GLOBAL_REF: usize = 21;
    pub const DELETE_LOCAL_REF: usize = 23;
    pub const GET_OBJECT_CLASS: usize = 31;
    pub const GET_METHOD_ID: usize = 33;
    pub const CALL_OBJECT_METHOD_A: usize = 36;
    pub const CALL_INT_METHOD_A: usize = 51;
    pub const NEW_OBJECT_A: usize = 30;
    pub const GET_FIELD_ID: usize = 94;
    pub const GET_OBJECT_FIELD: usize = 95;
    pub const SET_OBJECT_FIELD: usize = 104;
    pub const SET_BOOLEAN_FIELD: usize = 105;
    pub const SET_INT_FIELD: usize = 109;
    pub const GET_STATIC_OBJECT_FIELD: usize = 145;
    pub const GET_ARRAY_LENGTH: usize = 171;
    pub const GET_OBJECT_ARRAY_ELEMENT: usize = 173;
    pub const NEW_LONG_ARRAY: usize = 180;
    pub const NEW_FLOAT_ARRAY: usize = 181;
    pub const NEW_BYTE_ARRAY: usize = 176;
    pub const SET_BYTE_ARRAY_REGION: usize = 208;
    pub const SET_FLOAT_ARRAY_REGION: usize = 213;
    pub const CALL_VOID_METHOD_A: usize = 63;
    pub const GET_STATIC_METHOD_ID: usize = 113;
    pub const GET_STATIC_FIELD_ID: usize = 144;
    pub const CALL_STATIC_OBJECT_METHOD_A: usize = 116;
    pub const CALL_STATIC_INT_METHOD_A: usize = 131;
    pub const CALL_STATIC_LONG_METHOD_A: usize = 134;
    pub const CALL_STATIC_VOID_METHOD_A: usize = 143;
    pub const CALL_STATIC_VOID_METHOD: usize = 141;
    pub const GET_STATIC_INT_FIELD: usize = 150;
    pub const NEW_STRING_UTF: usize = 167;
    pub const GET_STRING_UTF_CHARS: usize = 169;
    pub const RELEASE_STRING_UTF_CHARS: usize = 170;
    pub const NEW_OBJECT_ARRAY: usize = 172;
    pub const GET_BYTE_ARRAY_ELEMENTS: usize = 184;
    pub const RELEASE_BYTE_ARRAY_ELEMENTS: usize = 192;
    pub const SET_OBJECT_ARRAY_ELEMENT: usize = 174;
}

/// A `JNIEnv*` wrapper.
#[derive(Clone, Copy)]
pub struct JniEnv(pub *mut c_void);

impl JniEnv {
    /// The function table. Slot 0 of the table is `GetVersion`'s predecessor
    /// only in the sense that the table itself starts at `reserved0`, so the
    /// table pointer must be read from `*env`.
    fn table(&self) -> *const *const c_void {
        // SAFETY: a JNIEnv is a pointer to a struct whose first member is the
        // function-table pointer.
        unsafe { *(self.0 as *const *const *const c_void) }
    }

    unsafe fn func<F: Copy>(&self, index: usize) -> F {
        let p = *self.table().add(index);
        debug_assert_eq!(std::mem::size_of::<F>(), std::mem::size_of::<*const c_void>());
        std::mem::transmute_copy(&p)
    }

    /// `GetVersion()` — used to validate the table layout.
    pub fn version(&self) -> i32 {
        let f: unsafe extern "C" fn(*mut c_void) -> i32 = unsafe { self.func(slot::GET_VERSION) };
        unsafe { f(self.0) }
    }

    /// `FindClass(name)`.
    pub fn find_class(&self, name: &str) -> Option<JClass> {
        let cname = std::ffi::CString::new(name).ok()?;
        let f: unsafe extern "C" fn(*mut c_void, *const i8) -> JClass =
            unsafe { self.func(slot::FIND_CLASS) };
        let c = unsafe { f(self.0, cname.as_ptr()) };
        if c.is_null() {
            self.describe_exception();
            return None;
        }
        Some(c)
    }

    /// `GetStaticMethodID(clazz, name, sig)`.
    pub fn static_method_id(&self, class: JClass, name: &str, sig: &str) -> Option<JMethodId> {
        let cname = std::ffi::CString::new(name).ok()?;
        let csig = std::ffi::CString::new(sig).ok()?;
        let f: unsafe extern "C" fn(*mut c_void, JClass, *const i8, *const i8) -> JMethodId =
            unsafe { self.func(slot::GET_STATIC_METHOD_ID) };
        let m = unsafe { f(self.0, class, cname.as_ptr(), csig.as_ptr()) };
        if m.is_null() {
            self.describe_exception();
            return None;
        }
        Some(m)
    }

    /// `GetStaticFieldID(clazz, name, sig)`.
    pub fn static_field_id(&self, class: JClass, name: &str, sig: &str) -> Option<JFieldId> {
        let cname = std::ffi::CString::new(name).ok()?;
        let csig = std::ffi::CString::new(sig).ok()?;
        let f: unsafe extern "C" fn(*mut c_void, JClass, *const i8, *const i8) -> JFieldId =
            unsafe { self.func(slot::GET_STATIC_FIELD_ID) };
        let id = unsafe { f(self.0, class, cname.as_ptr(), csig.as_ptr()) };
        if id.is_null() {
            self.describe_exception();
            return None;
        }
        Some(id)
    }

    /// `GetStaticIntField(clazz, field_id)`.
    pub fn static_int_field(&self, class: JClass, field: JFieldId) -> i32 {
        let f: unsafe extern "C" fn(*mut c_void, JClass, JFieldId) -> i32 =
            unsafe { self.func(slot::GET_STATIC_INT_FIELD) };
        unsafe { f(self.0, class, field) }
    }

    /// `NewStringUTF(bytes)`.
    pub fn new_string(&self, s: &str) -> JObject {
        let c = std::ffi::CString::new(s).unwrap_or_default();
        let f: unsafe extern "C" fn(*mut c_void, *const i8) -> JObject =
            unsafe { self.func(slot::NEW_STRING_UTF) };
        unsafe { f(self.0, c.as_ptr()) }
    }

    /// `GetMethodID(clazz, name, sig)`.
    pub fn method_id(&self, class: JClass, name: &str, sig: &str) -> Option<JMethodId> {
        let cname = std::ffi::CString::new(name).ok()?;
        let csig = std::ffi::CString::new(sig).ok()?;
        let f: unsafe extern "C" fn(*mut c_void, JClass, *const i8, *const i8) -> JMethodId =
            unsafe { self.func(slot::GET_METHOD_ID) };
        let m = unsafe { f(self.0, class, cname.as_ptr(), csig.as_ptr()) };
        if m.is_null() {
            self.describe_exception();
            return None;
        }
        Some(m)
    }

    /// `CallObjectMethodA(obj, method, args)`.
    pub fn call_object(&self, obj: JObject, method: JMethodId, args: &[JValue]) -> Option<JObject> {
        let f: unsafe extern "C" fn(*mut c_void, JObject, JMethodId, *const JValue) -> JObject =
            unsafe { self.func(slot::CALL_OBJECT_METHOD_A) };
        let v = unsafe { f(self.0, obj, method, args.as_ptr()) };
        self.exception_check().then_some(v)
    }

    /// `NewObjectA(clazz, method, args)`.
    pub fn new_object(&self, class: JClass, ctor: JMethodId, args: &[JValue]) -> Option<JObject> {
        let f: unsafe extern "C" fn(*mut c_void, JClass, JMethodId, *const JValue) -> JObject =
            unsafe { self.func(slot::NEW_OBJECT_A) };
        let v = unsafe { f(self.0, class, ctor, args.as_ptr()) };
        self.exception_check().then_some(v)
    }

    /// `GetFieldID(clazz, name, sig)`.
    pub fn field_id(&self, class: JClass, name: &str, sig: &str) -> Option<JFieldId> {
        let cname = std::ffi::CString::new(name).ok()?;
        let csig = std::ffi::CString::new(sig).ok()?;
        let f: unsafe extern "C" fn(*mut c_void, JClass, *const i8, *const i8) -> JFieldId =
            unsafe { self.func(slot::GET_FIELD_ID) };
        let id = unsafe { f(self.0, class, cname.as_ptr(), csig.as_ptr()) };
        if id.is_null() {
            self.describe_exception();
            return None;
        }
        Some(id)
    }

    /// `GetStaticObjectField(clazz, field)`.
    pub fn static_object_field(&self, class: JClass, field: JFieldId) -> JObject {
        let f: unsafe extern "C" fn(*mut c_void, JClass, JFieldId) -> JObject =
            unsafe { self.func(slot::GET_STATIC_OBJECT_FIELD) };
        unsafe { f(self.0, class, field) }
    }

    /// `GetObjectField(obj, field)`.
    pub fn object_field(&self, obj: JObject, field: JFieldId) -> JObject {
        let f: unsafe extern "C" fn(*mut c_void, JObject, JFieldId) -> JObject =
            unsafe { self.func(slot::GET_OBJECT_FIELD) };
        unsafe { f(self.0, obj, field) }
    }

    /// `SetObjectField(obj, field, value)`.
    pub fn set_object_field(&self, obj: JObject, field: JFieldId, value: JObject) {
        let f: unsafe extern "C" fn(*mut c_void, JObject, JFieldId, JObject) =
            unsafe { self.func(slot::SET_OBJECT_FIELD) };
        unsafe { f(self.0, obj, field, value) }
    }

    /// `SetIntField(obj, field, value)`.
    pub fn set_int_field(&self, obj: JObject, field: JFieldId, value: i32) {
        let f: unsafe extern "C" fn(*mut c_void, JObject, JFieldId, i32) =
            unsafe { self.func(slot::SET_INT_FIELD) };
        unsafe { f(self.0, obj, field, value) }
    }

    /// `SetBooleanField(obj, field, value)`.
    pub fn set_boolean_field(&self, obj: JObject, field: JFieldId, value: bool) {
        let f: unsafe extern "C" fn(*mut c_void, JObject, JFieldId, u8) =
            unsafe { self.func(slot::SET_BOOLEAN_FIELD) };
        unsafe { f(self.0, obj, field, value as u8) }
    }

    /// `CallVoidMethodA(obj, method, args)`.
    pub fn call_void(&self, obj: JObject, method: JMethodId, args: &[JValue]) -> bool {
        let f: unsafe extern "C" fn(*mut c_void, JObject, JMethodId, *const JValue) =
            unsafe { self.func(slot::CALL_VOID_METHOD_A) };
        unsafe { f(self.0, obj, method, args.as_ptr()) };
        self.exception_check()
    }

    /// `CallIntMethodA(obj, method, args)`.
    pub fn call_int(&self, obj: JObject, method: JMethodId, args: &[JValue]) -> Option<i32> {
        let f: unsafe extern "C" fn(*mut c_void, JObject, JMethodId, *const JValue) -> i32 =
            unsafe { self.func(slot::CALL_INT_METHOD_A) };
        let v = unsafe { f(self.0, obj, method, args.as_ptr()) };
        self.exception_check().then_some(v)
    }

    /// `GetArrayLength(array)`.
    pub fn array_length(&self, array: JObject) -> i32 {
        if array.is_null() {
            return 0;
        }
        let f: unsafe extern "C" fn(*mut c_void, JObject) -> i32 =
            unsafe { self.func(slot::GET_ARRAY_LENGTH) };
        unsafe { f(self.0, array) }
    }

    /// `GetObjectArrayElement(array, index)`.
    pub fn array_get(&self, array: JObject, index: i32) -> JObject {
        if array.is_null() {
            return std::ptr::null_mut();
        }
        let f: unsafe extern "C" fn(*mut c_void, JObject, i32) -> JObject =
            unsafe { self.func(slot::GET_OBJECT_ARRAY_ELEMENT) };
        unsafe { f(self.0, array, index) }
    }

    /// `NewObjectArray(len, elementClass, initial)`.
    pub fn new_object_array(&self, len: i32, class: JClass, initial: JObject) -> Option<JObject> {
        let f: unsafe extern "C" fn(*mut c_void, i32, JClass, JObject) -> JObject =
            unsafe { self.func(slot::NEW_OBJECT_ARRAY) };
        let v = unsafe { f(self.0, len, class, initial) };
        self.exception_check().then_some(v)
    }

    /// `SetObjectArrayElement(array, index, value)`.
    pub fn set_object_array_element(&self, array: JObject, index: i32, value: JObject) {
        let f: unsafe extern "C" fn(*mut c_void, JObject, i32, JObject) =
            unsafe { self.func(slot::SET_OBJECT_ARRAY_ELEMENT) };
        unsafe { f(self.0, array, index, value) }
    }

    /// `NewByteArray(len)`.
    pub fn new_byte_array(&self, len: i32) -> Option<JObject> {
        let f: unsafe extern "C" fn(*mut c_void, i32) -> JObject =
            unsafe { self.func(slot::NEW_BYTE_ARRAY) };
        let v = unsafe { f(self.0, len) };
        self.exception_check().then_some(v)
    }

    /// `SetByteArrayRegion(array, start, len, buf)`.
    pub fn set_byte_array_region(&self, array: JObject, start: i32, values: &[u8]) {
        let f: unsafe extern "C" fn(*mut c_void, JObject, i32, i32, *const i8) =
            unsafe { self.func(slot::SET_BYTE_ARRAY_REGION) };
        unsafe {
            f(
                self.0,
                array,
                start,
                values.len() as i32,
                values.as_ptr() as *const i8,
            )
        };
    }

    /// `NewFloatArray(len)`.
    pub fn new_float_array(&self, len: i32) -> JObject {
        let f: unsafe extern "C" fn(*mut c_void, i32) -> JObject =
            unsafe { self.func(slot::NEW_FLOAT_ARRAY) };
        unsafe { f(self.0, len) }
    }

    /// `SetFloatArrayRegion(array, start, len, buf)`.
    pub fn set_float_array_region(&self, array: JObject, start: i32, values: &[f32]) {
        let f: unsafe extern "C" fn(*mut c_void, JObject, i32, i32, *const f32) =
            unsafe { self.func(slot::SET_FLOAT_ARRAY_REGION) };
        unsafe { f(self.0, array, start, values.len() as i32, values.as_ptr()) };
    }

    /// `NewLongArray(len)`.
    pub fn new_long_array(&self, len: i32) -> Option<JObject> {
        let f: unsafe extern "C" fn(*mut c_void, i32) -> JObject =
            unsafe { self.func(slot::NEW_LONG_ARRAY) };
        let v = unsafe { f(self.0, len) };
        self.exception_check().then_some(v)
    }

    /// Copy a `byte[]` out of the VM.
    pub fn byte_array_copy(&self, array: JObject) -> Option<Vec<u8>> {
        if array.is_null() {
            return None;
        }
        let len = self.array_length(array);
        if len <= 0 {
            return Some(Vec::new());
        }
        let get: unsafe extern "C" fn(*mut c_void, JObject, *mut u8) -> *mut u8 =
            unsafe { self.func(slot::GET_BYTE_ARRAY_ELEMENTS) };
        let release: unsafe extern "C" fn(*mut c_void, JObject, *mut u8, i32) =
            unsafe { self.func(slot::RELEASE_BYTE_ARRAY_ELEMENTS) };
        let ptr = unsafe { get(self.0, array, std::ptr::null_mut()) };
        if ptr.is_null() {
            self.describe_exception();
            return None;
        }
        let out = unsafe { std::slice::from_raw_parts(ptr, len as usize) }.to_vec();
        // JNI_ABORT (2): the array is not modified, so do not copy back.
        unsafe { release(self.0, array, ptr, 2) };
        Some(out)
    }

    /// `Resolve a class through the *system* class loader.
    ///
    /// `FindClass` from a native thread only consults the boot class path, so
    /// application classes (anything in `-Djava.class.path`) have to be loaded
    /// the way Java code would load them. This mirrors what `dalvikvm` and
    /// `app_process` do.
    pub fn load_class(&self, name: &str) -> Option<JClass> {
        let dotted = name.trim_start_matches('L').trim_end_matches(';').replace('/', ".");
        let loader_class = self.find_class("java/lang/ClassLoader")?;
        let get_loader =
            self.static_method_id(loader_class, "getSystemClassLoader", "()Ljava/lang/ClassLoader;")?;
        let loader = self.call_static_object(loader_class, get_loader, &[])?;
        if loader.is_null() {
            return None;
        }
        let load = self.method_id(
            loader_class,
            "loadClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
        )?;
        let arg = self.new_string(&dotted);
        let result = self.call_object(loader, load, &[JValue { l: arg }]);
        match result {
            Some(c) if !c.is_null() => Some(c),
            _ => None,
        }
    }

    /// `GetStringUTFChars` / `ReleaseStringUTFChars`.
    pub fn get_string(&self, s: JObject) -> Option<String> {
        if s.is_null() {
            return None;
        }
        let get: unsafe extern "C" fn(*mut c_void, JObject, *mut u8) -> *const i8 =
            unsafe { self.func(slot::GET_STRING_UTF_CHARS) };
        let release: unsafe extern "C" fn(*mut c_void, JObject, *const i8) =
            unsafe { self.func(slot::RELEASE_STRING_UTF_CHARS) };
        unsafe {
            let p = get(self.0, s, std::ptr::null_mut());
            if p.is_null() {
                return None;
            }
            let out = std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned();
            release(self.0, s, p);
            Some(out)
        }
    }

    /// `NewObjectArray(len, String, null)` filled with `args`.
    pub fn string_array(&self, args: &[String]) -> Option<JObject> {
        let string_class = self.find_class("java/lang/String")?;
        let new_arr: unsafe extern "C" fn(*mut c_void, i32, JClass, JObject) -> JObject =
            unsafe { self.func(slot::NEW_OBJECT_ARRAY) };
        let set: unsafe extern "C" fn(*mut c_void, JObject, i32, JObject) =
            unsafe { self.func(slot::SET_OBJECT_ARRAY_ELEMENT) };
        unsafe {
            let arr = new_arr(self.0, args.len() as i32, string_class, std::ptr::null_mut());
            if arr.is_null() {
                self.describe_exception();
                return None;
            }
            for (i, a) in args.iter().enumerate() {
                let s = self.new_string(a);
                set(self.0, arr, i as i32, s);
            }
            Some(arr)
        }
    }

    /// `CallStaticVoidMethodA(clazz, method, args)`.
    pub fn call_static_void(&self, class: JClass, method: JMethodId, args: &[JValue]) -> bool {
        let f: unsafe extern "C" fn(*mut c_void, JClass, JMethodId, *const JValue) =
            unsafe { self.func(slot::CALL_STATIC_VOID_METHOD_A) };
        unsafe { f(self.0, class, method, args.as_ptr()) };
        self.exception_check()
    }

    /// `CallStaticIntMethodA(clazz, method, args)`.
    pub fn call_static_int(&self, class: JClass, method: JMethodId, args: &[JValue]) -> Option<i32> {
        let f: unsafe extern "C" fn(*mut c_void, JClass, JMethodId, *const JValue) -> i32 =
            unsafe { self.func(slot::CALL_STATIC_INT_METHOD_A) };
        let v = unsafe { f(self.0, class, method, args.as_ptr()) };
        self.exception_check().then_some(v)
    }

    /// `CallStaticLongMethodA(clazz, method, args)`.
    pub fn call_static_long(&self, class: JClass, method: JMethodId, args: &[JValue]) -> Option<i64> {
        let f: unsafe extern "C" fn(*mut c_void, JClass, JMethodId, *const JValue) -> i64 =
            unsafe { self.func(slot::CALL_STATIC_LONG_METHOD_A) };
        let v = unsafe { f(self.0, class, method, args.as_ptr()) };
        self.exception_check().then_some(v)
    }

    /// `CallStaticObjectMethodA(clazz, method, args)`.
    pub fn call_static_object(
        &self,
        class: JClass,
        method: JMethodId,
        args: &[JValue],
    ) -> Option<JObject> {
        let f: unsafe extern "C" fn(*mut c_void, JClass, JMethodId, *const JValue) -> JObject =
            unsafe { self.func(slot::CALL_STATIC_OBJECT_METHOD_A) };
        let v = unsafe { f(self.0, class, method, args.as_ptr()) };
        self.exception_check().then_some(v)
    }

    /// `ExceptionOccurred` + `ExceptionDescribe` + `ExceptionClear`.
    pub fn describe_exception(&self) {
        unsafe {
            let occurred: unsafe extern "C" fn(*mut c_void) -> JObject =
                self.func(slot::EXCEPTION_OCCURRED);
            if occurred(self.0).is_null() {
                return;
            }
            let describe: unsafe extern "C" fn(*mut c_void) = self.func(slot::EXCEPTION_DESCRIBE);
            describe(self.0);
            let clear: unsafe extern "C" fn(*mut c_void) = self.func(slot::EXCEPTION_CLEAR);
            clear(self.0);
        }
    }

    /// Returns `true` when no exception is pending (and logs it otherwise).
    pub fn exception_check(&self) -> bool {
        unsafe {
            let check: unsafe extern "C" fn(*mut c_void) -> u8 = self.func(slot::EXCEPTION_CHECK);
            if check(self.0) == 0 {
                return true;
            }
            let describe: unsafe extern "C" fn(*mut c_void) = self.func(slot::EXCEPTION_DESCRIBE);
            describe(self.0);
            let clear: unsafe extern "C" fn(*mut c_void) = self.func(slot::EXCEPTION_CLEAR);
            clear(self.0);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jvalue_is_8_bytes() {
        assert_eq!(std::mem::size_of::<JValue>(), 8);
    }

    #[test]
    fn slot_numbers_match_the_specification() {
        // Anchors from the JNI specification's table order. Note that only the
        // `Call*Method` families have `V`/`A` variants; field accessors do not,
        // which is why `GetStaticMethodID` is 113 and not 149.
        assert_eq!(slot::GET_VERSION, 4);
        assert_eq!(slot::FIND_CLASS, 6);
        assert_eq!(slot::GET_METHOD_ID, 33);
        assert_eq!(slot::GET_STATIC_METHOD_ID, 113);
        assert_eq!(slot::CALL_STATIC_VOID_METHOD_A, 143);
        assert_eq!(slot::NEW_STRING_UTF, 167);
        assert_eq!(slot::EXCEPTION_CHECK, 228);
    }
}
