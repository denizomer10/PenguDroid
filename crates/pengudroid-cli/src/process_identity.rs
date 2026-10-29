//! Guest process identity at the API-37 Java/libcore boundary only.
//!
//! The caller must supply the same UID it publishes in ApplicationInfo.uid,
//! before initializing app code. User-0 application UIDs are supported here;
//! the primary guest GID is the app UID, as for an ordinary Android app.
//! This neither changes host credentials nor virtualizes native syscalls. It
//! is compatibility metadata, NOT a sandbox, privilege drop, or attestation.
//!
//! This API-37 core-libart.jar has Java instance get*id() wrappers backed by
//! private static @CriticalNative nativeGet*id()I methods. Their callbacks take
//! NO JNIEnv/class/receiver arguments. Registering instance-JNI callbacks for
//! this artifact would be an ABI error; no alternate layout is attempted.

use std::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Mutex;

use crate::jni::{JClass, JMethodId, JniEnv, JObject, JNI_OK};

type IdentityResult<T> = Result<T, &'static str>;
type CriticalIdentityQuery = unsafe extern "C" fn() -> i32;

const FIRST_APP_UID: i32 = 10_000;
const LAST_APP_UID: i32 = 19_999;
const UNINSTALLED: i32 = -1;
const PROBE_APP_UID: i32 = FIRST_APP_UID;
const ACC_STATIC: i32 = 0x0008;
const ACC_NATIVE: i32 = 0x0100;

static APP_UID: AtomicI32 = AtomicI32::new(UNINSTALLED);
static INSTALL_LOCK: Mutex<()> = Mutex::new(());

const QUERIES: [(&str, &str, Query); 4] = [
    ("getuid", "nativeGetuid", Query::Uid),
    ("geteuid", "nativeGeteuid", Query::Euid),
    ("getgid", "nativeGetgid", Query::Gid),
    ("getegid", "nativeGetegid", Query::Egid),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HostIds {
    uid: libc::uid_t,
    euid: libc::uid_t,
    gid: libc::gid_t,
    egid: libc::gid_t,
}

impl HostIds {
    fn current() -> Self {
        // SAFETY: these are read-only libc queries, not credential setters or
        // interposed guest symbols. No host syscall implementation is changed.
        unsafe {
            Self {
                uid: libc::getuid(),
                euid: libc::geteuid(),
                gid: libc::getgid(),
                egid: libc::getegid(),
            }
        }
    }

    fn is_root(self) -> bool {
        self.uid == 0 || self.euid == 0
    }

    fn query(self, query: Query) -> i32 {
        match query {
            Query::Uid => self.uid as i32,
            Query::Euid => self.euid as i32,
            Query::Gid => self.gid as i32,
            Query::Egid => self.egid as i32,
        }
    }
}

#[derive(Clone, Copy)]
enum Query {
    Uid,
    Euid,
    Gid,
    Egid,
}

fn is_app_uid(uid: i32) -> bool {
    (FIRST_APP_UID..=LAST_APP_UID).contains(&uid)
}

fn validate_install(host: HostIds, requested: i32, installed: i32) -> IdentityResult<()> {
    if host.is_root() {
        return Err("refusing guest identity installation: real or effective host UID is root; run as a non-root user");
    }
    if !is_app_uid(requested) {
        return Err("app_uid must be a user-0 normal application UID in 10000..=19999");
    }
    if installed != UNINSTALLED && installed != requested {
        return Err("cannot change an already installed guest process UID");
    }
    Ok(())
}

fn query_value(host: HostIds, installed: i32, query: Query) -> i32 {
    // Recheck root at every query as well as installation. A later credential
    // change must never conceal root behind an unprivileged guest identity.
    if host.is_root() || !is_app_uid(installed) {
        host.query(query)
    } else {
        installed
    }
}

fn guest_query(query: Query) -> i32 {
    query_value(HostIds::current(), APP_UID.load(Ordering::Acquire), query)
}

// CriticalNative callbacks must remain allocation-free and must not call JNI.
unsafe extern "C" fn native_getuid() -> i32 { guest_query(Query::Uid) }
unsafe extern "C" fn native_geteuid() -> i32 { guest_query(Query::Euid) }
unsafe extern "C" fn native_getgid() -> i32 { guest_query(Query::Gid) }
unsafe extern "C" fn native_getegid() -> i32 { guest_query(Query::Egid) }

#[repr(C)]
struct NativeMethod {
    name: *const c_char,
    signature: *const c_char,
    function: *mut c_void,
}

impl NativeMethod {
    fn new(name: &'static [u8], function: CriticalIdentityQuery) -> Self {
        Self {
            name: name.as_ptr().cast(),
            signature: b"()I\0".as_ptr().cast(),
            function: function as *const () as *mut c_void,
        }
    }
}

/// Install one immutable guest UID/GID for the loaded API-37 libcore profile.
/// A repeated installation of the same UID is allowed; a different UID or any
/// real/effective root UID is rejected. No host credential is modified.
pub fn install(env: &JniEnv, app_uid: i32) -> bool {
    match install_checked(env, app_uid) {
        Ok(()) => {
            println!("process-identity: API-37 Linux nativeGetuid/nativeGeteuid/nativeGetgid/nativeGetegid registered (CriticalNative), uid={app_uid} gid={app_uid}");
            true
        }
        Err(error) => {
            env.describe_exception();
            eprintln!("process-identity: {error}; requested uid={app_uid}, host={:?}", HostIds::current());
            false
        }
    }
}

fn install_checked(env: &JniEnv, app_uid: i32) -> IdentityResult<()> {
    let _installation = INSTALL_LOCK.lock().map_err(|_| "identity installation lock poisoned")?;
    let previous = APP_UID.load(Ordering::Acquire);
    // Reject root before touching Java classes or native registrations.
    validate_install(HostIds::current(), app_uid, previous)?;
    let _frame = env.local_frame(24).ok_or("cannot allocate identity local frame")?;
    require_api37(env)?;
    let linux = env.find_class("libcore/io/Linux").ok_or("API-37 libcore.io.Linux missing")?;
    let method_class = env.find_class("java/lang/reflect/Method").ok_or("reflection Method missing")?;
    let get_modifiers = env.method_id(method_class, "getModifiers", "()I").ok_or("Method.getModifiers missing")?;
    for (wrapper, native, _) in QUERIES {
        let _method_frame = env.local_frame(8).ok_or("cannot allocate identity method frame")?;
        let method = env.method_id(linux, wrapper, "()I").ok_or("API-37 Linux Java identity wrapper missing")?;
        check_modifiers(env, linux, method, false, get_modifiers)?;
        let method = env.static_method_id(linux, native, "()I").ok_or("API-37 Linux native identity backing method missing")?;
        check_modifiers(env, linux, method, true, get_modifiers)?;
    }
    // CriticalNative annotations are build-visible in this DEX, not reflection
    // annotations. The zero-argument ABI was verified in the API-37 artifact;
    // the preflight above also rejects the older instance-native layout.
    let methods = [
        NativeMethod::new(b"nativeGetuid\0", native_getuid),
        NativeMethod::new(b"nativeGeteuid\0", native_geteuid),
        NativeMethod::new(b"nativeGetgid\0", native_getgid),
        NativeMethod::new(b"nativeGetegid\0", native_getegid),
    ];
    validate_install(HostIds::current(), app_uid, previous)?;
    // Publish before any newly registered callback can run. On failure, even a
    // partially registered query becomes transparent again via UNINSTALLED (or
    // keeps the previous identical mapping); do not unregister unrelated Linux
    // natives to try to roll back the entire class.
    APP_UID.store(app_uid, Ordering::Release);
    // SAFETY: JNI slot 215 is RegisterNatives. All four static native ()I
    // methods were preflighted, and the callback pointers have the exact
    // CriticalNative ABI. Names/signatures have static lifetime.
    let register: unsafe extern "C" fn(*mut c_void, JClass, *const NativeMethod, i32) -> i32 =
        unsafe { jni_function(env, 215) };
    let rc = unsafe { register(env.0, linux, methods.as_ptr(), methods.len() as i32) };
    let clean = env.exception_check();
    if rc != JNI_OK || !clean {
        APP_UID.store(previous, Ordering::Release);
        eprintln!("process-identity: RegisterNatives returned {rc}, no_pending_exception={clean}");
        return Err("failed to register all four API-37 identity queries");
    }
    Ok(())
}

fn require_api37(env: &JniEnv) -> IdentityResult<()> {
    let version = env.find_class("android/os/Build$VERSION").ok_or("Build.VERSION missing")?;
    let sdk = env.static_field_id(version, "SDK_INT", "I").ok_or("SDK_INT missing")?;
    let value = env.static_int_field(version, sdk);
    if !env.exception_check() || value != 37 { return Err("process identity requires API 37"); }
    let process = env.find_class("android/os/Process").ok_or("Process missing")?;
    for (name, expected) in [("FIRST_APPLICATION_UID", FIRST_APP_UID), ("LAST_APPLICATION_UID", LAST_APP_UID)] {
        let field = env.static_field_id(process, name, "I").ok_or("API-37 app UID range missing")?;
        let value = env.static_int_field(process, field);
        if !env.exception_check() || value != expected { return Err("unexpected API-37 application UID range"); }
    }
    Ok(())
}

fn check_modifiers(env: &JniEnv, class: JClass, method: JMethodId, native: bool,
    get_modifiers: JMethodId) -> IdentityResult<()> {
    // SAFETY: JNI slot 9 is ToReflectedMethod. The method ID belongs to class;
    // the isStatic argument matches the instance/static lookup used above.
    let reflect: unsafe extern "C" fn(*mut c_void, JClass, JMethodId, u8) -> JObject =
        unsafe { jni_function(env, 9) };
    let reflected = unsafe { reflect(env.0, class, method, u8::from(native)) };
    if !env.exception_check() || reflected.is_null() { return Err("cannot reflect identity method"); }
    let modifiers = env.call_int(reflected, get_modifiers, &[]).ok_or("cannot inspect identity method modifiers")?;
    let expected = if native { ACC_STATIC | ACC_NATIVE } else { 0 };
    if modifiers & (ACC_STATIC | ACC_NATIVE) != expected {
        return Err("identity method does not match the exact API-37 wrapper/static-native layout");
    }
    Ok(())
}

/// Standalone CheckJNI gate: no application or WebView is constructed here.
/// Host IDs are compared even when Java-side validation fails.
pub fn probe(env: &JniEnv) -> bool {
    let before = HostIds::current();
    let result = probe_checked(env);
    let after = HostIds::current();
    if before != after {
        eprintln!("process-identity: host libc identities changed: {before:?} -> {after:?}");
        return false;
    }
    match result {
        Ok(()) => {
            println!("process-identity: host libc getuid/geteuid/getgid/getegid unchanged");
            println!("process-identity: stable guest uid/gid=10000 passed");
            true
        }
        Err(error) => {
            env.describe_exception();
            eprintln!("process-identity: probe failed: {error}");
            false
        }
    }
}

fn probe_checked(env: &JniEnv) -> IdentityResult<()> {
    let _frame = env.local_frame(16).ok_or("cannot allocate identity probe frame")?;
    if !install(env, PROBE_APP_UID) { return Err("identity installation rejected"); }
    let linux_class = env.find_class("libcore/io/Linux").ok_or("probe Linux missing")?;
    let ctor = env.method_id(linux_class, "<init>", "()V").ok_or("Linux constructor missing")?;
    let linux = env.new_object(linux_class, ctor, &[]).filter(|value| !value.is_null()).ok_or("cannot construct Linux")?;
    let os = env.find_class("android/system/Os").ok_or("Os missing")?;
    let process = env.find_class("android/os/Process").ok_or("Process missing")?;
    let my_uid = env.static_method_id(process, "myUid", "()I").ok_or("Process.myUid missing")?;
    for _ in 0..2 {
        let _iteration = env.local_frame(8).ok_or("cannot allocate identity probe iteration frame")?;
        for (name, _, _) in QUERIES {
            let method = env.method_id(linux_class, name, "()I").ok_or("Linux identity wrapper missing")?;
            let value = env.call_int(linux, method, &[]).ok_or("Linux identity query threw")?;
            if value != PROBE_APP_UID {
                eprintln!("process-identity: Linux.{name} returned {value}, expected {PROBE_APP_UID}");
                return Err("Linux identity mismatch");
            }
            let method = env.static_method_id(os, name, "()I").ok_or("Os identity query missing")?;
            let value = env.call_static_int(os, method, &[]).ok_or("Os identity query threw")?;
            if value != PROBE_APP_UID {
                eprintln!("process-identity: Os.{name} returned {value}, expected {PROBE_APP_UID}");
                return Err("Os identity mismatch");
            }
        }
        if env.call_static_int(process, my_uid, &[]) != Some(PROBE_APP_UID)
            || APP_UID.load(Ordering::Acquire) != PROBE_APP_UID {
            return Err("Process.myUid disagrees with the stable configured app UID");
        }
    }
    println!("process-identity: Java Linux/Os uid=10000 euid=10000 gid=10000 egid=10000; Process.myUid=10000");
    Ok(())
}

unsafe fn jni_function<F: Copy>(env: &JniEnv, slot: usize) -> F {
    // SAFETY: callers supply the exact fixed JNI slot and its function type.
    let table = unsafe { *(env.0 as *const *const *const c_void) };
    let pointer = unsafe { *table.add(slot) };
    debug_assert_eq!(std::mem::size_of::<F>(), std::mem::size_of::<*const c_void>());
    unsafe { std::mem::transmute_copy(&pointer) }
}

#[cfg(test)]
mod tests {
    use super::*;

    // All policy tests use synthetic snapshots, never process credential setters
    // or the global installation state; they are independent of the test UID.
    fn ordinary_host() -> HostIds {
        HostIds { uid: 1000, euid: 1001, gid: 2000, egid: 2001 }
    }

    #[test]
    fn accepts_only_normal_user_zero_app_uids() {
        for uid in [10_000, 10_001, 19_999] {
            assert_eq!(validate_install(ordinary_host(), uid, UNINSTALLED), Ok(()));
        }
        for uid in [i32::MIN, -1, 0, 1000, 9999, 20_000, 90_000, 99_999, 110_000, i32::MAX] {
            assert!(validate_install(ordinary_host(), uid, UNINSTALLED).is_err(), "accepted uid {uid}");
        }
    }

    #[test]
    fn rejects_real_or_effective_root_even_after_installation() {
        for (uid, euid) in [(0, 0), (0, 1000), (1000, 0)] {
            let host = HostIds { uid, euid, ..ordinary_host() };
            for installed in [UNINSTALLED, PROBE_APP_UID] {
                assert!(validate_install(host, PROBE_APP_UID, installed).unwrap_err().contains("root"));
            }
        }
    }

    #[test]
    fn installed_guest_uid_is_immutable() {
        assert_eq!(validate_install(ordinary_host(), PROBE_APP_UID, PROBE_APP_UID), Ok(()));
        assert!(validate_install(ordinary_host(), PROBE_APP_UID + 1, PROBE_APP_UID).is_err());
    }

    #[test]
    fn maps_all_four_metadata_queries_to_the_app_identity() {
        let host = ordinary_host();
        for (_, _, query) in QUERIES {
            assert_eq!(query_value(host, PROBE_APP_UID, query), PROBE_APP_UID);
        }
        assert_eq!(host, ordinary_host());
    }

    #[test]
    fn later_root_credentials_are_never_hidden() {
        for (uid, euid) in [(0, 0), (0, 1000), (1000, 0)] {
            let host = HostIds { uid, euid, ..ordinary_host() };
            for (_, _, query) in QUERIES {
                assert_eq!(query_value(host, PROBE_APP_UID, query), host.query(query));
            }
        }
    }

    #[test]
    fn uninstalled_or_invalid_mapping_preserves_host_queries() {
        let host = ordinary_host();
        for installed in [UNINSTALLED, i32::MIN, 0, 1000, 20_000] {
            for (_, _, query) in QUERIES {
                assert_eq!(query_value(host, installed, query), host.query(query));
            }
        }
    }

    #[test]
    fn native_method_record_has_jni_layout() {
        assert_eq!(std::mem::size_of::<NativeMethod>(), 3 * std::mem::size_of::<*const c_void>());
        assert_eq!(std::mem::align_of::<NativeMethod>(), std::mem::align_of::<*const c_void>());
    }
}
