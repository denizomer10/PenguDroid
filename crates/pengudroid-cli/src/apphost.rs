//! Bind an application into a running `ActivityThread`.
//!
//! On a device `system_server` answers `IActivityManager.attachApplication` by
//! calling back into the app's `ApplicationThread.bindApplication(...)` with an
//! `AppBindData` parcel. Faking that parcel is fragile: it is a 20-field AIDL
//! call whose layout changes between releases and which starts with an
//! interface-token check.
//!
//! JNI can do the same thing without any parcel. `GetMethodID` and `GetFieldID`
//! ignore Java access modifiers, so a native caller can:
//!
//! 1. `ActivityThread.currentActivityThread()` — the instance `main()` created;
//! 2. `new ActivityThread$AppBindData()` and set its fields directly;
//! 3. call the *private* `handleBindApplication(AppBindData)` on that instance.
//!
//! That is what the binder callback does, minus the marshalling.

use std::ffi::c_void;
use std::path::Path;

use crate::jni::{JClass, JMethodId, JObject, JValue, JniEnv};

/// Everything `handleBindApplication` needs to know about the app.
pub struct AppSpec<'a> {
    pub package: &'a str,
    pub apk: &'a Path,
    pub data_dir: &'a Path,
    pub native_lib_dir: &'a Path,
    pub target_sdk: i32,
    pub min_sdk: i32,
    /// Android uid for the app. There is no package manager here, so a fixed
    /// value is used; it only has to be stable and non-zero.
    pub uid: i32,
}

fn new_instance(env: &JniEnv, class_name: &str) -> Option<(JClass, JObject)> {
    let class = env.find_class(class_name)?;
    let ctor = env.method_id(class, "<init>", "()V")?;
    let obj = env.new_object(class, ctor, &[])?;
    Some((class, obj))
}

/// Build an `ApplicationInfo` describing the APK we are about to run.
fn build_app_info(env: &JniEnv, spec: &AppSpec<'_>) -> Option<JObject> {
    let (class, info) = new_instance(env, "android/content/pm/ApplicationInfo")?;

    let set_str = |field: &str, value: &str| {
        if let Some(id) = env.field_id(class, field, "Ljava/lang/String;") {
            let s = env.new_string(value);
            env.set_object_field(info, id, s);
        }
    };
    let set_int = |field: &str, value: i32| {
        if let Some(id) = env.field_id(class, field, "I") {
            env.set_int_field(info, id, value);
        }
    };

    set_str("packageName", spec.package);
    set_str("sourceDir", &spec.apk.display().to_string());
    set_str("publicSourceDir", &spec.apk.display().to_string());
    set_str("dataDir", &spec.data_dir.display().to_string());
    set_str("nativeLibraryDir", &spec.native_lib_dir.display().to_string());
    set_str("primaryCpuAbi", "x86_64");
    set_int("targetSdkVersion", spec.target_sdk);
    set_int("minSdkVersion", spec.min_sdk);
    set_int("uid", spec.uid);
    Some(info)
}

fn new_configuration(env: &JniEnv) -> Option<JObject> {
    let (class, config) = new_instance(env, "android/content/res/Configuration")?;
    if let Some(m) = env.method_id(class, "setToDefaults", "()V") {
        env.call_void(config, m, &[]);
    }
    // `LoadApk`/`LocaleList.setDefault` reject an empty locale list, and
    // `setToDefaults()` leaves it empty.
    if let Some(loc) = locale_list(env) {
        if let Some(m) = env.method_id(class, "setLocales", "(Landroid/os/LocaleList;)V") {
            env.call_void(config, m, &[JValue { l: loc }]);
        }
    }
    Some(config)
}

/// `new LocaleList(new Locale("en", "US"))`
fn locale_list(env: &JniEnv) -> Option<JObject> {
    let (locale_class, locale) = new_instance(env, "java/util/Locale")?;
    let ctor = env.method_id(
        locale_class,
        "<init>",
        "(Ljava/lang/String;Ljava/lang/String;)V",
    )?;
    let en = env.new_string("en");
    let us = env.new_string("US");
    let locale = env.new_object(locale_class, ctor, &[JValue { l: en }, JValue { l: us }])?;
    let _ = locale;
    let list_class = env.find_class("android/os/LocaleList")?;
    let list_ctor = env.method_id(list_class, "<init>", "([Ljava/util/Locale;)V")?;
    let arr_class = env.find_class("java/util/Locale")?;
    let array = env.new_object_array(1, arr_class, std::ptr::null_mut())?;
    env.set_object_array_element(array, 0, locale);
    env.new_object(list_class, list_ctor, &[JValue { l: array }])
}

/// Give `Typeface` a system font map.
///
/// `Typeface.loadPreinstalledSystemFontMap()` builds the map from
/// `/system/etc/fonts.xml` through the native `FontConfig`. That path yields an
/// empty map here, so `setSystemFontMap` dereferences
/// `sSystemFontMap.get("sans-serif")` == null. Seeding the one family the
/// framework demands is enough to get through `handleBindApplication`; the real
/// fix is making the native font config parse.
pub fn seed_font_map(env: &JniEnv) -> bool {
    let Some(tf_class) = env.find_class("android/graphics/Typeface") else {
        eprintln!("apphost: Typeface class not found for the font map seed");
        return false;
    };
    // `createFromFile` was removed in this release; `Typeface.Builder` replaces
    // it. `Builder(File)` + `build()` is the public way to load a font file.
    let Some((file_class, file)) = new_instance(env, "java/io/File") else {
        return false;
    };
    let Some(file_ctor) = env.method_id(file_class, "<init>", "(Ljava/lang/String;)V") else {
        return false;
    };
    let path = env.new_string("/system/fonts/Roboto-Regular.ttf");
    let Some(file) = env.new_object(file_class, file_ctor, &[JValue { l: path }]) else {
        return false;
    };
    let _ = file;
    let Some(builder_class) = env.find_class("android/graphics/Typeface$Builder") else {
        return false;
    };
    let Some(ctor) = env.method_id(builder_class, "<init>", "(Ljava/io/File;)V") else {
        eprintln!("apphost: Typeface$Builder(File) missing");
        return false;
    };
    let Some(builder) = env.new_object(builder_class, ctor, &[JValue { l: file }]) else {
        eprintln!("apphost: Typeface$Builder threw");
        return false;
    };
    let Some(build) = env.method_id(builder_class, "build", "()Landroid/graphics/Typeface;") else {
        return false;
    };
    let Some(typeface) = env.call_object(builder, build, &[]) else {
        eprintln!("apphost: Typeface$Builder.build threw");
        return false;
    };
    if typeface.is_null() {
        eprintln!("apphost: Typeface$Builder.build returned null");
        return false;
    }
    let Some((map_class, map)) = new_instance(env, "java/util/HashMap") else {
        return false;
    };
    let Some(put) = env.method_id(
        map_class,
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ) else {
        return false;
    };
    for family in ["sans-serif", "sans-serif-medium", "sans-serif-bold"] {
        let key = env.new_string(family);
        env.call_object(map, put, &[JValue { l: key }, JValue { l: typeface }]);
    }
    let Some(set_map) = env.static_method_id(
        tf_class,
        "setSystemFontMap",
        "(Ljava/util/Map;)V",
    ) else {
        eprintln!("apphost: Typeface.setSystemFontMap missing");
        return false;
    };
    let ok = env.call_static_void(tf_class, set_map, &[JValue { l: map }]);
    if ok {
        println!("apphost: seeded the system font map");
    } else {
        eprintln!("apphost: Typeface.setSystemFontMap threw");
    }
    ok
}

/// Print how many families `Typeface`'s system font map holds.
///
/// `loadPreinstalledSystemFontMap()` is normally called from
/// `ActivityThread.handleBindApplication`, so the diagnostic triggers it too -
/// otherwise the map is simply still empty.
pub fn font_map_size(env: &JniEnv) {
    let Some(tf) = env.find_class("android/graphics/Typeface") else {
        eprintln!("--fontmap: no Typeface");
        return;
    };
    if let Some(load) = env.static_method_id(tf, "loadPreinstalledSystemFontMap", "()V") {
        if !env.call_static_void(tf, load, &[]) {
            eprintln!("--fontmap: loadPreinstalledSystemFontMap threw");
        }
    } else {
        eprintln!("--fontmap: no loadPreinstalledSystemFontMap");
    }
    let Some(get) = env.static_method_id(tf, "getSystemFontMap", "()Ljava/util/Map;") else {
        eprintln!("--fontmap: no Typeface.getSystemFontMap");
        return;
    };
    let Some(map) = env.call_static_object(tf, get, &[]) else {
        eprintln!("--fontmap: getSystemFontMap threw");
        return;
    };
    let Some(map_class) = env.find_class("java/util/Map") else { return };
    let Some(size) = env.method_id(map_class, "size", "()I") else { return };
    let n = env.call_static_object(map, size, &[]);
    let _ = n;
    let f: unsafe extern "C" fn(*mut c_void, JObject, JMethodId, *const JValue) -> i32 =
        unsafe { self::call_int_method(env) };
    let count = unsafe { f(env.0, map, size, std::ptr::null()) };
    println!("--fontmap: {count} families");
}

fn call_int_method(env: &JniEnv) -> unsafe extern "C" fn(*mut c_void, JObject, JMethodId, *const JValue) -> i32 {
    // JNINativeInterface slot 49 = CallIntMethodA.
    unsafe {
        let table = *(env.0 as *const *const *const c_void);
        std::mem::transmute(*table.add(49))
    }
}

/// Bind the app into `thread`, the object returned by
/// `ActivityThread.currentActivityThread()`.
pub fn bind_application(env: &JniEnv, thread: JObject, spec: &AppSpec<'_>) -> bool {
    let Some((bind_class, data)) = new_instance(env, "android/app/ActivityThread$AppBindData") else {
        eprintln!("apphost: cannot allocate AppBindData");
        return false;
    };

    let Some(app_info) = build_app_info(env, spec) else {
        eprintln!("apphost: cannot build ApplicationInfo");
        return false;
    };
    seed_font_map(env);
    let Some(config) = new_configuration(env) else {
        eprintln!("apphost: cannot build Configuration");
        return false;
    };
    let Some((_, providers)) = new_instance(env, "java/util/ArrayList") else {
        eprintln!("apphost: cannot build provider list");
        return false;
    };
    // `LoadedApk` needs a CompatibilityInfo. There is no public constant or
    // constructor to build one in this release, so borrow the system's:
    // `ActivityThread.currentActivityThread().getSystemContext()` exposes the
    // resources-backed instance via `Resources`.
    let compat = system_compat_info(env);

    let process_name = env.new_string(spec.package);

    let mut set_obj = |field: &str, sig: &str, value: JObject| match env.field_id(bind_class, field, sig) {
        Some(id) => env.set_object_field(data, id, value),
        None => eprintln!("apphost: AppBindData has no field {field}"),
    };
    set_obj("processName", "Ljava/lang/String;", process_name);
    set_obj("appInfo", "Landroid/content/pm/ApplicationInfo;", app_info);
    set_obj("config", "Landroid/content/res/Configuration;", config);
    set_obj("providers", "Ljava/util/List;", providers);
    if let Some(compat) = compat {
        set_obj(
            "compatInfo",
            "Landroid/content/res/CompatibilityInfo;",
            compat,
        );
    } else {
        eprintln!("apphost: no CompatibilityInfo available (compatInfo stays null)");
    }

    let mut set_bool = |field: &str, value: bool| {
        if let Some(id) = env.field_id(bind_class, field, "Z") {
            env.set_boolean_field(data, id, value);
        }
    };
    set_bool("restrictedBackupMode", false);
    set_bool("persistent", false);
    set_bool("trackAllocation", false);
    set_bool("enableBinderTracking", false);

    if let Some(id) = env.field_id(bind_class, "debugMode", "I") {
        env.set_int_field(data, id, 0);
    }
    // `handleBindApplication` dereferences these arrays without a null check.
    if let Some(id) = env.field_id(bind_class, "disabledCompatChanges", "[J") {
        if let Some(arr) = env.new_long_array(0) {
            env.set_object_field(data, id, arr);
        }
    }

    let Some(thread_class) = env.find_class("android/app/ActivityThread") else {
        return false;
    };
    let Some(handle) = env.method_id(
        thread_class,
        "handleBindApplication",
        "(Landroid/app/ActivityThread$AppBindData;)V",
    ) else {
        eprintln!("apphost: ActivityThread.handleBindApplication not found");
        return false;
    };
    println!("apphost: calling handleBindApplication for {}", spec.package);
    let ok = env.call_void(thread, handle, &[JValue { l: data }]);
    if ok {
        println!("apphost: handleBindApplication returned normally");
    } else {
        eprintln!("apphost: handleBindApplication threw");
    }
    ok
}

/// Find a `CompatibilityInfo` instance the framework already made.
///
/// `Resources` keeps one (`ResourcesImpl.mCompatibilityInfo`); searching the
/// system context is the least invasive way to get a real object without
/// constructing one.
fn system_compat_info(env: &JniEnv) -> Option<JObject> {
    // `ActivityThread.getSystemContext()` -> ContextImpl -> getResources()
    let thread_class = env.find_class("android/app/ActivityThread")?;
    let current = env.static_method_id(
        thread_class,
        "currentActivityThread",
        "()Landroid/app/ActivityThread;",
    )?;
    let thread = env.call_static_object(thread_class, current, &[])?;
    if thread.is_null() {
        return None;
    }
    let get_system_context = env.method_id(
        thread_class,
        "getSystemContext",
        "()Landroid/app/ContextImpl;",
    )?;
    let ctx = env.call_object(thread, get_system_context, &[])?;
    if ctx.is_null() {
        return None;
    }
    let ctx_class = env.find_class("android/app/ContextImpl")?;
    let get_resources = env.method_id(ctx_class, "getResources", "()Landroid/content/res/Resources;")?;
    let res = env.call_object(ctx, get_resources, &[])?;
    if res.is_null() {
        return None;
    }
    // Required is `Resources.getCompatibilityInfo()` on some releases; try it,
    // then fall back to a static field on CompatibilityInfo.
    let res_class = env.find_class("android/content/res/Resources")?;
    if let Some(m) = env.method_id(res_class, "getCompatibilityInfo", "()Landroid/content/res/CompatibilityInfo;") {
        let ci = env.call_object(res, m, &[]);
        if let Some(ci) = ci {
            if !ci.is_null() {
                eprintln!("apphost: borrowed CompatibilityInfo from Resources");
                return Some(ci);
            }
        }
    }
    let ci_class = env.find_class("android/content/res/CompatibilityInfo")?;
    for field in ["DEFAULT_COMPATIBILITY_INFO", "sDefaultCompatibilityInfo"] {
        if let Some(id) = env.field_id(ci_class, field, "Landroid/content/res/CompatibilityInfo;") {
            let v = env.static_object_field(ci_class, id);
            if !v.is_null() {
                eprintln!("apphost: using CompatibilityInfo.{field}");
                return Some(v);
            }
        }
    }
    None
}

/// Print what *Java* sees for a few system properties.
///
/// `Build.<clinit>` splits property values on `,`; Java drops trailing empty
/// strings, so `"".split(",")` is a zero-length array and indexing `[0]` throws.
pub fn print_java_props(env: &JniEnv) {
    let Some(class) = env.find_class("android/os/SystemProperties") else {
        eprintln!("--java-props: no SystemProperties");
        return;
    };
    let Some(get) = env.static_method_id(
        class,
        "get",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
    ) else {
        eprintln!("--java-props: no SystemProperties.get");
        return;
    };
    let empty = env.new_string("");
    for name in [
        "ro.product.cpu.abilist",
        "ro.product.cpu.abilist32",
        "ro.product.cpu.abilist64",
        "ro.build.version.sdk",
        "ro.build.id",
    ] {
        let key = env.new_string(name);
        let v = env.call_static_object(class, get, &[JValue { l: key }, JValue { l: empty }]);
        match v {
            Some(x) if !x.is_null() => {
                let text = env.get_string(x).unwrap_or_default();
                println!("  {name} = {text:?}");
            }
            other => println!("  {name} = <call failed: {:?}> (exception above, if any)", other.is_some()),
        }
    }
}

/// `--dump-class <name>`: print a class's declared fields and methods.
///
/// ART reports `NoSuchFieldError` for names that exist in another release's
/// source; the authoritative answer is what this framework.jar actually has.
pub fn dump_class(env: &JniEnv, name: &str) {
    let Some(class) = env.load_class(&name.replace('.', "/")) else {
        eprintln!("--dump-class: {name} not found");
        return;
    };
    let Some(class_cls) = env.find_class("java/lang/Class") else {
        return;
    };
    let (Some(get_name), Some(get_fields), Some(get_methods)) = (
        env.method_id(class_cls, "getName", "()Ljava/lang/String;"),
        env.method_id(class_cls, "getDeclaredFields", "()[Ljava/lang/reflect/Field;"),
        env.method_id(class_cls, "getDeclaredMethods", "()[Ljava/lang/reflect/Method;"),
    ) else {
        return;
    };
    let (Some(field_cls), Some(method_cls)) = (
        env.find_class("java/lang/reflect/Field"),
        env.find_class("java/lang/reflect/Method"),
    ) else {
        return;
    };
    let (Some(f_name), Some(f_type)) = (
        env.method_id(field_cls, "getName", "()Ljava/lang/String;"),
        env.method_id(field_cls, "getType", "()Ljava/lang/Class;"),
    ) else {
        return;
    };
    let Some(m_name) = env.method_id(method_cls, "getName", "()Ljava/lang/String;") else {
        return;
    };

    println!("--- fields of {name}");
    if let Some(fields) = env.call_object(class, get_fields, &[]) {
        for i in 0..env.array_length(fields) {
            let f = env.array_get(fields, i);
            if f.is_null() {
                continue;
            }
            let n = env.call_object(f, f_name, &[]).and_then(|s| env.get_string(s));
            let ty = env
                .call_object(f, f_type, &[])
                .and_then(|c| env.call_object(c, get_name, &[]))
                .and_then(|s| env.get_string(s));
            println!("  {} {}", ty.unwrap_or_default(), n.unwrap_or_default());
        }
    }

    println!("--- methods of {name}");
    if let Some(methods) = env.call_object(class, get_methods, &[]) {
        for i in 0..env.array_length(methods) {
            let m = env.array_get(methods, i);
            if m.is_null() {
                continue;
            }
            if let Some(n) = env.call_object(m, m_name, &[]).and_then(|s| env.get_string(s)) {
                println!("  {n}");
            }
        }
    }
}

/// Resolve `ActivityThread.currentActivityThread()` in a VM that already ran
/// `ActivityThread.main` on another thread.
pub fn current_activity_thread(env: &JniEnv) -> Option<JObject> {
    let class: JClass = env.find_class("android/app/ActivityThread")?;
    let m: JMethodId = env.static_method_id(
        class,
        "currentActivityThread",
        "()Landroid/app/ActivityThread;",
    )?;
    let value = env.call_static_object(class, m, &[])?;
    (!value.is_null()).then_some(value)
}

/// Attach the current (host) thread to `vm`, returning its `JNIEnv*`.
pub fn attach_current_thread(vm: *mut c_void) -> Option<JniEnv> {
    if vm.is_null() {
        return None;
    }
    // JavaVM function table: DestroyJavaVM(3), AttachCurrentThread(4).
    let table = unsafe { *(vm as *const *const *const c_void) };
    if table.is_null() {
        return None;
    }
    let attach = unsafe { *table.add(4) };
    if attach.is_null() {
        return None;
    }
    type Attach = unsafe extern "C" fn(*mut c_void, *mut *mut c_void, *mut c_void) -> i32;
    let f: Attach = unsafe { std::mem::transmute(attach) };
    let mut env: *mut c_void = std::ptr::null_mut();
    let rc = unsafe { f(vm, &mut env, std::ptr::null_mut()) };
    if rc != 0 || env.is_null() {
        None
    } else {
        Some(JniEnv(env))
    }
}
