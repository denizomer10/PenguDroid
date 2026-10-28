//! Deterministic JNI-table tests; no ART process, audio server, or external sink.
use super::*;
use super::super::contract::{Declaration, NATIVE, STATIC};
use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_void};

#[derive(Clone)]
enum Java {
    Class(String), Method(Declaration), String(String), Objects(Vec<Object>),
    Track { handle: i64, jni_data: i64, stream: i32 }, Masks(ChannelMasks), Attributes(i32),
    Ints(Vec<i32>), Longs(Vec<i64>), Bytes(Vec<u8>), Shorts(Vec<i16>), Floats(Vec<f32>),
    Direct(Vec<u8>), Params, Parcel,
}
struct State {
    objects: Vec<Box<Java>>,
    classes: HashMap<String, Object>,
    declarations: Vec<Declaration>,
    sdk: i32,
    pending: bool,
    exception: String,
    register_calls: Vec<Vec<(String, String, usize)>>,
    fail_register: bool,
    register_exception: bool,
    globals: usize,
    frames: i32,
    reads: usize,
}
#[repr(C)]
struct FakeEnv {
    table: *const *const c_void,
    state: *mut State,
}
struct Harness {
    table: Box<[*const c_void; 232]>,
    state: Box<State>,
    env: Box<FakeEnv>,
}
impl Harness {
    fn new() -> Self {
        let mut table = Box::new([std::ptr::null(); 232]);
        macro_rules! bind { ($slot:expr, $fn:ident) => { table[$slot] = $fn as *const () as *const c_void; }; }
        bind!(6, find_class); bind!(14, throw_new); bind!(19, push_frame); bind!(20, pop_frame);
        bind!(21, new_global); bind!(22, delete_global); bind!(23, delete_local); bind!(24, same);
        bind!(30, new_object); bind!(32, instance); bind!(33, method_id); bind!(36, call_object);
        bind!(51, call_int); bind!(94, field_id); bind!(101, get_long); bind!(109, set_int); bind!(110, set_long);
        bind!(144, field_id); bind!(150, sdk_int); bind!(169, utf); bind!(170, release_utf);
        bind!(171, array_length); bind!(173, object_element); bind!(179, new_ints);
        bind!(200, get_bytes); bind!(202, get_shorts); bind!(203, get_ints); bind!(205, get_floats);
        bind!(211, set_ints); bind!(212, set_longs); bind!(215, register_natives);
        bind!(219, java_vm); bind!(228, exception_check); bind!(230, direct_address); bind!(231, direct_capacity);
        let mut state = Box::new(State {
            objects: Vec::new(), classes: HashMap::new(),
            declarations: super::super::runtime_contract::track_declarations(), sdk: 37,
            pending: false, exception: String::new(), register_calls: Vec::new(), fail_register: false, register_exception: false,
            globals: 0, frames: 0, reads: 0,
        });
        let env = Box::new(FakeEnv { table: table.as_ptr(), state: &mut *state });
        Self { table, state, env }
    }
    fn env(&mut self) -> Env { (&mut *self.env as *mut FakeEnv).cast() }
    fn alloc(&mut self, value: Java) -> Object { self.state.alloc(value) }
    fn clear_exception(&mut self) { self.state.pending = false; self.state.exception.clear(); }
    fn setup(&mut self, rate: i32, session: i32, encoding: i32) -> (Object, Object, Object, i32) {
        let track = self.alloc(Java::Track { handle: 0, jni_data: 0, stream: -1 });
        let attributes = self.alloc(Java::Attributes(0));
        let rates = self.alloc(Java::Ints(vec![rate]));
        let sessions = self.alloc(Java::Ints(vec![session]));
        let masks = self.alloc(Java::Masks(ChannelMasks { position: 12, index: 0, acn: 0, count: 2 }));
        let parcel = self.alloc(Java::Parcel);
        let result = unsafe { native_setup(self.env(), track, std::ptr::null_mut(), attributes, rates, masks,
            encoding, 32, 1, sessions, parcel, 0, 0, 0, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
        (track, rates, sessions, result)
    }
    fn handle(&self, track: Object) -> i64 {
        match unsafe { &*track.cast::<Java>() } { Java::Track { handle, .. } => *handle, _ => panic!("not a track") }
    }
}
impl State {
    fn alloc(&mut self, value: Java) -> Object {
        let mut value = Box::new(value);
        let ptr = (&mut *value as *mut Java).cast();
        self.objects.push(value);
        ptr
    }
    fn class(&mut self, name: &str) -> Object {
        if let Some(class) = self.classes.get(name) { return *class; }
        let class = self.alloc(Java::Class(name.into()));
        self.classes.insert(name.into(), class); class
    }
}
unsafe fn state<'a>(env: Env) -> &'a mut State { &mut *(*(env.cast::<FakeEnv>())).state }
unsafe fn object<'a>(object: Object) -> &'a mut Java { &mut *object.cast::<Java>() }
unsafe extern "C" fn exception_check(env: Env) -> u8 { state(env).pending as u8 }
unsafe extern "C" fn find_class(env: Env, name: *const c_char) -> Object { state(env).class(CStr::from_ptr(name).to_str().unwrap()) }
unsafe extern "C" fn throw_new(env: Env, _class: Object, message: *const c_char) -> i32 {
    state(env).pending = true; state(env).exception = CStr::from_ptr(message).to_string_lossy().into_owned(); 0
}
unsafe extern "C" fn push_frame(env: Env, _capacity: i32) -> i32 { state(env).frames += 1; 0 }
unsafe extern "C" fn pop_frame(env: Env, _result: Object) -> Object { state(env).frames -= 1; std::ptr::null_mut() }
unsafe extern "C" fn new_global(env: Env, value: Object) -> Object { state(env).globals += 1; value }
unsafe extern "C" fn delete_global(env: Env, _value: Object) { state(env).globals -= 1; }
unsafe extern "C" fn delete_local(_env: Env, _value: Object) {}
unsafe extern "C" fn same(_env: Env, left: Object, right: Object) -> u8 { (left == right) as u8 }
unsafe extern "C" fn java_vm(_env: Env, out: *mut *mut c_void) -> i32 { *out = 1usize as *mut c_void; 0 }
unsafe extern "C" fn sdk_int(env: Env, _class: Object, _field: Id) -> i32 { state(env).sdk }
unsafe extern "C" fn field_id(_env: Env, _class: Object, name: *const c_char, _sig: *const c_char) -> Id {
    (match CStr::from_ptr(name).to_bytes() { b"mNativeTrackInJavaObj" => 1, b"mJniData" => 2, b"mStreamType" => 3, b"SDK_INT" => 4, _ => panic!("unknown field") }) as Id
}
unsafe extern "C" fn method_id(_env: Env, _class: Object, name: *const c_char, _sig: *const c_char) -> Id {
    (match CStr::from_ptr(name).to_bytes() {
        b"getDeclaredMethods" => 1, b"getName" => 2, b"getModifiers" => 3, b"getParameterTypes" => 4,
        b"getReturnType" => 5, b"getPositionMask" => 6, b"getIndexMask" => 7, b"getAcnMask" => 8,
        b"getChannelCount" => 9, b"getFlags" => 10, b"getVolumeControlStream" => 11,
        b"<init>" => 12, b"allowDefaults" => 13, _ => panic!("unknown method"),
    }) as Id
}
unsafe extern "C" fn instance(_env: Env, value: Object, class: Object) -> u8 {
    if value.is_null() { return 0; }
    let Java::Class(name) = object(class) else { panic!("not a class") };
    matches!((object(value), name.as_str()),
        (Java::Track { .. }, "android/media/AudioTrack") |
        (Java::Masks(_), "android/media/AudioFormat$ChannelMasks") |
        (Java::Attributes(_), "android/media/AudioAttributes")) as u8
}
fn type_names(descriptor: &str) -> Vec<String> {
    let bytes = descriptor.as_bytes();
    let mut i = 0; let mut names = Vec::new();
    while i < bytes.len() {
        let start = i;
        while bytes[i] == b'[' { i += 1; }
        let array = i != start;
        let first = i; i += 1;
        if bytes[first] == b'L' { while bytes[i] != b';' { i += 1; } i += 1; }
        let name = if array { descriptor[start..i].replace('/', ".") }
            else if bytes[first] == b'L' { descriptor[first + 1..i - 1].replace('/', ".") }
            else { match bytes[first] { b'V' => "void", b'I' => "int", b'J' => "long", b'F' => "float", b'Z' => "boolean", _ => panic!("unknown primitive") }.into() };
        names.push(name);
    }
    names
}
unsafe extern "C" fn call_object(env: Env, value: Object, method: Id, _args: *const jni::Value) -> Object {
    match method as usize {
        1 => {
            let Java::Class(name) = object(value) else { panic!("not a class") };
            let declarations = if name == "android/media/AudioTrack" { state(env).declarations.clone() } else {
                ["newAudioSessionId", "newAudioPlayerId", "newAudioRecorderId"].iter().map(|name|
                    Declaration { name: (*name).into(), descriptor: "()I".into(), modifiers: NATIVE | STATIC }).collect()
            };
            let methods = declarations.into_iter().map(|d| state(env).alloc(Java::Method(d))).collect();
            state(env).alloc(Java::Objects(methods))
        }
        2 => {
            let name = match object(value) { Java::Class(name) | Java::String(name) => name.clone(), Java::Method(d) => d.name.clone(), _ => panic!("no name") };
            state(env).alloc(Java::String(name))
        }
        4 | 5 => {
            let Java::Method(declaration) = object(value) else { panic!("not a method") };
            let descriptor = declaration.descriptor.clone();
            let split = descriptor.find(')').unwrap();
            let names = if method as usize == 4 { type_names(&descriptor[1..split]) } else { type_names(&descriptor[split + 1..]) };
            let classes: Vec<_> = names.into_iter().map(|name| state(env).class(&name)).collect();
            if method as usize == 4 { state(env).alloc(Java::Objects(classes)) } else { classes[0] }
        }
        13 => value,
        _ => panic!("unknown object call"),
    }
}
unsafe extern "C" fn call_int(_env: Env, value: Object, method: Id, _args: *const jni::Value) -> i32 {
    match (object(value), method as usize) {
        (Java::Method(d), 3) => d.modifiers,
        (Java::Masks(m), 6) => m.position, (Java::Masks(m), 7) => m.index,
        (Java::Masks(m), 8) => m.acn, (Java::Masks(m), 9) => m.count,
        (Java::Attributes(flags), 10) => *flags, (Java::Attributes(_), 11) => 3,
        _ => panic!("unknown int call"),
    }
}
unsafe extern "C" fn new_object(env: Env, _class: Object, _ctor: Id, _args: *const jni::Value) -> Object { state(env).alloc(Java::Params) }
unsafe extern "C" fn utf(_env: Env, value: Object, _copy: *mut u8) -> *const c_char {
    let Java::String(value) = object(value) else { panic!("not a string") };
    std::ffi::CString::new(value.as_str()).unwrap().into_raw()
}
unsafe extern "C" fn release_utf(_env: Env, _value: Object, ptr: *const c_char) { drop(std::ffi::CString::from_raw(ptr.cast_mut())); }
unsafe extern "C" fn array_length(_env: Env, value: Object) -> i32 {
    (match object(value) { Java::Objects(v) => v.len(), Java::Ints(v) => v.len(), Java::Longs(v) => v.len(),
        Java::Bytes(v) => v.len(), Java::Shorts(v) => v.len(), Java::Floats(v) => v.len(), _ => panic!("not an array") }) as i32
}
unsafe extern "C" fn object_element(_env: Env, value: Object, index: i32) -> Object {
    let Java::Objects(values) = object(value) else { panic!("not an object array") }; values[index as usize]
}
unsafe extern "C" fn new_ints(env: Env, count: i32) -> Object { state(env).alloc(Java::Ints(vec![0; count as usize])) }
unsafe extern "C" fn get_ints(_env: Env, value: Object, offset: i32, count: i32, out: *mut i32) {
    let Java::Ints(values) = object(value) else { panic!("not ints (setup ABI mismatch)") };
    std::ptr::copy_nonoverlapping(values.as_ptr().add(offset as usize), out, count as usize);
}
unsafe extern "C" fn set_ints(_env: Env, value: Object, offset: i32, count: i32, input: *const i32) {
    let Java::Ints(values) = object(value) else { panic!("not ints") };
    values[offset as usize..offset as usize + count as usize].copy_from_slice(std::slice::from_raw_parts(input, count as usize));
}
unsafe extern "C" fn set_longs(_env: Env, value: Object, offset: i32, count: i32, input: *const i64) {
    let Java::Longs(values) = object(value) else { panic!("not longs") };
    values[offset as usize..offset as usize + count as usize].copy_from_slice(std::slice::from_raw_parts(input, count as usize));
}
macro_rules! region {
    ($name:ident, $variant:ident, $ty:ty) => {
        unsafe extern "C" fn $name(env: Env, value: Object, offset: i32, count: i32, out: *mut $ty) {
            state(env).reads += 1;
            let Java::$variant(values) = object(value) else { panic!("wrong array region type") };
            assert!(offset >= 0 && count >= 0 && offset as usize + count as usize <= values.len());
            std::ptr::copy_nonoverlapping(values.as_ptr().add(offset as usize).cast::<$ty>(), out, count as usize);
        }
    };
}
region!(get_bytes, Bytes, i8); region!(get_shorts, Shorts, i16); region!(get_floats, Floats, f32);
unsafe extern "C" fn direct_capacity(_env: Env, value: Object) -> i64 {
    match object(value) { Java::Direct(v) => v.len() as i64, _ => -1 }
}
unsafe extern "C" fn direct_address(_env: Env, value: Object) -> Object {
    match object(value) { Java::Direct(v) => v.as_mut_ptr().cast(), _ => std::ptr::null_mut() }
}
unsafe extern "C" fn get_long(_env: Env, value: Object, field: Id) -> i64 {
    match (object(value), field as usize) { (Java::Track { handle, .. }, 1) => *handle, (Java::Track { jni_data, .. }, 2) => *jni_data, _ => panic!("wrong long field") }
}
unsafe extern "C" fn set_long(_env: Env, value: Object, field: Id, input: i64) {
    match (object(value), field as usize) { (Java::Track { handle, .. }, 1) => *handle = input, (Java::Track { jni_data, .. }, 2) => *jni_data = input, _ => panic!("wrong long field") }
}
unsafe extern "C" fn set_int(_env: Env, value: Object, field: Id, input: i32) {
    let (Java::Track { stream, .. }, 3) = (object(value), field as usize) else { panic!("wrong int field") }; *stream = input;
}
unsafe extern "C" fn register_natives(env: Env, _class: Object, methods: *const jni::NativeMethod, count: i32) -> i32 {
    let methods = std::slice::from_raw_parts(methods, count as usize).iter().map(|m|
        (CStr::from_ptr(m.name).to_str().unwrap().into(), CStr::from_ptr(m.signature).to_str().unwrap().into(), m.fn_ptr as usize)).collect();
    state(env).register_calls.push(methods);
    if state(env).register_exception { state(env).pending = true; }
    if state(env).fail_register { -1 } else { 0 }
}

#[test]
fn registration_and_jni_lifecycle_are_fail_closed_typed_bounded_and_repeatable() {
    // One mock VM owns the process-lifetime metadata, just like the real adapter.
    let mut h = Harness::new();
    assert_eq!(h.table.len(), 232);
    unsafe {
        h.state.pending = true;
        assert_eq!(register(h.env()), -1); assert!(h.state.pending); h.clear_exception();
        h.state.sdk = 33;
        assert_eq!(register(h.env()), -1); h.state.sdk = 37;
        let original = h.state.declarations.clone();
        for change in 0..5 {
            h.state.declarations = original.clone();
            match change {
                0 => h.state.declarations.iter_mut().find(|d| d.name == "native_setup").unwrap().descriptor = "([IIIII[I)I".into(),
                1 => h.state.declarations[0].modifiers ^= STATIC,
                2 => h.state.declarations[0].modifiers &= !NATIVE,
                3 => { h.state.declarations.pop(); },
                _ => h.state.declarations.push(Declaration { name: "native_unknown".into(), descriptor: "()V".into(), modifiers: NATIVE }),
            }
            assert_eq!(register(h.env()), -1);
            assert!(h.state.register_calls.is_empty());
            assert_eq!(h.state.globals, 0); assert_eq!(h.state.frames, 0);
            let (track, _, _, result) = h.setup(44_100, 51, engine::PCM_16);
            assert_eq!(result, INVALID_OPERATION); assert_eq!(h.handle(track), 0);
        }
        h.state.declarations = original;
        h.state.fail_register = true;
        assert_eq!(register(h.env()), -1);
        assert_eq!(h.state.globals, 4); assert_eq!(h.state.frames, 0);
        let (track, _, _, result) = h.setup(44_100, 51, engine::PCM_16);
        assert_eq!(result, INVALID_OPERATION); assert_eq!(h.handle(track), 0);
        h.state.fail_register = false;
        h.state.register_exception = true;
        assert_eq!(register(h.env()), -1);
        assert!(h.state.pending); h.clear_exception(); h.state.register_exception = false;
        assert_eq!(register(h.env()), 0);
        assert_eq!(register(h.env()), 0);
        assert_eq!(h.state.globals, 4); assert_eq!(h.state.frames, 0);
        assert_eq!(h.state.register_calls.iter().map(Vec::len).collect::<Vec<_>>(), [57, 57, 57, 2, 57, 2]);
        let (track, rates, sessions, result) = h.setup(44_100, 51, engine::PCM_16);
        assert_eq!(result, 0); assert!(!h.state.pending);
        assert!(matches!(object(rates), Java::Ints(v) if v == &[44_100]));
        assert!(matches!(object(sessions), Java::Ints(v) if v == &[51]));
        let handle = h.handle(track); assert!(handle > 0);
        assert_eq!(native_buffer_capacity(h.env(), track), 8);
        assert_eq!(native_resize(h.env(), track, 4), 4);
        assert_eq!(native_buffer_size(h.env(), track), 4);
        assert_eq!(native_get_rate(h.env(), track), 44_100);
        assert_eq!(native_set_rate(h.env(), track, 48_000), INVALID_OPERATION);
        assert_eq!(native_set_threshold(h.env(), track, 99), 4);
        assert_eq!(native_get_threshold(h.env(), track), 4);
        // Unsupported ownership modes never touch outputs or publish a handle.
        let unowned = h.alloc(Java::Track { handle: 0, jni_data: 0, stream: -1 });
        let attributes = h.alloc(Java::Attributes(0));
        let masks = h.alloc(Java::Masks(ChannelMasks { position: 12, index: 0, acn: 0, count: 2 }));
        let parcel = h.alloc(Java::Parcel);
        for (attachment, offload, mode, encapsulation, tuner) in [
            (1, 0, 1, 0, std::ptr::null_mut()), (0, 1, 1, 0, std::ptr::null_mut()),
            (0, 0, 0, 0, std::ptr::null_mut()), (0, 0, 1, 1, std::ptr::null_mut()),
            (0, 0, 1, 0, parcel),
        ] {
            assert_eq!(native_setup(h.env(), unowned, std::ptr::null_mut(), attributes, rates, masks,
                engine::PCM_16, 32, mode, sessions, parcel, attachment, offload, encapsulation, tuner,
                std::ptr::null_mut(), std::ptr::null_mut()), INVALID_OPERATION);
            assert_eq!(h.handle(unowned), 0);
            assert!(matches!(object(rates), Java::Ints(v) if v == &[44_100]));
            assert!(matches!(object(sessions), Java::Ints(v) if v == &[51]));
        }
        assert_eq!(native_setup(h.env(), unowned, std::ptr::null_mut(), attributes, rates, masks,
            engine::PCM_16, 32, 1, rates, parcel, 0, 0, 0, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()), BAD_VALUE);
        assert_eq!(h.handle(unowned), 0);
        // Invoke an erased table entry through the descriptor's exact JNI type.
        let write_short_ptr = h.state.register_calls[2].iter().find(|m| m.0 == "native_write_short").unwrap().2;
        let write_short: unsafe extern "C" fn(Env, Object, Object, i32, i32, i32, u8) -> i32 = std::mem::transmute(write_short_ptr);
        let shorts = h.alloc(Java::Shorts((0..32).collect()));
        assert_eq!(write_short(h.env(), track, shorts, 2, 12, engine::PCM_16, 0), 8); // 16 source bytes / sizeof(short)
        assert_eq!(native_written_frames(h.env(), track), 4);
        assert_eq!(native_position(h.env(), track), 0); // Queued is not played.
        let timestamps = h.alloc(Java::Longs(vec![77, 88]));
        assert_eq!(native_timestamp(h.env(), track, timestamps), INVALID_OPERATION);
        assert!(matches!(object(timestamps), Java::Longs(v) if v == &[77, 88]));
        let reads = h.state.reads;
        assert_eq!(write_short(h.env(), track, shorts, -1, 4, engine::PCM_16, 0), BAD_VALUE);
        assert_eq!(write_short(h.env(), track, shorts, 31, 4, engine::PCM_16, 0), BAD_VALUE);
        assert_eq!(write_short(h.env(), track, shorts, i32::MAX, i32::MAX, engine::PCM_16, 0), BAD_VALUE);
        assert_eq!(h.state.reads, reads);
        native_pause(h.env(), track); native_flush(h.env(), track);
        assert_eq!(native_written_frames(h.env(), track), 0);
        let bytes = h.alloc(Java::Bytes((0..32).collect()));
        assert_eq!(native_write_byte(h.env(), track, bytes, 4, 24, engine::PCM_16, 0), 16);
        native_flush(h.env(), track);
        let direct = h.alloc(Java::Direct(vec![0; 32]));
        assert_eq!(native_write_native_bytes(h.env(), track, direct, 32, 1, engine::PCM_16, 0), BAD_VALUE);
        assert_eq!(native_write_native_bytes(h.env(), track, bytes, 0, 4, engine::PCM_16, 0), BAD_VALUE);
        assert_eq!(native_write_native_bytes(h.env(), track, direct, 4, 24, engine::PCM_16, 0), 16);
        let route = native_routed_devices(h.env(), track);
        assert!(matches!(object(route), Java::Ints(v) if v.is_empty()));
        assert!(!native_get_params(h.env(), track).is_null());
        let out = h.alloc(Java::Ints(vec![99]));
        assert_eq!(native_get_dual_mono(h.env(), track, out), 0);
        assert!(matches!(object(out), Java::Ints(v) if v == &[0]));
        let description = h.alloc(Java::Floats(vec![99.0]));
        assert_eq!(native_get_description(h.env(), track, description), INVALID_OPERATION);
        assert!(matches!(object(description), Java::Floats(v) if v == &[99.0]));
        let params = native_get_params(h.env(), track);
        native_set_params(h.env(), track, params);
        assert!(h.state.pending); h.clear_exception();
        assert_eq!(native_apply_shaper(h.env(), track, std::ptr::null_mut(), std::ptr::null_mut()), -38);
        assert_eq!(native_flush_from_frame(h.env(), track, 0, 0), INVALID_OPERATION as i64);
        assert!(h.state.pending); h.clear_exception();
        // Re-registration with a live handle keeps the same metadata and closed table.
        assert_eq!(register(h.env()), 0); assert_eq!(h.handle(track), handle);
        native_release(h.env(), track); native_release(h.env(), track);
        assert_eq!(h.handle(track), 0); assert!(engine::get(handle).is_none());
        let foreign = h.alloc(Java::Track { handle: 0x7fff_1234_5678, jni_data: 0, stream: 3 });
        native_release(h.env(), foreign);
        assert!(h.state.pending); assert_eq!(h.handle(foreign), 0x7fff_1234_5678); h.clear_exception();
        let (float_track, rates, sessions, result) = h.setup(0, 0, engine::PCM_FLOAT);
        assert_eq!(result, 0);
        assert!(matches!(object(rates), Java::Ints(v) if v == &[48_000]));
        assert!(matches!(object(sessions), Java::Ints(v) if v[0] > 0));
        let floats = h.alloc(Java::Floats(vec![0.0, 1.0, -1.0, 0.5, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0]));
        assert_eq!(native_write_float(h.env(), float_track, floats, 2, 8, engine::PCM_FLOAT, 0), 8);
        assert_eq!(native_write_short(h.env(), float_track, shorts, 0, 4, engine::PCM_FLOAT, 0), BAD_VALUE);
        let owned = engine::get(h.handle(float_track)).unwrap(); owned.release();
        assert_eq!(native_write_float(h.env(), float_track, floats, 0, 4, engine::PCM_FLOAT, 0), DEAD_OBJECT);
        native_release(h.env(), float_track);
        for (rate, session, encoding) in [(-1, 0, engine::PCM_16), (44_100, -1, engine::PCM_16), (44_100, 0, 3)] {
            let (track, _, _, result) = h.setup(rate, session, encoding);
            assert!(result < 0); assert_eq!(h.handle(track), 0);
        }
        for _ in 0..16 {
            let (track, _, _, result) = h.setup(44_100, 0, engine::PCM_16); assert_eq!(result, 0);
            let handle = h.handle(track); native_release(h.env(), track); assert!(engine::get(handle).is_none());
        }
        assert_eq!(h.state.globals, 4); assert_eq!(h.state.frames, 0); assert!(!h.state.pending);
        READY.store(false, Ordering::Release);
    }
}
