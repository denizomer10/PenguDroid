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

use crate::jni::{JClass, JFieldId, JMethodId, JObject, JValue, JniEnv};

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
        match env.field_id(class, field, "Ljava/lang/String;") {
            Some(id) => {
                let s = env.new_string(value);
                env.set_object_field(info, id, s);
            }
            None => eprintln!("apphost: ApplicationInfo.{field} not found"),
        }
    };
    let set_int = |field: &str, value: i32| {
        match env.field_id(class, field, "I") {
            Some(id) => env.set_int_field(info, id, value),
            None => eprintln!("apphost: ApplicationInfo.{field} not found"),
        }
    };

    set_str("packageName", spec.package);
    set_str("sourceDir", &spec.apk.display().to_string());
    set_str("publicSourceDir", &spec.apk.display().to_string());
    set_str("dataDir", &spec.data_dir.display().to_string());
    // `ContextImpl.createDeviceProtectedStorageContext()` resolves its data
    // directory through `LoadedApk.getDeviceProtectedDataDirFile()`, which is
    // built from these two fields. Leaving them null makes
    // `setupGraphicsSupport` throw "No data directory found for package".
    set_str(
        "deviceProtectedDataDir",
        &spec.data_dir.join("device_protected").display().to_string(),
    );
    set_str(
        "credentialProtectedDataDir",
        &spec.data_dir.display().to_string(),
    );
    set_str("nativeLibraryDir", &spec.native_lib_dir.display().to_string());
    set_str("primaryCpuAbi", "x86_64");
    set_int("targetSdkVersion", spec.target_sdk);
    set_int("minSdkVersion", spec.min_sdk);
    set_int("uid", spec.uid);
    // Read back the fields `LoadedApk` depends on: a silently failed write here
    // shows up much later as "No data directory found for package".
    if let Some(id) = env.field_id(class, "dataDir", "Ljava/lang/String;") {
        let back = env.object_field(info, id);
        match env.get_string(back) {
            Some(s) => println!("apphost: ApplicationInfo.dataDir = {s}"),
            None => eprintln!("apphost: ApplicationInfo.dataDir is null after the write"),
        }
    }
    for field in ["deviceProtectedDataDir", "credentialProtectedDataDir", "sourceDir"] {
        if let Some(id) = env.field_id(class, field, "Ljava/lang/String;") {
            let back = env.object_field(info, id);
            match env.get_string(back) {
                Some(s) => println!("apphost: ApplicationInfo.{field} = {s}"),
                None => eprintln!("apphost: ApplicationInfo.{field} is null after the write"),
            }
        }
    }
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
    // JNINativeInterface slot 51 = CallIntMethodA (49 is the varargs variant).
    unsafe {
        let table = *(env.0 as *const *const *const c_void);
        std::mem::transmute(*table.add(51))
    }
}

/// `list.size()` through the `java.util.List` interface.
fn list_size(env: &JniEnv, list: JObject) -> Option<i32> {
    let class = env.find_class("java/util/List")?;
    let method = env.method_id(class, "size", "()I")?;
    let f = unsafe { call_int_method(env) };
    let value = unsafe { f(env.0, list, method, std::ptr::null()) };
    env.exception_check().then_some(value)
}

/// `map.size()` through the `java.util.Map` interface.
fn map_size(env: &JniEnv, map: JObject) -> Option<i32> {
    let class = env.find_class("java/util/Map")?;
    let method = env.method_id(class, "size", "()I")?;
    let f = unsafe { call_int_method(env) };
    let value = unsafe { f(env.0, map, method, std::ptr::null()) };
    env.exception_check().then_some(value)
}

/// `PENGUDROID_FONTDEBUG=2`: step through the fonts.xml parse by hand.
///
/// `FontListParser.readFamilies` returns an empty list without throwing, so the
/// only way to see where it collapses is to drive the same `XmlPullParser`
/// calls and report the event stream.
pub fn font_parse_debug(env: &JniEnv) {
    let Some(xml) = env.find_class("android/util/Xml") else {
        eprintln!("fontparse: android.util.Xml not found");
        return;
    };
    let Some(new_parser) = env.static_method_id(xml, "newPullParser", "()Lorg/xmlpull/v1/XmlPullParser;") else {
        eprintln!("fontparse: Xml.newPullParser missing");
        return;
    };
    let Some(parser) = env.call_static_object(xml, new_parser, &[]) else {
        eprintln!("fontparse: newPullParser threw");
        return;
    };
    if parser.is_null() {
        eprintln!("fontparse: newPullParser returned null");
        return;
    }
    let Some(parser_class) = env.find_class("org/xmlpull/v1/XmlPullParser") else {
        return;
    };

    // setInput(InputStream, String)
    let Some(file_class) = env.find_class("java/io/FileInputStream") else { return };
    let Some(file_ctor) = env.method_id(file_class, "<init>", "(Ljava/lang/String;)V") else {
        return;
    };
    let path = env.new_string("/system/etc/fonts.xml");
    let Some(stream) = env.new_object(file_class, file_ctor, &[JValue { l: path }]) else {
        eprintln!("fontparse: FileInputStream threw");
        return;
    };
    let Some(set_input) = env.method_id(
        parser_class,
        "setInput",
        "(Ljava/io/InputStream;Ljava/lang/String;)V",
    ) else {
        eprintln!("fontparse: setInput missing");
        return;
    };
    if !env.call_void(parser, set_input, &[JValue { l: stream }, JValue { l: std::ptr::null_mut() }]) {
        eprintln!("fontparse: setInput threw");
        return;
    }

    let Some(next_tag) = env.method_id(parser_class, "nextTag", "()I") else { return };
    let Some(next) = env.method_id(parser_class, "next", "()I") else { return };
    let Some(get_name) = env.method_id(parser_class, "getName", "()Ljava/lang/String;") else {
        return;
    };
    let Some(get_event) = env.method_id(parser_class, "getEventType", "()I") else { return };
    let Some(get_text) = env.method_id(parser_class, "getText", "()Ljava/lang/String;") else {
        return;
    };

    let int_call = unsafe { call_int_method(env) };
    let event = unsafe { int_call(env.0, parser, next_tag, std::ptr::null()) };
    if !env.exception_check() {
        eprintln!("fontparse: nextTag threw");
        return;
    }
    let root = env.call_object(parser, get_name, &[]).and_then(|n| env.get_string(n));
    println!("fontparse: nextTag -> event {event}, root {root:?}");

    let mut families = 0;
    let mut fonts = 0;
    let mut texts = 0;
    let mut first_text: Option<String> = None;
    for _ in 0..4000 {
        let ev = unsafe { int_call(env.0, parser, next, std::ptr::null()) };
        if !env.exception_check() {
            eprintln!("fontparse: next() threw after {families} families");
            return;
        }
        match ev {
            1 => break, // END_DOCUMENT
            2 => {
                let name = env.call_object(parser, get_name, &[]).and_then(|n| env.get_string(n));
                match name.as_deref() {
                    Some("family") => families += 1,
                    Some("font") => fonts += 1,
                    _ => {}
                }
            }
            4 => {
                texts += 1;
                if first_text.is_none() {
                    first_text = env.call_object(parser, get_text, &[]).and_then(|t| env.get_string(t));
                }
            }
            _ => {}
        }
    }
    println!(
        "fontparse: families={families} fonts={fonts} text-events={texts} first-text={first_text:?}"
    );
    let _ = get_event;

    // Does `File.isFile()` agree with the shell?
    let Some(file) = env.find_class("java/io/File") else { return };
    let Some(ctor) = env.method_id(file, "<init>", "(Ljava/lang/String;)V") else { return };
    let Some(is_file) = env.method_id(file, "isFile", "()Z") else { return };
    let Some(exists) = env.method_id(file, "exists", "()Z") else { return };
    let bool_call: unsafe extern "C" fn(*mut c_void, JObject, JMethodId, *const JValue) -> u8 =
        unsafe {
            let table = *(env.0 as *const *const *const c_void);
            std::mem::transmute(*table.add(39)) // CallBooleanMethodA
        };
    for p in ["/system/fonts/Roboto-Regular.ttf", "/system/etc/fonts.xml"] {
        let s = env.new_string(p);
        let Some(f) = env.new_object(file, ctor, &[JValue { l: s }]) else { continue };
        let e = unsafe { bool_call(env.0, f, exists, std::ptr::null()) };
        let r = unsafe { bool_call(env.0, f, is_file, std::ptr::null()) };
        println!("fontparse: {p} exists={e} isFile={r}");
    }
}

/// `PENGUDROID_FONTDEBUG=1`: pinpoint why the system font map comes out empty.
///
/// Walks the chain `FontListParser` -> `SystemFonts.buildSystemFallback` ->
/// `Font.Builder.build` and reports where it collapses, so the fix can target
/// the native call that actually fails instead of guessing.
pub fn font_debug(env: &JniEnv) {
    let Some(sf) = env.find_class("android/graphics/fonts/SystemFonts") else {
        eprintln!("fontdebug: SystemFonts not found");
        return;
    };
    let Some(get_cfg) = env.static_method_id(
        sf,
        "getSystemPreinstalledFontConfig",
        "()Landroid/text/FontConfig;",
    ) else {
        eprintln!("fontdebug: getSystemPreinstalledFontConfig missing");
        return;
    };
    let Some(config) = env.call_static_object(sf, get_cfg, &[]) else {
        eprintln!("fontdebug: getSystemPreinstalledFontConfig threw");
        return;
    };
    if config.is_null() {
        eprintln!("fontdebug: font config is null");
        return;
    }
    let families = env
        .method_id(env.find_class("android/text/FontConfig").unwrap_or(std::ptr::null_mut()), "getFontFamilies", "()Ljava/util/List;")
        .and_then(|m| env.call_object(config, m, &[]));
    match families {
        Some(list) => println!("fontdebug: config families = {:?}", list_size(env, list)),
        None => eprintln!("fontdebug: getFontFamilies threw"),
    }

    let Some(build) = env.static_method_id(
        sf,
        "buildSystemFallback",
        "(Landroid/text/FontConfig;)Ljava/util/Map;",
    ) else {
        eprintln!("fontdebug: buildSystemFallback missing");
        return;
    };
    match env.call_static_object(sf, build, &[JValue { l: config }]) {
        Some(map) if map.is_null() => eprintln!("fontdebug: buildSystemFallback returned null"),
        Some(map) => println!("fontdebug: fallback families = {:?}", map_size(env, map)),
        None => eprintln!("fontdebug: buildSystemFallback threw"),
    }

    // The single most informative step: can a Font be built from a real file?
    let Some(file_class) = env.find_class("java/io/File") else { return };
    let Some(file_ctor) = env.method_id(file_class, "<init>", "(Ljava/lang/String;)V") else {
        return;
    };
    let path = env.new_string("/system/fonts/Roboto-Regular.ttf");
    let Some(file) = env.new_object(file_class, file_ctor, &[JValue { l: path }]) else {
        eprintln!("fontdebug: File ctor threw");
        return;
    };
    let Some(font_builder) = env.find_class("android/graphics/fonts/Font$Builder") else {
        eprintln!("fontdebug: Font$Builder not found");
        return;
    };
    let Some(ctor) = env.method_id(font_builder, "<init>", "(Ljava/io/File;)V") else {
        eprintln!("fontdebug: Font$Builder(File) missing");
        return;
    };
    let Some(builder) = env.new_object(font_builder, ctor, &[JValue { l: file }]) else {
        eprintln!("fontdebug: Font$Builder ctor threw");
        return;
    };
    let Some(build_font) = env.method_id(
        font_builder,
        "build",
        "()Landroid/graphics/fonts/Font;",
    ) else {
        eprintln!("fontdebug: Font$Builder.build missing");
        return;
    };
    match env.call_object(builder, build_font, &[]) {
        Some(font) if font.is_null() => eprintln!("fontdebug: Font.Builder.build returned null"),
        Some(_) => println!("fontdebug: Font.Builder.build ok"),
        None => eprintln!("fontdebug: Font.Builder.build threw (see above)"),
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
    install_package_manager_replies(env, app_info, spec.package);
    // `handleBindApplication` calls `Typeface.loadPreinstalledSystemFontMap()`
    // itself, and that now works: the shim rewrites `stat64`/`lstat64` too, so
    // `FontListParser` accepts the font files and the map is built from
    // fonts.xml. Seeding a hand-made map here would only shadow the real one.
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
    // `mCoreSettings` is normally pushed in by `ActivityManagerService` through
    // `IApplicationThread.updateCoreSettings`; with no system server it stays
    // null and `handleBindApplication` dereferences it a few lines in
    // (`mCoreSettings.getString(Settings.System.TIME_12_24)`). An empty Bundle
    // is exactly what the framework treats as "no overrides".
    ensure_core_settings(env, thread_class, thread);
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
        install_app_class_loader(env, data, spec.apk);
        if let Some(compat) = compat {
            report_include_code(env, thread, app_info, compat, data);
        }
        if let Ok(name) = std::env::var("PENGUDROID_PROBE_APP_CLASS") {
            probe_app_class(env, data, &name);
        }
        if std::env::var_os("PENGUDROID_DUMP_HOLDER").is_some() {
            install_provider_replies(env, app_info, spec.package);
        }
        install_display_replies(env);
        if let Ok(name) = std::env::var("PENGUDROID_LAUNCH_ACTIVITY") {
            launch_activity(env, thread, data, spec, &name);
        }
    } else {
        eprintln!("apphost: handleBindApplication threw");
    }
    ok
}

/// Give the calling thread a `Looper` if it has none.
fn ensure_looper(env: &JniEnv) {
    let Some(looper_class) = env.find_class("android/os/Looper") else {
        return;
    };
    let Some(my_looper) = env.static_method_id(looper_class, "myLooper", "()Landroid/os/Looper;") else {
        return;
    };
    if let Some(existing) = env.call_static_object(looper_class, my_looper, &[]) {
        if !existing.is_null() {
            return;
        }
    }
    let Some(prepare) = env.static_method_id(looper_class, "prepare", "()V") else {
        return;
    };
    if env.call_static_void(looper_class, prepare, &[]) {
        println!("apphost: prepared a Looper for the bind thread");
    } else {
        eprintln!("apphost: Looper.prepare threw");
    }
}

/// Start an Activity through `ActivityThread.handleLaunchActivity`.
///
/// This is what `system_server` triggers with `scheduleLaunchActivity`; calling
/// it directly skips the parcel and the binder round trip, which is the same
/// shortcut `--bind-app` takes for `handleBindApplication`.
fn launch_activity(
    env: &JniEnv,
    thread: JObject,
    data: JObject,
    spec: &AppSpec<'_>,
    activity_name: &str,
) -> bool {
    let Some(thread_class) = env.find_class("android/app/ActivityThread") else {
        return false;
    };
    // `Activity`'s constructor creates a `Handler`, which requires a `Looper` on
    // the calling thread. `ActivityThread.main` prepared the main looper on the
    // helper thread; this thread needs its own before an Activity can be built.
    ensure_looper(env);
    let Some(bind_class) = env.find_class("android/app/ActivityThread$AppBindData") else {
        return false;
    };
    let Some(info_field) = env.field_id(bind_class, "info", "Landroid/app/LoadedApk;") else {
        return false;
    };
    let loaded_apk = env.object_field(data, info_field);
    if loaded_apk.is_null() {
        eprintln!("apphost: cannot launch an Activity before the app is bound");
        return false;
    }

    // ActivityInfo
    let Some(ai_class) = env.find_class("android/content/pm/ActivityInfo") else {
        return false;
    };
    let Some((_, activity_info)) = new_instance(env, "android/content/pm/ActivityInfo") else {
        eprintln!("apphost: cannot allocate ActivityInfo");
        return false;
    };
    if let Some(id) = env.field_id(ai_class, "name", "Ljava/lang/String;") {
        env.set_object_field(activity_info, id, env.new_string(activity_name));
    }
    if let Some(id) = env.field_id(ai_class, "packageName", "Ljava/lang/String;") {
        env.set_object_field(activity_info, id, env.new_string(spec.package));
    }
    if let Some(id) = env.field_id(
        ai_class,
        "applicationInfo",
        "Landroid/content/pm/ApplicationInfo;",
    ) {
        let app_info = env.object_field(loaded_apk, env.field_id(
            env.find_class("android/app/LoadedApk").unwrap_or(std::ptr::null_mut()),
            "mApplicationInfo",
            "Landroid/content/pm/ApplicationInfo;",
        ).unwrap_or(std::ptr::null_mut()));
        if !app_info.is_null() {
            env.set_object_field(activity_info, id, app_info);
        }
    }
    if let Some(id) = env.field_id(ai_class, "exported", "Z") {
        env.set_boolean_field(activity_info, id, true);
    }

    // Intent naming the component.
    let Some(intent_class) = env.find_class("android/content/Intent") else {
        return false;
    };
    let Some((_, intent)) = new_instance(env, "android/content/Intent") else {
        eprintln!("apphost: cannot allocate Intent");
        return false;
    };
    let Some(set_class_name) = env.method_id(
        intent_class,
        "setClassName",
        "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;",
    ) else {
        eprintln!("apphost: Intent.setClassName missing");
        return false;
    };
    let pkg = env.new_string(spec.package);
    let cls = env.new_string(activity_name);
    if env
        .call_object(intent, set_class_name, &[JValue { l: pkg }, JValue { l: cls }])
        .is_none()
    {
        eprintln!("apphost: Intent.setClassName threw");
        return false;
    }

    // ActivityClientRecord
    let Some(rec_class) = env.find_class("android/app/ActivityThread$ActivityClientRecord") else {
        return false;
    };
    let Some((_, record)) = new_instance(env, "android/app/ActivityThread$ActivityClientRecord")
    else {
        eprintln!("apphost: cannot allocate ActivityClientRecord");
        return false;
    };
    let mut set = |field: &str, sig: &str, value: JObject| {
        if let Some(id) = env.field_id(rec_class, field, sig) {
            env.set_object_field(record, id, value);
        } else {
            eprintln!("apphost: ActivityClientRecord.{field} not found");
        }
    };
    set("activityInfo", "Landroid/content/pm/ActivityInfo;", activity_info);
    set("intent", "Landroid/content/Intent;", intent);
    set("packageInfo", "Landroid/app/LoadedApk;", loaded_apk);

    let Some(handle) = env.method_id(
        thread_class,
        "handleLaunchActivity",
        "(Landroid/app/ActivityThread$ActivityClientRecord;Landroid/app/servertransaction/PendingTransactionActions;Landroid/content/Intent;)Landroid/app/Activity;",
    ) else {
        eprintln!("apphost: ActivityThread.handleLaunchActivity not found");
        return false;
    };
    println!("apphost: calling handleLaunchActivity for {activity_name}");
    match env.call_object(
        thread,
        handle,
        &[
            JValue { l: record },
            JValue { l: std::ptr::null_mut() },
            JValue { l: std::ptr::null_mut() },
        ],
    ) {
        Some(activity) if activity.is_null() => {
            eprintln!("apphost: handleLaunchActivity returned null");
            false
        }
        Some(_) => {
            println!("apphost: handleLaunchActivity returned an Activity");
            resume_activity(env, thread_class, thread, record);
            true
        }
        None => {
            eprintln!("apphost: handleLaunchActivity threw");
            false
        }
    }
}

/// Drive `onResume` and the window attach.
///
/// `handleLaunchActivity` only runs `onCreate`; on a device `TransactionExecutor`
/// follows it with `handleResumeActivity`, which calls `onResume` and then
/// `WindowManager.addView` — the first step into the window/SurfaceFlinger path.
fn resume_activity(env: &JniEnv, thread_class: JClass, thread: JObject, record: JObject) {
    let Some(handle) = env.method_id(
        thread_class,
        "handleResumeActivity",
        "(Landroid/app/ActivityThread$ActivityClientRecord;ZZZLjava/lang/String;)V",
    ) else {
        eprintln!("apphost: ActivityThread.handleResumeActivity not found");
        return;
    };
    let reason = env.new_string("pengudroid");
    println!("apphost: calling handleResumeActivity");
    let ok = env.call_void(
        thread,
        handle,
        &[
            JValue { l: record },
            JValue { z: 0 },
            JValue { z: 0 },
            JValue { z: 0 },
            JValue { l: reason },
        ],
    );
    if ok {
        println!("apphost: handleResumeActivity returned");
    } else {
        eprintln!("apphost: handleResumeActivity threw");
    }
}

/// Point the app's `LoadedApk` at a class loader that actually has the APK.
///
/// The loader the framework builds here comes out with an empty dex path
/// (`DexPathList[[]]`), so the APK's own classes are unreachable and no
/// Activity can be instantiated. `LoadedApk.getClassLoader()` returns
/// `mClassLoader` when it is non-null, so installing one built from the APK is
/// enough for `performLaunchActivity`.
fn install_app_class_loader(env: &JniEnv, data: JObject, apk: &Path) -> bool {
    let Some(bind_class) = env.find_class("android/app/ActivityThread$AppBindData") else {
        return false;
    };
    let Some(info_field) = env.field_id(bind_class, "info", "Landroid/app/LoadedApk;") else {
        return false;
    };
    let loaded_apk = env.object_field(data, info_field);
    if loaded_apk.is_null() {
        return false;
    }
    let Some(apk_class) = env.find_class("android/app/LoadedApk") else {
        return false;
    };

    let Some(pcl_class) = env.find_class("dalvik/system/PathClassLoader") else {
        eprintln!("apphost: PathClassLoader not found");
        return false;
    };
    let Some(ctor) = env.method_id(
        pcl_class,
        "<init>",
        "(Ljava/lang/String;Ljava/lang/ClassLoader;)V",
    ) else {
        eprintln!("apphost: PathClassLoader(String, ClassLoader) missing");
        return false;
    };
    let Some(cl_class) = env.find_class("java/lang/ClassLoader") else {
        return false;
    };
    let Some(get_system) = env.static_method_id(
        cl_class,
        "getSystemClassLoader",
        "()Ljava/lang/ClassLoader;",
    ) else {
        return false;
    };
    let Some(parent) = env.call_static_object(cl_class, get_system, &[]) else {
        return false;
    };
    let path = env.new_string(&apk.display().to_string());
    let Some(loader) = env.new_object(pcl_class, ctor, &[JValue { l: path }, JValue { l: parent }])
    else {
        eprintln!("apphost: PathClassLoader construction threw");
        return false;
    };

    let mut installed = false;
    for field in ["mClassLoader", "mDefaultClassLoader"] {
        if let Some(id) = env.field_id(apk_class, field, "Ljava/lang/ClassLoader;") {
            env.set_object_field(loaded_apk, id, loader);
            installed = true;
        }
    }
    if installed {
        println!("apphost: installed a PathClassLoader for {}", apk.display());
    }
    installed
}

/// Read `LoadedApk.mIncludeCode` for the bind's package info and for a fresh
/// `getPackageInfoNoCheck` call.
///
/// The bind's instance reports `false`, which makes the class loader come out
/// with an empty dex path. Comparing it against a direct call separates "the
/// bind asks for a code-less package info" from "this build maps the
/// constructor's boolean arguments differently".
fn report_include_code(
    env: &JniEnv,
    thread: JObject,
    app_info: JObject,
    compat: JObject,
    data: JObject,
) {
    let Some(apk_class) = env.find_class("android/app/LoadedApk") else {
        return;
    };
    let Some(field) = env.field_id(apk_class, "mIncludeCode", "Z") else {
        eprintln!("apphost: LoadedApk.mIncludeCode not found");
        return;
    };
    let register_field = env.field_id(apk_class, "mRegisterPackage", "Z");
    let security_field = env.field_id(apk_class, "mSecurityViolation", "Z");
    let get_bool: unsafe extern "C" fn(*mut c_void, JObject, JFieldId) -> u8 = unsafe {
        let table = *(env.0 as *const *const *const c_void);
        std::mem::transmute(*table.add(96)) // GetBooleanField
    };

    let Some(bind_class) = env.find_class("android/app/ActivityThread$AppBindData") else {
        return;
    };
    if let Some(info_field) = env.field_id(bind_class, "info", "Landroid/app/LoadedApk;") {
        let apk = env.object_field(data, info_field);
        if !apk.is_null() {
            println!(
                "apphost: bind's LoadedApk.mIncludeCode = {}",
                unsafe { get_bool(env.0, apk, field) }
            );
        }
    }

    let Some(thread_class) = env.find_class("android/app/ActivityThread") else {
        return;
    };
    let Some(get_info) = env.method_id(
        thread_class,
        "getPackageInfoNoCheck",
        "(Landroid/content/pm/ApplicationInfo;Landroid/content/res/CompatibilityInfo;)Landroid/app/LoadedApk;",
    ) else {
        eprintln!("apphost: 2-arg getPackageInfoNoCheck not found");
        return;
    };
    match env.call_object(thread, get_info, &[JValue { l: app_info }, JValue { l: compat }]) {
        Some(apk) if !apk.is_null() => {
            println!(
                "apphost: fresh getPackageInfoNoCheck.mIncludeCode = {}",
                unsafe { get_bool(env.0, apk, field) }
            );
            if let Some(id) = register_field {
                println!(
                    "apphost: fresh mRegisterPackage = {}",
                    unsafe { get_bool(env.0, apk, id) }
                );
            }
            if let Some(id) = security_field {
                println!(
                    "apphost: fresh mSecurityViolation = {}",
                    unsafe { get_bool(env.0, apk, id) }
                );
            }
        }
        Some(_) => eprintln!("apphost: getPackageInfoNoCheck returned null"),
        None => eprintln!("apphost: getPackageInfoNoCheck threw"),
    }
}

/// Resolve a class through the app's own `LoadedApk` class loader.
///
/// This is the loader `Instrumentation.newActivity` will use, so it is the
/// first thing a launch needs: if the APK's dex is not on its path, no Activity
/// can be instantiated no matter what the binder side does.
fn probe_app_class(env: &JniEnv, data: JObject, name: &str) {
    let Some(bind_class) = env.find_class("android/app/ActivityThread$AppBindData") else {
        return;
    };
    let Some(field) = env.field_id(bind_class, "info", "Landroid/app/LoadedApk;") else {
        eprintln!("apphost: AppBindData.info not found");
        return;
    };
    let loaded_apk = env.object_field(data, field);
    if loaded_apk.is_null() {
        eprintln!("apphost: AppBindData.info is null after the bind");
        return;
    }
    let Some(apk_class) = env.find_class("android/app/LoadedApk") else {
        return;
    };
    // Report what the loader was built from: an empty dex path means
    // `sourceDir` was null when `createOrUpdateClassLoaderLocked` ran.
    if let Some(id) = env.field_id(apk_class, "mIncludeCode", "Z") {
        let f: unsafe extern "C" fn(*mut c_void, JObject, JFieldId) -> u8 = unsafe {
            let table = *(env.0 as *const *const *const c_void);
            std::mem::transmute(*table.add(96)) // GetBooleanField
        };
        println!("apphost: LoadedApk.mIncludeCode = {}", unsafe { f(env.0, loaded_apk, id) });
    }
    if let Some(id) = env.field_id(
        apk_class,
        "mApplicationInfo",
        "Landroid/content/pm/ApplicationInfo;",
    ) {
        let ai = env.object_field(loaded_apk, id);
        if !ai.is_null() {
            if let Some(sid) = env.field_id(
                env.find_class("android/content/pm/ApplicationInfo").unwrap_or(std::ptr::null_mut()),
                "sourceDir",
                "Ljava/lang/String;",
            ) {
                let s = env.object_field(ai, sid);
                println!("apphost: LoadedApk.mApplicationInfo.sourceDir = {:?}", env.get_string(s));
            }
        }
    }
    let Some(get_loader) = env.method_id(apk_class, "getClassLoader", "()Ljava/lang/ClassLoader;") else {
        eprintln!("apphost: LoadedApk.getClassLoader missing");
        return;
    };
    let Some(loader) = env.call_object(loaded_apk, get_loader, &[]) else {
        eprintln!("apphost: LoadedApk.getClassLoader threw");
        return;
    };
    if loader.is_null() {
        eprintln!("apphost: LoadedApk.getClassLoader returned null");
        return;
    }
    let Some(loader_class) = env.find_class("java/lang/ClassLoader") else {
        return;
    };
    let Some(load) = env.method_id(
        loader_class,
        "loadClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
    ) else {
        return;
    };
    let arg = env.new_string(name);
    match env.call_object(loader, load, &[JValue { l: arg }]) {
        Some(cls) if cls.is_null() => eprintln!("apphost: app class loader returned null for {name}"),
        Some(_) => println!("apphost: app class loader resolved {name}"),
        None => {
            eprintln!("apphost: app class loader could not load {name}");
            env.describe_exception();
        }
    }
}

/// Serialize `obj` with the framework's own `writeToParcel`.
///
/// Hand-rolling a parcel for a framework class is version-specific and fails
/// silently; asking the class to write itself keeps the layout exact.
fn marshal_object(env: &JniEnv, obj: JObject, class_name: &str) -> Option<Vec<u8>> {
    let class = env.find_class(class_name)?;
    let parcel_class = env.find_class("android/os/Parcel")?;
    let obtain = env.static_method_id(parcel_class, "obtain", "()Landroid/os/Parcel;")?;
    let parcel = env.call_static_object(parcel_class, obtain, &[])?;
    let write = env.method_id(class, "writeToParcel", "(Landroid/os/Parcel;I)V")?;
    if !env.call_void(obj, write, &[JValue { l: parcel }, JValue { i: 0 }]) {
        eprintln!("apphost: {class_name}.writeToParcel threw");
        return None;
    }
    let marshall = env.method_id(parcel_class, "marshall", "()[B")?;
    let bytes = env.call_object(parcel, marshall, &[])?;
    env.byte_array_copy(bytes)
}

/// Hand a serialized reply to the binder broker.
fn install_reply(data: &[u8], symbol: &str) {
    // SAFETY: the shim is loaded in this process; the symbol takes a plain
    // pointer/length pair and copies the bytes.
    unsafe {
        let name = std::ffi::CString::new(symbol).ok();
        let Some(name) = name else { return };
        let sym = libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr());
        if sym.is_null() {
            eprintln!("apphost: binder broker does not export {symbol}");
            return;
        }
        let install: unsafe extern "C" fn(*const u8, usize) = std::mem::transmute(sym);
        install(data.as_ptr(), data.len());
        println!("apphost: installed a {} byte reply for {symbol}", data.len());
    }
}

/// Answer the `IPackageManager` queries `handleBindApplication` makes.
///
/// `LoadedApk.initializeJavaContextClassLoader()` asks for the app's
/// `PackageInfo` and throws "is package not installed?" when it is null; a few
/// lines later `handleBindApplication` asks for the `ApplicationInfo` and
/// dereferences `metaData` on the result. There is no package manager here, so
/// both answers are built from the `ApplicationInfo` we already have.
fn install_package_manager_replies(env: &JniEnv, app_info: JObject, package: &str) {
    let Some(pi_class) = env.find_class("android/content/pm/PackageInfo") else {
        eprintln!("apphost: PackageInfo not found");
        return;
    };
    let Some(ctor) = env.method_id(pi_class, "<init>", "()V") else {
        return;
    };
    let Some(pi) = env.new_object(pi_class, ctor, &[]) else {
        eprintln!("apphost: cannot allocate PackageInfo");
        return;
    };
    if let Some(id) = env.field_id(pi_class, "packageName", "Ljava/lang/String;") {
        env.set_object_field(pi, id, env.new_string(package));
    }
    if let Some(id) = env.field_id(
        pi_class,
        "applicationInfo",
        "Landroid/content/pm/ApplicationInfo;",
    ) {
        env.set_object_field(pi, id, app_info);
    }
    if let Some(id) = env.field_id(pi_class, "versionName", "Ljava/lang/String;") {
        env.set_object_field(pi, id, env.new_string("1.0"));
    }
    if let Some(id) = env.field_id(pi_class, "versionCode", "I") {
        env.set_int_field(pi, id, 1);
    }

    if let Some(data) = marshal_object(env, pi, "android/content/pm/PackageInfo") {
        install_reply(&data, "__pengudroid_binder_set_package_info");
    }
    if let Some(data) = marshal_object(env, app_info, "android/content/pm/ApplicationInfo") {
        install_reply(&data, "__pengudroid_binder_set_application_info");
    }
}

/// Install the replies the display stack needs.
///
/// `ContextImpl.getDisplayNoVerify()` asks `ResourcesManager` for the default
/// display, which goes through `DisplayManagerGlobal.getCompatibleDisplay` ->
/// `IDisplayManager.getDisplayInfo`. With no display service the reply is
/// empty, `getDisplayInfo` returns null and `Activity.setContentView` NPEs on
/// `Display.getDisplayId()`. The `DisplayInfo` is marshalled by the framework
/// itself so the layout matches this build.
fn install_display_replies(env: &JniEnv) {
    let Some(info_class) = env.find_class("android/view/DisplayInfo") else {
        eprintln!("apphost: DisplayInfo not found");
        return;
    };
    let Some((_, info)) = new_instance(env, "android/view/DisplayInfo") else {
        eprintln!("apphost: cannot allocate DisplayInfo");
        return;
    };
    let set_int = |field: &str, value: i32| {
        if let Some(id) = env.field_id(info_class, field, "I") {
            env.set_int_field(info, id, value);
        }
    };
    let set_str = |field: &str, value: &str| {
        if let Some(id) = env.field_id(info_class, field, "Ljava/lang/String;") {
            env.set_object_field(info, id, env.new_string(value));
        }
    };
    set_int("displayId", 0);
    set_str("name", "PenguDroid");
    set_str("uniqueId", "local:0");
    for field in [
        "appWidth",
        "appHeight",
        "logicalWidth",
        "logicalHeight",
        "largestNominalAppWidth",
        "largestNominalAppHeight",
        "smallestNominalAppWidth",
        "smallestNominalAppHeight",
    ] {
        set_int(field, if field.contains("Height") { 720 } else { 1280 });
    }
    // `DisplayInfo` has no `densityDpi` field in this build; the density is
    // derived from the logical size and the display metrics.
    // Display.STATE_ON = 2, Display.TYPE_INTERNAL = 1, Surface.ROTATION_0 = 0.
    set_int("state", 2);
    set_int("committedState", 2);
    set_int("type", 1);
    set_int("rotation", 0);
    set_int("modeId", 1);
    set_int("layerStack", 0);
    set_int("displayGroupId", 0);
    set_int("colorMode", 0);

    if let Some(data) = marshal_object(env, info, "android/view/DisplayInfo") {
        install_reply(&data, "__pengudroid_binder_set_display_info");
    }
}

/// Install the replies the `settings` provider needs.
///
/// `Activity.attach` reads `Settings.Global`, which resolves the `settings`
/// content provider through `IActivityManager.getContentProvider` and then
/// calls `IContentProvider.call`. There is no provider here, so the broker
/// answers with a `ProviderInfo` and a handle, and every `call` returns an
/// empty `Bundle` (so settings reads fall back to their defaults).
fn install_provider_replies(env: &JniEnv, app_info: JObject, package: &str) {
    let Some(pi_class) = env.find_class("android/content/pm/ProviderInfo") else {
        eprintln!("apphost: ProviderInfo not found");
        return;
    };
    let Some((_, info)) = new_instance(env, "android/content/pm/ProviderInfo") else {
        return;
    };
    if let Some(id) = env.field_id(pi_class, "name", "Ljava/lang/String;") {
        env.set_object_field(info, id, env.new_string("SettingsProvider"));
    }
    if let Some(id) = env.field_id(pi_class, "authority", "Ljava/lang/String;") {
        env.set_object_field(info, id, env.new_string("settings"));
    }
    if let Some(id) = env.field_id(pi_class, "packageName", "Ljava/lang/String;") {
        env.set_object_field(info, id, env.new_string(package));
    }
    if let Some(id) = env.field_id(
        pi_class,
        "applicationInfo",
        "Landroid/content/pm/ApplicationInfo;",
    ) {
        env.set_object_field(info, id, app_info);
    }
    if let Some(data) = marshal_object(env, info, "android/content/pm/ProviderInfo") {
        install_reply(&data, "__pengudroid_binder_set_provider_info");
    }
    if let Some((_, bundle)) = new_instance(env, "android/os/Bundle") {
        if let Some(data) = marshal_object(env, bundle, "android/os/Bundle") {
            install_reply(&data, "__pengudroid_binder_set_empty_bundle");
        }
    }
}

/// Give the `ActivityThread` an empty core-settings `Bundle` if it has none.
fn ensure_core_settings(env: &JniEnv, thread_class: JClass, thread: JObject) {
    let Some(field) = env.field_id(thread_class, "mCoreSettings", "Landroid/os/Bundle;") else {
        eprintln!("apphost: ActivityThread.mCoreSettings not found");
        return;
    };
    if !env.object_field(thread, field).is_null() {
        return;
    }
    let Some((bundle_class, bundle)) = new_instance(env, "android/os/Bundle") else {
        eprintln!("apphost: cannot allocate a Bundle for mCoreSettings");
        return;
    };
    let _ = bundle_class;
    env.set_object_field(thread, field, bundle);
    println!("apphost: seeded an empty mCoreSettings Bundle");
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
