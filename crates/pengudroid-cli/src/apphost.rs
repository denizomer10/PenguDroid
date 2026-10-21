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
    /// `android:name` of the `<application>` element, if the manifest has one.
    ///
    /// `LoadedApk.makeApplicationInner` reads it from
    /// `ApplicationInfo.className`; leaving it null makes the framework
    /// instantiate a plain `android.app.Application`, so the app's own
    /// `onCreate` (where Roblox loads its native library) never runs.
    pub app_class: Option<&'a str>,
    /// The manifest's `<provider>` entries.
    ///
    /// `handleBindApplication` calls `installContentProviders(app, providers)`,
    /// and that is how `androidx.startup.InitializationProvider` runs — which is
    /// what initialises WorkManager, EmojiCompat and friends. With an empty list
    /// every one of those stays uninitialised and the app dies on
    /// "WorkManager is not initialized properly".
    pub providers: &'a [crate::apk::Component],
    /// `android:theme` resource id for the launched activity, if the manifest
    /// declares one (the activity's own, else the application's).
    pub theme: Option<u32>,
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
    if let Some(app_class) = spec.app_class {
        set_str("className", app_class);
    }
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
    // `FLAG_HARDWARE_ACCELERATED` (0x10000000) is what makes `ViewRootImpl`
    // create a `ThreadedRenderer`; without it the window is added but nothing is
    // ever drawn. `FLAG_HAS_CODE` (0x4) and `FLAG_SUPPORTS_SCREEN_DENSITIES`
    // (0x2000) are what a normal APK declares.
    set_int("flags", 0x1000_0000 | 0x2000 | 0x4);
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
        // `Configuration.updateFrom` only copies the locale list when the delta
        // has `userSetLocale` set, and `setLocales` does not set it. Without
        // this the activity's `Resources` keeps a null locale list and
        // `ConfigurationCompat.getLocales(...)` NPEs.
        if let Some(id) = env.field_id(class, "userSetLocale", "Z") {
            env.set_boolean_field(config, id, true);
        }
        // `Resources` builds its own `Configuration` from the process default,
        // and `ConfigurationCompat.getLocales(...)` NPEs when that one's locale
        // list is null (NewPipe's `Localization.getAppLocale`).
        if let Some(list_class) = env.find_class("android/os/LocaleList") {
            if let Some(m) = env.static_method_id(
                list_class,
                "setDefault",
                "(Landroid/os/LocaleList;)V",
            ) {
                env.call_static_void(list_class, m, &[JValue { l: loc }]);
            }
        }
    }
    // `setToDefaults()` leaves `densityDpi` at DPI_UNDEFINED (0), which makes
    // `DisplayMetrics.scaledDensity` zero -- and then every `sp` dimension
    // resolves to 0px (NewPipe's auto-size text sizes hit exactly that).
    let set_int = |field: &str, value: i32| {
        if let Some(id) = env.field_id(class, field, "I") {
            env.set_int_field(config, id, value);
        }
    };
    set_int("densityDpi", 420);
    // 1280x720 at 420 dpi, in dp.
    set_int("screenWidthDp", 487);
    set_int("screenHeightDp", 274);
    set_int("smallestScreenWidthDp", 274);
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

/// Give the app's `Resources` a usable `Configuration`.
///
/// `Resources` keeps its own `Configuration` instance, separate from the one in
/// `AppBindData`, and `ConfigurationCompat.getLocales(...)` NPEs when that
/// instance's locale list is null (NewPipe's `Localization.getAppLocale`).
fn fix_app_configuration(env: &JniEnv, data: JObject) {
    let Some(bind_class) = env.find_class("android/app/ActivityThread$AppBindData") else {
        return;
    };
    let Some(info_field) = env.field_id(bind_class, "info", "Landroid/app/LoadedApk;") else {
        return;
    };
    let loaded_apk = env.object_field(data, info_field);
    if loaded_apk.is_null() {
        return;
    }
    let Some(apk_class) = env.find_class("android/app/LoadedApk") else {
        return;
    };
    let Some(get_res) = env.method_id(
        apk_class,
        "getResources",
        "()Landroid/content/res/Resources;",
    ) else {
        return;
    };
    let Some(res) = env.call_object(loaded_apk, get_res, &[]) else {
        return;
    };
    if res.is_null() {
        return;
    }
    let Some(res_class) = env.find_class("android/content/res/Resources") else {
        return;
    };
    let Some(get_cfg) = env.method_id(
        res_class,
        "getConfiguration",
        "()Landroid/content/res/Configuration;",
    ) else {
        return;
    };
    let Some(cfg) = env.call_object(res, get_cfg, &[]) else {
        return;
    };
    if cfg.is_null() {
        return;
    }
    let Some(cfg_class) = env.find_class("android/content/res/Configuration") else {
        return;
    };
    if let Some(loc) = locale_list(env) {
        if let Some(m) = env.method_id(cfg_class, "setLocales", "(Landroid/os/LocaleList;)V") {
            env.call_void(cfg, m, &[JValue { l: loc }]);
        } else {
            eprintln!("apphost: Configuration.setLocales missing");
        }
    } else {
        eprintln!("apphost: cannot build a LocaleList");
    }
    if let Some(get_locales) = env.method_id(
        cfg_class,
        "getLocales",
        "()Landroid/os/LocaleList;",
    ) {
        let locales = env.call_object(cfg, get_locales, &[]);
        match locales {
            Some(l) if !l.is_null() => println!("apphost: app Resources locales = ok"),
            _ => eprintln!("apphost: app Resources locales = NULL"),
        }
    }
    if let Some(id) = env.field_id(cfg_class, "densityDpi", "I") {
        env.set_int_field(cfg, id, 420);
    }
    // `ResourcesManager` builds every new `Resources` from its process-wide
    // configuration, and `Resources.getSystem()` exposes that same instance.
    // Patching it directly is the only way to reach the copy the activity gets.
    if let Some(get_system) = env.static_method_id(
        res_class,
        "getSystem",
        "()Landroid/content/res/Resources;",
    ) {
        if let Some(sys) = env.call_static_object(res_class, get_system, &[]) {
            if let Some(sys_cfg) = env.call_object(sys, get_cfg, &[]) {
                if !sys_cfg.is_null() {
                    if let Some(loc) = locale_list(env) {
                        if let Some(m) = env.method_id(
                            cfg_class,
                            "setLocales",
                            "(Landroid/os/LocaleList;)V",
                        ) {
                            env.call_void(sys_cfg, m, &[JValue { l: loc }]);
                        }
                    }
                    if let Some(id) = env.field_id(cfg_class, "densityDpi", "I") {
                        env.set_int_field(sys_cfg, id, 420);
                    }
                    if let Some(get_locales) = env.method_id(
                        cfg_class,
                        "getLocales",
                        "()Landroid/os/LocaleList;",
                    ) {
                        let l = env.call_object(sys_cfg, get_locales, &[]);
                        match l {
                            Some(v) if !v.is_null() => println!("apphost: system locales = ok"),
                            _ => eprintln!("apphost: system locales = NULL"),
                        }
                    }
                }
            }
        }
    }
    // `ResourcesManager` keeps a process-wide configuration that every new
    // `Resources` is built from; the system server normally pushes it with
    // `applyConfigurationToResources`. Without it the activity's own
    // `Resources` gets a bare `Configuration` whose locale list is null.
    if let Some(rm_class) = env.find_class("android/app/ResourcesManager") {
        // `applyConfigurationToResources` is an *instance* method returning
        // boolean; the singleton comes from `ResourcesManager.getInstance()`.
        if let (Some(get_instance), Some(apply)) = (
            env.static_method_id(
                rm_class,
                "getInstance",
                "()Landroid/app/ResourcesManager;",
            ),
            env.method_id(
                rm_class,
                "applyConfigurationToResources",
                "(Landroid/content/res/Configuration;Landroid/content/res/CompatibilityInfo;)Z",
            ),
        ) {
            if let Some(rm) = env.call_static_object(rm_class, get_instance, &[]) {
                let compat = system_compat_info(env).unwrap_or(std::ptr::null_mut());
                if env.call_boolean(rm, apply, &[JValue { l: cfg }, JValue { l: compat }]).is_some() {
                    println!("apphost: applied the configuration to ResourcesManager");
                } else {
                    eprintln!("apphost: applyConfigurationToResources threw");
                }
                // JNI field access ignores `private`, so the process-wide
                // configuration can be inspected (and patched) directly.
                if let Some(f) = env.field_id(
                    rm_class,
                    "mResConfiguration",
                    "Landroid/content/res/Configuration;",
                ) {
                    let res_cfg = env.object_field(rm, f);
                    if !res_cfg.is_null() {
                        if let Some(get_locales) = env.method_id(
                            cfg_class,
                            "getLocales",
                            "()Landroid/os/LocaleList;",
                        ) {
                            let l = env.call_object(res_cfg, get_locales, &[]);
                            match l {
                                Some(v) if !v.is_null() => {
                                    println!("apphost: mResConfiguration locales = ok")
                                }
                                _ => eprintln!("apphost: mResConfiguration locales = NULL"),
                            }
                        }
                        if let Some(loc) = locale_list(env) {
                            if let Some(m) = env.method_id(
                                cfg_class,
                                "setLocales",
                                "(Landroid/os/LocaleList;)V",
                            ) {
                                env.call_void(res_cfg, m, &[JValue { l: loc }]);
                            }
                        }
                    }
                }
            }
        }
    }
    println!("apphost: patched the app Resources configuration");
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
    // `RobloxApplication.onCreate` reads `Settings.Global` (through
    // `Settings.NameValueCache`), which resolves the `settings` provider via
    // `IActivityManager.getContentProvider` *during* the bind. The reply has to
    // be in place before `handleBindApplication` runs, not after it.
    install_provider_replies(env, app_info, spec.package);
    // `RobloxApplication.onCreate` also asks `DisplayManager` for the default
    // display's size, which goes through `IDisplayManager.getDisplayInfo`.
    install_display_replies(env);
    install_window_replies(env);
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
    install_provider_list(env, providers, app_info, spec);
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
    // Create the `LoadedApk` and give it a working class loader *before* the
    // bind: `handleBindApplication` instantiates the Application through it, so
    // the app's own class has to be reachable by then. `getPackageInfoNoCheck`
    // caches the instance in `ActivityThread.mPackages`, and the bind reuses it
    // (the app uid is in user 0, so the cache is not skipped).
    if let Some(compat) = compat {
        if let Some(get_info) = env.method_id(
            thread_class,
            "getPackageInfoNoCheck",
            "(Landroid/content/pm/ApplicationInfo;Landroid/content/res/CompatibilityInfo;)Landroid/app/LoadedApk;",
        ) {
            match env.call_object(thread, get_info, &[JValue { l: app_info }, JValue { l: compat }]) {
                Some(apk) if !apk.is_null() => {
                    install_class_loader_on(env, apk, spec.apk, spec.native_lib_dir);
                }
                Some(_) => eprintln!("apphost: getPackageInfoNoCheck returned null"),
                None => eprintln!("apphost: getPackageInfoNoCheck threw"),
            }
        }
    }
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
        if let Some(compat) = compat {
            report_include_code(env, thread, app_info, compat, data);
        }
        if let Ok(name) = std::env::var("PENGUDROID_PROBE_APP_CLASS") {
            probe_app_class(env, data, &name);
        }
        report_display_metrics(env);
        fix_app_configuration(env, data);
        report_application(env, thread);
        if let Ok(name) = std::env::var("PENGUDROID_LOAD_LIB") {
            probe_load_library(env, &name);
        }
        if let Ok(name) = std::env::var("PENGUDROID_LAUNCH_ACTIVITY") {
            launch_activity(env, thread, data, spec, &name);
        }
    } else {
        eprintln!("apphost: handleBindApplication threw");
    }
    ok
}

/// Print the display metrics the app sees.
///
/// `sp` dimensions resolve through `scaledDensity`; if that is zero every
/// `sp` value becomes 0px, which is what breaks NewPipe's auto-size text.
fn report_display_metrics(env: &JniEnv) {
    let Some(res_class) = env.find_class("android/content/res/Resources") else {
        eprintln!("metrics: Resources not found");
        return;
    };
    let Some(get_system) = env.static_method_id(res_class, "getSystem", "()Landroid/content/res/Resources;") else {
        eprintln!("metrics: Resources.getSystem missing");
        return;
    };
    let Some(res) = env.call_static_object(res_class, get_system, &[]) else {
        eprintln!("metrics: Resources.getSystem threw");
        return;
    };
    let Some(get_metrics) = env.method_id(res_class, "getDisplayMetrics", "()Landroid/util/DisplayMetrics;") else {
        eprintln!("metrics: getDisplayMetrics missing");
        return;
    };
    let Some(metrics) = env.call_object(res, get_metrics, &[]) else {
        eprintln!("metrics: getDisplayMetrics threw");
        return;
    };
    let Some(metrics_class) = env.find_class("android/util/DisplayMetrics") else {
        eprintln!("metrics: DisplayMetrics not found");
        return;
    };
    let get_float: unsafe extern "C" fn(*mut c_void, JObject, JFieldId) -> f32 = unsafe {
        let table = *(env.0 as *const *const *const c_void);
        std::mem::transmute(*table.add(102)) // GetFloatField
    };
    let get_int: unsafe extern "C" fn(*mut c_void, JObject, JFieldId) -> i32 = unsafe {
        let table = *(env.0 as *const *const *const c_void);
        std::mem::transmute(*table.add(100)) // GetIntField
    };
    for field in ["density", "scaledDensity"] {
        if let Some(id) = env.field_id(metrics_class, field, "F") {
            println!("apphost: DisplayMetrics.{field} = {}", unsafe { get_float(env.0, metrics, id) });
        }
    }
    if let Some(id) = env.field_id(metrics_class, "densityDpi", "I") {
        println!("apphost: DisplayMetrics.densityDpi = {}", unsafe { get_int(env.0, metrics, id) });
    }
}

/// Print the `Application` the bind created.
///
/// `handleBindApplication` instantiates it through `Instrumentation`; if that
/// silently produced nothing, the app's own `onCreate` (where Roblox loads its
/// native library) never runs.
fn report_application(env: &JniEnv, thread: JObject) {
    let Some(thread_class) = env.find_class("android/app/ActivityThread") else {
        return;
    };
    let Some(field) = env.field_id(thread_class, "mInitialApplication", "Landroid/app/Application;") else {
        eprintln!("app: ActivityThread.mInitialApplication not found");
        return;
    };
    let app = env.object_field(thread, field);
    if app.is_null() {
        eprintln!("app: mInitialApplication is null");
        return;
    }
    let Some(obj_class) = env.find_class("java/lang/Object") else {
        return;
    };
    let Some(get_class) = env.method_id(obj_class, "getClass", "()Ljava/lang/Class;") else {
        return;
    };
    let Some(cls) = env.call_object(app, get_class, &[]) else {
        return;
    };
    let Some(class_class) = env.find_class("java/lang/Class") else {
        return;
    };
    let Some(get_name) = env.method_id(class_class, "getName", "()Ljava/lang/String;") else {
        return;
    };
    if let Some(name) = env.call_object(cls, get_name, &[]).and_then(|n| env.get_string(n)) {
        println!("app: mInitialApplication = {name}");
    }
}

/// `PENGUDROID_LOAD_LIB=<name>`: call `System.loadLibrary(name)` and report.
///
/// `UnsatisfiedLinkError: No implementation found` means the library never
/// registered its natives; this says whether `loadLibrary` itself fails and, if
/// so, with what.
fn probe_load_library(env: &JniEnv, name: &str) {
    let Some(system_class) = env.find_class("java/lang/System") else {
        return;
    };
    let Some(load) = env.static_method_id(system_class, "loadLibrary", "(Ljava/lang/String;)V") else {
        eprintln!("loadlib: System.loadLibrary missing");
        return;
    };
    let arg = env.new_string(name);
    if env.call_static_void(system_class, load, &[JValue { l: arg }]) {
        println!("loadlib: System.loadLibrary({name}) ok");
    } else {
        eprintln!("loadlib: System.loadLibrary({name}) failed (see above)");
    }
}

/// Set up the main thread the way `ActivityThread.main` does, without entering
/// the loop.
///
/// An `Activity` must be created on the thread that owns the *main* `Looper`:
/// androidx's `LifecycleRegistry` enforces it (`addObserver must be called on
/// the main thread`), and so does a lot of framework code. `ActivityThread.main`
/// would give us that thread but then block in `Looper.loop()`, so the pieces
/// are done here instead and the caller enters the loop afterwards.
pub fn prepare_main_thread(env: &JniEnv) -> bool {
    let Some(looper_class) = env.find_class("android/os/Looper") else {
        return false;
    };
    let Some(prepare) = env.static_method_id(looper_class, "prepareMainLooper", "()V") else {
        eprintln!("apphost: Looper.prepareMainLooper missing");
        return false;
    };
    if !env.call_static_void(looper_class, prepare, &[]) {
        eprintln!("apphost: Looper.prepareMainLooper threw");
        return false;
    }
    println!("apphost: prepared the main Looper");

    let Some(thread_class) = env.find_class("android/app/ActivityThread") else {
        return false;
    };
    let Some(ctor) = env.method_id(thread_class, "<init>", "()V") else {
        eprintln!("apphost: ActivityThread() missing");
        return false;
    };
    let Some(thread) = env.new_object(thread_class, ctor, &[]) else {
        eprintln!("apphost: new ActivityThread() threw");
        return false;
    };
    let Some(attach) = env.method_id(thread_class, "attach", "(ZJ)V") else {
        eprintln!("apphost: ActivityThread.attach missing");
        return false;
    };
    if !env.call_void(thread, attach, &[JValue { z: 0 }, JValue { j: 0 }]) {
        eprintln!("apphost: ActivityThread.attach threw");
        return false;
    }
    println!("apphost: ActivityThread attached on the main thread");
    true
}

/// Enter the main `Looper`. Never returns for a GUI app.
pub fn loop_forever(env: &JniEnv) {
    let Some(looper_class) = env.find_class("android/os/Looper") else {
        return;
    };
    let Some(loop_fn) = env.static_method_id(looper_class, "loop", "()V") else {
        return;
    };
    println!("apphost: entering Looper.loop()");
    let _ = env.call_static_void(looper_class, loop_fn, &[]);
    eprintln!("apphost: Looper.loop() returned");
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
    // `Activity.attach` passes `(info.flags & ActivityInfo.FLAG_HARDWARE_ACCELERATED)
    // != 0` to `Window.setWindowManager`, and that is what puts
    // `FLAG_HARDWARE_ACCELERATED` on the window attributes — without it
    // `ViewRootImpl.enableHardwareAcceleration` never creates a renderer and
    // nothing is drawn.
    if let Some(id) = env.field_id(ai_class, "flags", "I") {
        // `ActivityInfo.FLAG_HARDWARE_ACCELERATED` is 0x200 in this build
        // (read out with `--dump-class`).
        env.set_int_field(activity_info, id, 0x200);
    }
    if let Some(theme) = spec.theme {
        if let Some(id) = env.field_id(ai_class, "theme", "I") {
            env.set_int_field(activity_info, id, theme as i32);
        }
    }
    // `AppCompatDelegate` asks `PackageManager.getActivityInfo` for the
    // component while the activity is being created; without an answer it logs
    // `NameNotFoundException` and falls back to the platform theme, which is
    // not an AppCompat theme. Publish the same object the launch uses.
    if let Some(bytes) = marshal_object(env, activity_info, "android/content/pm/ActivityInfo") {
        install_reply(&bytes, "__pengudroid_binder_set_activity_info");
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
    // The activity gets its own `Resources`, built from this override
    // configuration. Leaving it null makes `ResourcesManager` fall back to a
    // bare `Configuration` whose locale list is null, and
    // `ConfigurationCompat.getLocales(...)` then NPEs.
    if let Some(config) = new_configuration(env) {
        if let Some(cfg_class) = env.find_class("android/content/res/Configuration") {
            if let Some(get_locales) = env.method_id(
                cfg_class,
                "getLocales",
                "()Landroid/os/LocaleList;",
            ) {
                let locales = env.call_object(config, get_locales, &[]);
                match locales {
                    Some(l) if !l.is_null() => println!("apphost: overrideConfig locales = ok"),
                    _ => eprintln!("apphost: overrideConfig locales = NULL"),
                }
            }
        }
        // Deliberately *not* setting `overrideConfig`: with an override,
        // `ResourcesManager.generateConfig` builds a fresh `Configuration` copy,
        // while without one it hands out the live `mResConfiguration` — the very
        // object `fix_app_configuration` patched. That is the only way to be
        // sure the activity sees the locale list.
        let _ = config;
    }

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
fn install_class_loader_on(env: &JniEnv, loaded_apk: JObject, apk: &Path, native_lib_dir: &Path) -> bool {
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
        "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/ClassLoader;)V",
    ) else {
        eprintln!("apphost: PathClassLoader(String, String, ClassLoader) missing");
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
    // The library search path matters: `System.loadLibrary` resolves through
    // the class loader's native library directories, and the two-argument
    // constructor leaves them at the system defaults, so the app's own
    // `lib/<abi>/*.so` would never be found.
    let lib_path = env.new_string(&native_lib_dir.display().to_string());
    let Some(loader) = env.new_object(
        pcl_class,
        ctor,
        &[JValue { l: path }, JValue { l: lib_path }, JValue { l: parent }],
    ) else {
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
    // `PackageInfo.signingInfo` is null unless a package manager fills it in,
    // and signature checks (`org.acra`, Play Integrity, ...) dereference it
    // without a null check. A default `SigningInfo` reports "no signatures",
    // which is the truthful answer for an APK we never verified.
    if let Some((_, signing)) = new_instance(env, "android/content/pm/SigningInfo") {
        if let Some(id) = env.field_id(pi_class, "signingInfo", "Landroid/content/pm/SigningInfo;") {
            env.set_object_field(pi, id, signing);
        }
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
    // `Display.getRealMetrics()` reads `logicalDensityDpi`, and a zero there
    // makes `DisplayMetrics.density` zero — which turns
    // `ThreadedRenderer.setLightCenter`'s `zRatio` into Infinity and aborts the
    // first traversal with "lightZ must be a finite positive".
    set_int("logicalDensityDpi", 420);
    // Display.STATE_ON = 2, Display.TYPE_INTERNAL = 1, Surface.ROTATION_0 = 0.
    set_int("state", 2);
    set_int("committedState", 2);
    set_int("type", 1);
    set_int("rotation", 0);
    set_int("modeId", 1);
    // `Choreographer` asks the display for its refresh rate, which resolves
    // `modeId` against `supportedModes`; an empty array makes `findMode` throw
    // "Unable to locate mode 1".
    if let Some(mode_class) = env.find_class("android/view/Display$Mode") {
        let mut made = false;
        for sig in ["(IIIF[F)V", "(IIIF)V"] {
            let Some(ctor) = env.method_id(mode_class, "<init>", sig) else { continue };
            let rates = env.new_float_array(1);
            if !rates.is_null() {
                env.set_float_array_region(rates, 0, &[60.0]);
            }
            let args: &[JValue] = if sig == "(IIIF[F)V" {
                &[
                    JValue { i: 1 },
                    JValue { i: 1280 },
                    JValue { i: 720 },
                    JValue { f: 60.0 },
                    JValue { l: rates },
                ]
            } else {
                &[
                    JValue { i: 1 },
                    JValue { i: 1280 },
                    JValue { i: 720 },
                    JValue { f: 60.0 },
                ]
            };
            let Some(mode) = env.new_object(mode_class, ctor, args) else { continue };
            if let Some(arr) = env.new_object_array(1, mode_class, std::ptr::null_mut()) {
                env.set_object_array_element(arr, 0, mode);
                if let Some(id) = env.field_id(
                    info_class,
                    "supportedModes",
                    "[Landroid/view/Display$Mode;",
                ) {
                    env.set_object_field(info, id, arr);
                    made = true;
                }
            }
            if made {
                break;
            }
        }
        if !made {
            eprintln!("apphost: could not build a Display.Mode for supportedModes");
        }
    }
    set_int("layerStack", 0);
    set_int("displayGroupId", 0);
    set_int("colorMode", 0);

    if let Some(data) = marshal_object(env, info, "android/view/DisplayInfo") {
        install_reply(&data, "__pengudroid_binder_set_display_info");
    }
}

/// Install what the window session needs.
///
/// `ViewRootImpl.setView` calls `IWindowManager.openSession` and then
/// `IWindowSession.addToDisplayAsUser`. With no window manager the session is
/// null and `addView` throws "Adding window failed". The reply itself is built
/// in the shim (it carries a file descriptor, which `Parcel.marshall()` cannot
/// express); all the host has to supply is `InsetsState.SIZE`, because
/// `ViewRootImpl` passes an `InsetsSourceControl[SIZE]` as the
/// `out InsetsSourceControl[]` argument and the AIDL proxy insists on an exact
/// length match.
fn install_window_replies(env: &JniEnv) {
    let Some(insets_class) = env.find_class("android/view/InsetsState") else {
        eprintln!("apphost: InsetsState not found");
        return;
    };
    let Some(field) = env.static_field_id(insets_class, "SIZE", "I") else {
        eprintln!("apphost: InsetsState.SIZE not found");
        return;
    };
    let size = env.static_int_field(insets_class, field);
    println!("apphost: InsetsState.SIZE = {size}");
    if let Some(f) = shim_symbol("__pengudroid_binder_set_insets_size") {
        let f: unsafe extern "C" fn(i32) = unsafe { std::mem::transmute(f) };
        unsafe { f(size) };
    }
}

/// Resolve a symbol exported by the preloaded shim.
fn shim_symbol(name: &str) -> Option<*mut std::ffi::c_void> {
    let c = std::ffi::CString::new(name).ok()?;
    let p = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c.as_ptr()) };
    if p.is_null() {
        eprintln!("apphost: shim symbol {name} not found");
        None
    } else {
        Some(p)
    }
}

/// Fill `list` with a `ProviderInfo` for every `<provider>` in the manifest.
///
/// `ActivityThread.installContentProviders` walks this list and calls
/// `provider.attachInfo()` + `provider.onCreate()` for each entry, which is how
/// `androidx.startup.InitializationProvider` gets to run its initializers.
fn install_provider_list(env: &JniEnv, list: JObject, app_info: JObject, spec: &AppSpec<'_>) {
    if spec.providers.is_empty() {
        return;
    }
    let Some(list_class) = env.find_class("java/util/List") else {
        return;
    };
    let Some(add) = env.method_id(list_class, "add", "(Ljava/lang/Object;)Z") else {
        return;
    };
    let Some(pi_class) = env.find_class("android/content/pm/ProviderInfo") else {
        return;
    };
    let mut added = 0usize;
    for provider in spec.providers {
        let Some((_, info)) = new_instance(env, "android/content/pm/ProviderInfo") else {
            continue;
        };
        if let Some(id) = env.field_id(pi_class, "name", "Ljava/lang/String;") {
            env.set_object_field(info, id, env.new_string(&provider.name));
        }
        if let Some(id) = env.field_id(pi_class, "packageName", "Ljava/lang/String;") {
            env.set_object_field(info, id, env.new_string(spec.package));
        }
        if let Some(authority) = &provider.authority {
            if let Some(id) = env.field_id(pi_class, "authority", "Ljava/lang/String;") {
                env.set_object_field(info, id, env.new_string(authority));
            }
        }
        if let Some(id) = env.field_id(
            pi_class,
            "applicationInfo",
            "Landroid/content/pm/ApplicationInfo;",
        ) {
            env.set_object_field(info, id, app_info);
        }
        if let Some(id) = env.field_id(pi_class, "exported", "Z") {
            env.set_boolean_field(info, id, provider.exported);
        }
        if let Some(id) = env.field_id(pi_class, "enabled", "Z") {
            env.set_boolean_field(info, id, true);
        }
        if let Some(id) = env.field_id(pi_class, "grantUriPermissions", "Z") {
            env.set_boolean_field(info, id, provider.grant_uri_permissions);
        }
        if let Some(bundle) = build_meta_bundle(env, &provider.meta) {
            if let Some(id) = env.field_id(pi_class, "metaData", "Landroid/os/Bundle;") {
                env.set_object_field(info, id, bundle);
            }
        }
        // `androidx.startup.InitializationProvider.onCreate` asks the package
        // manager for its own `ProviderInfo` to read the initializer list out of
        // `metaData`; publish the same object under the class name.
        if let Some(bytes) = marshal_object(env, info, "android/content/pm/ProviderInfo") {
            install_provider_entry(&provider.name, &bytes);
        }
        if env.call_boolean(list, add, &[JValue { l: info }]) == Some(true) {
            added += 1;
        }
    }
    println!("apphost: queued {added} content providers for install");
}

/// Build a `Bundle` from a component's `<meta-data>`.
fn build_meta_bundle(
    env: &JniEnv,
    meta: &[(String, crate::apk::MetaValue)],
) -> Option<JObject> {
    if meta.is_empty() {
        return None;
    }
    let (bundle_class, bundle) = new_instance(env, "android/os/Bundle")?;
    let put_string = env.method_id(
        bundle_class,
        "putString",
        "(Ljava/lang/String;Ljava/lang/String;)V",
    );
    let put_int = env.method_id(bundle_class, "putInt", "(Ljava/lang/String;I)V");
    let put_bool = env.method_id(bundle_class, "putBoolean", "(Ljava/lang/String;Z)V");
    for (key, value) in meta {
        let k = env.new_string(key);
        match value {
            crate::apk::MetaValue::Str(s) => {
                if let Some(m) = put_string {
                    env.call_void(bundle, m, &[JValue { l: k }, JValue { l: env.new_string(s) }]);
                }
            }
            crate::apk::MetaValue::Int(i) => {
                if let Some(m) = put_int {
                    env.call_void(bundle, m, &[JValue { l: k }, JValue { i: *i }]);
                }
            }
            crate::apk::MetaValue::Bool(b) => {
                if let Some(m) = put_bool {
                    env.call_void(bundle, m, &[JValue { l: k }, JValue { z: *b as u8 }]);
                }
            }
            crate::apk::MetaValue::Ref(r) => {
                if let Some(m) = put_int {
                    env.call_void(bundle, m, &[JValue { l: k }, JValue { i: *r as i32 }]);
                }
            }
        }
    }
    Some(bundle)
}

/// Publish one marshalled `ProviderInfo` under its class name.
fn install_provider_entry(name: &str, bytes: &[u8]) {
    let Some(f) = shim_symbol("__pengudroid_binder_set_provider_entry") else {
        return;
    };
    let f: unsafe extern "C" fn(*const u8, usize, *const u8, usize) =
        unsafe { std::mem::transmute(f) };
    unsafe { f(name.as_ptr(), name.len(), bytes.as_ptr(), bytes.len()) };
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
        // `Settings.NameValueCache.getStringsForPrefix` casts
        // `Bundle.getSerializable("value")` to a `HashMap` and iterates it, so
        // an empty bundle NPEs inside `DeviceConfig`. Put an empty map there;
        // `getString("value")` still returns null (it swallows the cast).
        if let Some((_, map)) = new_instance(env, "java/util/HashMap") {
            let Some(bundle_class) = env.find_class("android/os/Bundle") else {
                return;
            };
            if let Some(put) = env.method_id(
                bundle_class,
                "putSerializable",
                "(Ljava/lang/String;Ljava/io/Serializable;)V",
            ) {
                let key = env.new_string("value");
                env.call_void(bundle, put, &[JValue { l: key }, JValue { l: map }]);
            }
        }
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
    // Static fields carry the transaction codes (`QUERY_TRANSACTION`, ...), so
    // print their values too: the names alone do not tell you what to answer.
    let f_get = env.method_id(field_cls, "get", "(Ljava/lang/Object;)Ljava/lang/Object;");
    let f_mods = env.method_id(field_cls, "getModifiers", "()I");
    // AIDL `*_TRANSACTION` constants are package-private, so reflection needs
    // the access check turned off before `Field.get` will hand back a value.
    let f_accessible = env.method_id(field_cls, "setAccessible", "(Z)V");
    let Some(string_cls) = env.find_class("java/lang/String") else {
        return;
    };
    let Some(to_string) = env.method_id(string_cls, "toString", "()Ljava/lang/String;") else {
        return;
    };
    let Some(integer_cls) = env.find_class("java/lang/Integer") else {
        return;
    };
    let Some(int_value) = env.method_id(integer_cls, "intValue", "()I") else {
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
            let mut value = String::new();
            let is_static = f_mods
                .and_then(|m| env.call_int(f, m, &[]))
                .is_some_and(|m| m & 0x0008 != 0);
            if is_static {
                if let Some(acc) = f_accessible {
                    env.call_void(f, acc, &[JValue { z: 1 }]);
                }
                if let Some(get) = f_get {
                    if let Some(v) = env.call_object(f, get, &[JValue { l: std::ptr::null_mut() }]) {
                        // Method ids are per-class, so pick the accessor that
                        // matches the field's declared type.
                        value = match ty.as_deref() {
                            Some("java.lang.String") => env
                                .call_object(v, to_string, &[])
                                .and_then(|s| env.get_string(s))
                                .unwrap_or_default(),
                            Some("int") | Some("short") | Some("byte") => env
                                .call_int(v, int_value, &[])
                                .map(|n| n.to_string())
                                .unwrap_or_default(),
                            _ => String::new(),
                        };
                    }
                }
            }
            println!(
                "  {} {}{}",
                ty.unwrap_or_default(),
                n.unwrap_or_default(),
                if value.is_empty() {
                    String::new()
                } else {
                    format!(" = {value}")
                }
            );
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
