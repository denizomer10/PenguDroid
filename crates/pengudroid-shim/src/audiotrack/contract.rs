//! Closed API37 declaration validation, independent of registration side effects.
use super::jni::{self, Env, Id, NativeMethod, Object};
use std::ffi::CStr;
use std::os::raw::c_void;

pub(super) const SETUP_DESCRIPTOR: &CStr = c"(Ljava/lang/Object;Ljava/lang/Object;[ILjava/lang/Object;III[ILandroid/os/Parcel;JZILjava/lang/Object;Ljava/lang/String;Ljava/lang/String;)I";
pub(super) const STATIC: i32 = 0x0008;
pub(super) const NATIVE: i32 = 0x0100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Abi { Void, Int, Long, Float, Boolean, Reference }
pub(super) trait JniType { const ABI: Abi; }
impl JniType for () { const ABI: Abi = Abi::Void; }
impl JniType for i32 { const ABI: Abi = Abi::Int; }
impl JniType for i64 { const ABI: Abi = Abi::Long; }
impl JniType for f32 { const ABI: Abi = Abi::Float; }
impl JniType for u8 { const ABI: Abi = Abi::Boolean; }
impl JniType for *mut c_void { const ABI: Abi = Abi::Reference; }

pub(super) struct Spec {
    pub(super) name: &'static CStr,
    pub(super) descriptor: &'static CStr,
    pub(super) is_static: bool,
    pub(super) pointer: *mut c_void,
    pub(super) arguments: &'static [Abi],
    pub(super) result: Abi,
}
impl Spec {
    pub(super) fn native(&self) -> NativeMethod {
        NativeMethod { name: self.name.as_ptr(), signature: self.descriptor.as_ptr(), fn_ptr: self.pointer }
    }
}

#[derive(Debug, Clone)]
pub(super) struct Declaration {
    pub(super) name: String,
    pub(super) descriptor: String,
    pub(super) modifiers: i32,
}

fn descriptor_abi(descriptor: &str) -> Result<(Vec<Abi>, Abi), String> {
    fn ty(bytes: &[u8], index: &mut usize, allow_void: bool) -> Result<Abi, String> {
        let Some(&first) = bytes.get(*index) else { return Err("truncated descriptor".into()); };
        *index += 1;
        Ok(match first {
            b'V' if allow_void => Abi::Void,
            b'I' => Abi::Int,
            b'J' => Abi::Long,
            b'F' => Abi::Float,
            b'Z' => Abi::Boolean,
            b'L' => {
                let start = *index;
                while bytes.get(*index).is_some_and(|b| *b != b';') { *index += 1; }
                if *index == start || bytes.get(*index) != Some(&b';') {
                    return Err("invalid object descriptor".into());
                }
                *index += 1;
                Abi::Reference
            }
            b'[' => {
                // Primitive arrays are references, including byte/short/double arrays.
                while bytes.get(*index) == Some(&b'[') { *index += 1; }
                match bytes.get(*index) {
                    Some(b'B' | b'C' | b'S' | b'D') => { *index += 1; }
                    _ => { ty(bytes, index, false)?; }
                }
                Abi::Reference
            }
            _ => return Err(format!("unsupported JNI descriptor type {}", first as char)),
        })
    }
    let bytes = descriptor.as_bytes();
    if bytes.first() != Some(&b'(') { return Err("missing descriptor '('".into()); }
    let mut index = 1;
    let mut args = Vec::new();
    while bytes.get(index) != Some(&b')') { args.push(ty(bytes, &mut index, false)?); }
    index += 1;
    let result = ty(bytes, &mut index, true)?;
    if index != bytes.len() { return Err("trailing descriptor data".into()); }
    Ok((args, result))
}

pub(super) fn validate(specs: &[Spec], declarations: &[Declaration], closed: bool) -> Result<(), String> {
    let mut names = std::collections::HashSet::new();
    for spec in specs {
        let name = spec.name.to_str().map_err(|_| "non-ASCII contract name")?;
        let descriptor = spec.descriptor.to_str().map_err(|_| "non-ASCII contract descriptor")?;
        if !names.insert(name) { return Err(format!("duplicate contract method {name}")); }
        let (args, result) = descriptor_abi(descriptor)?;
        if args != spec.arguments || result != spec.result || spec.pointer.is_null() {
            return Err(format!("typed wrapper ABI disagrees with {name}{descriptor}"));
        }
        let candidates: Vec<_> = declarations.iter().filter(|d| d.name == name).collect();
        if candidates.len() != 1 { return Err(format!("missing/overloaded native {name}{descriptor}")); }
        let declaration = candidates[0];
        if declaration.descriptor != descriptor
            || declaration.modifiers & NATIVE == 0
            || (declaration.modifiers & STATIC != 0) != spec.is_static
        {
            return Err(format!("declaration mismatch for {name}: {} flags={:#x}; expected {descriptor} static={}",
                declaration.descriptor, declaration.modifiers, spec.is_static));
        }
    }
    if closed {
        for declaration in declarations.iter().filter(|d| d.modifiers & NATIVE != 0) {
            if !names.contains(declaration.name.as_str()) {
                return Err(format!("unknown AudioTrack native {}{}", declaration.name, declaration.descriptor));
            }
        }
    }
    Ok(())
}

pub(super) unsafe fn required_class(env: Env, name: &CStr) -> Result<Object, ()> {
    let class = jni::find_class(env, name);
    if class.is_null() || jni::exception_check(env) { Err(()) } else { Ok(class) }
}
pub(super) unsafe fn required_method(env: Env, class: Object, name: &CStr, sig: &CStr) -> Result<Id, ()> {
    let method = jni::method(env, class, name, sig);
    if method.is_null() || jni::exception_check(env) { Err(()) } else { Ok(method) }
}

/// Reflect declarations, not inherited GetMethodID matches. An unknown instance
/// native is a safety failure, even if the methods we expected still exist.
pub(super) unsafe fn declarations(env: Env, class: Object) -> Result<Vec<Declaration>, ()> {
    let _frame = jni::LocalFrame::new(env, 32)?;
    let class_class = required_class(env, c"java/lang/Class")?;
    let method_class = required_class(env, c"java/lang/reflect/Method")?;
    let declared = required_method(env, class_class, c"getDeclaredMethods", c"()[Ljava/lang/reflect/Method;")?;
    let class_name = required_method(env, class_class, c"getName", c"()Ljava/lang/String;")?;
    let name = required_method(env, method_class, c"getName", c"()Ljava/lang/String;")?;
    let modifiers = required_method(env, method_class, c"getModifiers", c"()I")?;
    let parameters = required_method(env, method_class, c"getParameterTypes", c"()[Ljava/lang/Class;")?;
    let return_type = required_method(env, method_class, c"getReturnType", c"()Ljava/lang/Class;")?;
    let methods = jni::call_object(env, class, declared, &[]);
    if methods.is_null() || jni::exception_check(env) { return Err(()); }
    let count = jni::array_length(env, methods);
    if jni::exception_check(env) || !(0..=4096).contains(&count) { return Err(()); }
    let mut result = Vec::with_capacity(count as usize);
    for index in 0..count {
        let _method_frame = jni::LocalFrame::new(env, 32)?;
        let method = jni::object_element(env, methods, index);
        if method.is_null() || jni::exception_check(env) { return Err(()); }
        let method_name = jni::call_object(env, method, name, &[]);
        if jni::exception_check(env) { return Err(()); }
        let method_name = jni::string(env, method_name)?;
        let flags = jni::call_int(env, method, modifiers, &[]);
        if jni::exception_check(env) { return Err(()); }
        let params = jni::call_object(env, method, parameters, &[]);
        if params.is_null() || jni::exception_check(env) { return Err(()); }
        let params_count = jni::array_length(env, params);
        if jni::exception_check(env) || !(0..=64).contains(&params_count) { return Err(()); }
        let mut descriptor = String::from("(");
        for p in 0..params_count {
            let param = jni::object_element(env, params, p);
            if param.is_null() || jni::exception_check(env) { return Err(()); }
            descriptor.push_str(&type_descriptor(env, param, class_name)?);
            jni::delete_local_ref(env, param);
        }
        descriptor.push(')');
        let ret = jni::call_object(env, method, return_type, &[]);
        if ret.is_null() || jni::exception_check(env) { return Err(()); }
        descriptor.push_str(&type_descriptor(env, ret, class_name)?);
        result.push(Declaration { name: method_name, descriptor, modifiers: flags });
    }
    Ok(result)
}

unsafe fn type_descriptor(env: Env, class: Object, name: Id) -> Result<String, ()> {
    let string = jni::call_object(env, class, name, &[]);
    if jni::exception_check(env) { return Err(()); }
    let name = jni::string(env, string)?;
    jni::delete_local_ref(env, string);
    Ok(match name.as_str() {
        "void" => "V".into(), "boolean" => "Z".into(), "byte" => "B".into(),
        "char" => "C".into(), "short" => "S".into(), "int" => "I".into(),
        "long" => "J".into(), "float" => "F".into(), "double" => "D".into(),
        name if name.starts_with('[') => name.replace('.', "/"),
        _ => format!("L{};", name.replace('.', "/")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::api37::{system_specs, track_specs};

    fn declared(specs: &[Spec]) -> Vec<Declaration> {
        specs.iter().map(|s| Declaration {
            name: s.name.to_str().unwrap().into(), descriptor: s.descriptor.to_str().unwrap().into(),
            modifiers: NATIVE | if s.is_static { STATIC } else { 0 },
        }).collect()
    }
    #[test]
    fn api37_closed_table_has_57_exact_typed_entries() {
        let specs = track_specs();
        assert_eq!(specs.len(), 57);
        assert_eq!(specs.iter().filter(|s| s.is_static).count(), 4);
        validate(&specs, &declared(&specs), true).unwrap();
        let sys = system_specs();
        assert_eq!(sys.len(), 2);
        validate(&sys, &declared(&sys), false).unwrap();
        let actual = super::super::runtime_contract::track_declarations();
        assert_eq!(actual.len(), 57);
        validate(&specs, &actual, true).unwrap();
    }
    #[test]
    fn rejects_stale_setup_even_if_the_name_matches() {
        let specs = track_specs();
        let mut declarations = declared(&specs);
        let setup = declarations.iter_mut().find(|d| d.name == "native_setup").unwrap();
        assert_eq!(setup.descriptor, SETUP_DESCRIPTOR.to_str().unwrap());
        let original = setup.descriptor.clone();
        for stale in [
            original.replace("[ILjava/lang/Object;III", "[IIIII"),
            original.replace("Landroid/os/Parcel;", ""),
            original.replacen("Ljava/lang/String;Ljava/lang/String;", "Ljava/lang/String;", 1),
            original.replace("[ILjava/lang/Object;", "[ILandroid/media/AudioFormat$ChannelMasks;"),
        ] {
            let mut bad = declared(&specs);
            bad.iter_mut().find(|d| d.name == "native_setup").unwrap().descriptor = stale;
            assert!(validate(&specs, &bad, true).is_err());
        }
    }
    #[test]
    fn rejects_unknown_missing_non_native_static_and_overloaded_methods() {
        let specs = track_specs();
        for change in 0..5 {
            let mut bad = declared(&specs);
            match change {
                0 => bad.push(Declaration { name: "native_future_pointer_user".into(), descriptor: "()V".into(), modifiers: NATIVE }),
                1 => { bad.pop(); },
                2 => bad[0].modifiers &= !NATIVE,
                3 => bad[0].modifiers ^= STATIC,
                _ => bad.push(bad[0].clone()),
            }
            assert!(validate(&specs, &bad, true).is_err());
        }
    }
    #[test]
    fn validates_long_boolean_float_reference_and_renamed_methods() {
        let specs = track_specs();
        for (name, result) in [("native_get_written_frames_count", Abi::Long), ("native_flushFromFrame", Abi::Long),
            ("native_setOutputDevice", Abi::Boolean), ("native_getRoutedDeviceIds", Abi::Reference)] {
            assert_eq!(specs.iter().find(|s| s.name.to_str().unwrap() == name).unwrap().result, result);
        }
        assert!(specs.iter().any(|s| s.name == c"native_set_playback_params"));
        assert!(!specs.iter().any(|s| s.name == c"native_setPlaybackParams"));
        let volume = specs.iter().find(|s| s.name == c"native_setVolume").unwrap();
        assert_eq!(volume.arguments, [Abi::Float, Abi::Float]);
        let mut wrong = track_specs();
        wrong[0].result = Abi::Int;
        assert!(validate(&wrong, &declared(&wrong), true).is_err());
    }
}
