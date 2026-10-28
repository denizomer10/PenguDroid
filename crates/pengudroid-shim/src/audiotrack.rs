//! Android 17 / API 37 AudioTrack, hosted at the JNI boundary.
//!
//! The loader must run the original libandroid_runtime registration groups first,
//! then call `__pengudroid_register_audio_api37` after the last group. Those groups
//! use internalRegisterMethodsOrDie, not the exported AndroidRuntime interposer.
//! Audio registration is deliberately never patched by name in that interposer.
//!
//! The adapter validates the complete AudioTrack native declaration set before
//! installing any pointers. All instance natives are hosted: a Java long holding
//! one of our integer handles must never reach libaudioclient's C++ pointer code.
//! PCM goes to the existing pw-cat/aplay sink through a bounded streaming queue.

use std::ffi::CStr;
use std::os::raw::{c_char, c_void};

mod api37;
mod contract;
mod engine;
mod jni;
#[cfg(test)]
mod runtime_contract;

// BLAST shares these JNI primitives, but not the audio contract/registrar.
pub(crate) use jni::{find_class as jni_find_class, slot as jni_slot, NativeMethod};

/// Install the descriptor-checked API37 audio adapter. Returns zero on success,
/// negative on failure (including an already-pending Java exception). Failures
/// preserve any Java exception for the caller's required framework gate.
///
/// # Safety
/// `env` must be the current thread's live JNIEnv*, after all original
/// libandroid_runtime native registration groups have successfully completed.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_register_audio_api37(env: *mut c_void) -> i32 {
    api37::register(env)
}

/// Preserve the existing BLAST hook. Audio classes always take the original
/// registration path; only the explicit, validated registrar above hosts audio.
#[no_mangle]
pub unsafe extern "C" fn _ZN7android14AndroidRuntime21registerNativeMethodsEP7_JNIEnvPKcPK15JNINativeMethodi(
    env: *mut c_void,
    class_name: *const c_char,
    methods: *const NativeMethod,
    count: i32,
) -> i32 {
    if env.is_null() || class_name.is_null() {
        return -1;
    }
    let name = CStr::from_ptr(class_name).to_string_lossy();
    if !crate::blast::hosts(&name) || methods.is_null() || count <= 0 {
        let Some(real) = crate::dlsym_next::<unsafe extern "C" fn(
            *mut c_void, *const c_char, *const NativeMethod, i32,
        ) -> i32>(b"_ZN7android14AndroidRuntime21registerNativeMethodsEP7_JNIEnvPKcPK15JNINativeMethodi\0")
        else {
            return -1;
        };
        return real(env, class_name, methods, count);
    }
    let original = std::slice::from_raw_parts(methods, count as usize);
    let mut replaced = 0;
    let patched: Vec<NativeMethod> = original.iter().map(|m| {
        let replacement = if m.name.is_null() {
            None
        } else {
            crate::blast::override_for(&name, &CStr::from_ptr(m.name).to_string_lossy())
        };
        if replacement.is_some() { replaced += 1; }
        NativeMethod {
            name: m.name,
            signature: m.signature,
            fn_ptr: replacement.unwrap_or(m.fn_ptr),
        }
    }).collect();
    let class = jni_find_class(env, CStr::from_ptr(class_name));
    if class.is_null() || jni::exception_check(env) { return -1; }
    let rc = jni::register_natives(env, class, &patched);
    jni::delete_local_ref(env, class);
    eprintln!("blast: hosted {replaced} native(s) of {name} (rc={rc})");
    rc
}
