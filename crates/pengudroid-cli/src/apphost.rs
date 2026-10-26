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

/// The PenguDroid surface geometry, in pixels.
///
/// This is the one place the size is written down. Everything that has to agree
/// about it reads these: the `DisplayInfo` the framework sees, the
/// `SurfaceControl` handed to `ViewRootImpl`, the window frame, and (through
/// `PENGUDROID_SURFACE_W`/`_H`, which `art_test` mirrors) the EGL surface the
/// gleshim renders into. Before this existed the defaults disagreed — portrait in
/// two places and landscape in two others — so hwui rendered 1280x720 into an
/// FBO the window frame described as 720x1280.
const SURFACE_W: i32 = 720;
const SURFACE_H: i32 = 1280;

/// The display density, in dpi.
///
/// Everything the framework lays out is scaled by this: `dp = px * 160 / dpi`.
/// At 320 dpi the surface is 360x640 dp, which is a conventional small phone.
/// It used to be 420 dpi while the dp fields were still the *landscape* 1280x720
/// values, so the app believed it had a 487x274 dp screen — and a Material
/// `AlertDialog`, whose minimum width is 280 dp, came out wider than the 274 dp
/// it thought it had and was clipped by the surface edge.
const DENSITY_DPI: i32 = 320;

/// The surface width in dp.
const SURFACE_W_DP: i32 = SURFACE_W * 160 / DENSITY_DPI;
/// The surface height in dp.
const SURFACE_H_DP: i32 = SURFACE_H * 160 / DENSITY_DPI;
use std::path::Path;

use crate::jni::{JClass, JFieldId, JMethodId, JObject, JValue, JniEnv};

/// Everything `handleBindApplication` needs to know about the app.
pub struct AppSpec<'a> {
    pub package: &'a str,
    pub apk: &'a Path,
    /// Additional APKs of a split bundle (`.apkm`/`.xapk` `config.<abi>` etc.).
    ///
    /// They are not just native libraries: a split can also carry dex. The app's
    /// `PathClassLoader` is built from `ApplicationInfo.sourceDir` plus
    /// `splitSourceDirs`, so without this a class that lives in the split is
    /// invisible — which is how Roblox died, with
    /// "ClassNotFoundException: com.roblox.client.gmasdk...GmaWebViewInitializerProvider"
    /// for a provider its own manifest declares.
    pub splits: &'a [std::path::PathBuf],
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
    // A split bundle's other APKs go into `splitSourceDirs`, which is what
    // `LoadedApk.createOrUpdateClassLoaderLocked` appends to the dex path. Without
    // this, every class that lives in a split is "not found" even though the file
    // is right there.
    if !spec.splits.is_empty() {
        let joined: Vec<String> = spec.splits.iter().map(|p| p.display().to_string()).collect();
        let text = joined.join(":");
        match env.field_id(class, "splitSourceDirs", "[Ljava/lang/String;") {
            Some(id) => match env.string_array(&joined) {
                Some(arr) => {
                    env.set_object_field(info, id, arr);
                    println!("apphost: ApplicationInfo.splitSourceDirs = {text}");
                }
                None => eprintln!("apphost: could not build splitSourceDirs"),
            },
            None => eprintln!("apphost: ApplicationInfo.splitSourceDirs not found"),
        }
        // `splitPublicSourceDirs` mirrors it for callers that only look at public
        // paths (package manager dumps, some asset loaders).
        if let Some(id) = env.field_id(class, "splitPublicSourceDirs", "[Ljava/lang/String;") {
            if let Some(arr) = env.string_array(&joined) {
                env.set_object_field(info, id, arr);
            }
        }
    }
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
    set_int("densityDpi", DENSITY_DPI);
    // Derived from the surface so the two can never disagree again.
    set_int("screenWidthDp", SURFACE_W_DP);
    set_int("screenHeightDp", SURFACE_H_DP);
    set_int("smallestScreenWidthDp", SURFACE_W_DP.min(SURFACE_H_DP));
    // WindowLayout.computeFrames uses Configuration.windowConfiguration bounds
    // for local relayout, not DisplayInfo or screenWidthDp. Empty bounds made a
    // later relayoutAsync collapse a previously valid activity window to 0x0.
    // Build the real window-manager configuration before Resources copies it.
    let window_class = env.find_class("android/app/WindowConfiguration")?;
    let field = env.field_id(class, "windowConfiguration", "Landroid/app/WindowConfiguration;")?;
    let window = env.object_field(config, field);
    if window.is_null() { return None; }
    let rect_class = env.find_class("android/graphics/Rect")?;
    let ctor = env.method_id(rect_class, "<init>", "(IIII)V")?;
    let coords = [JValue { i: 0 }, JValue { i: 0 }, JValue { i: SURFACE_W }, JValue { i: SURFACE_H }];
    let bounds = env.new_object(rect_class, ctor, &coords)?;
    if bounds.is_null() { return None; }
    let set_bounds = env.method_id(window_class, "setBounds", "(Landroid/graphics/Rect;)V")?;
    if !env.call_void(window, set_bounds, &[JValue { l: bounds }]) { return None; }
    for name in ["setAppBounds", "setMaxBounds"] {
        let set = env.method_id(window_class, name, "(IIII)V")?;
        if !env.call_void(window, set, &coords) { return None; }
    }
    let mode = env.method_id(window_class, "setWindowingMode", "(I)V")?;
    if !env.call_void(window, mode, &[JValue { i: 1 }]) { return None; } // FULLSCREEN
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
        env.set_int_field(cfg, id, DENSITY_DPI);
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
                        env.set_int_field(sys_cfg, id, DENSITY_DPI);
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

/// The package being hosted, for per-app host configuration.
///
/// [`bind_application`] is the only path that reaches [`loop_forever`], so the
/// input pump can read the keymap for this package without threading a
/// parameter through the JNI setup.
static APP_PACKAGE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Bind the app into `thread`, the object returned by
/// `ActivityThread.currentActivityThread()`.
pub fn bind_application(env: &JniEnv, thread: JObject, spec: &AppSpec<'_>) -> bool {
    let _ = APP_PACKAGE.set(spec.package.to_string());
    if install_virtual_keyboard(env).is_none() {
        eprintln!("apphost: cannot install the Android virtual keyboard");
        return false;
    }
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
    // `MediaSessionManager` resolves the `media_session` binder through
    // `MediaFrameworkPlatformInitializer`, whose `MediaServiceManager` is
    // installed by `system_server` during boot. Without it, the constructor of
    // `MediaSession` NPEs and NewPipe's `PlayerService.onCreate` dies with
    // "Unable to create service ... on a null object reference".
    install_platform_service_managers(env);
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
        if std::env::var_os("PENGUDROID_PROBE_MEDIA").is_some() {
            probe_media_codecs(env);
        }
        if std::env::var_os("PENGUDROID_PROBE_AUDIO").is_some() {
            probe_audio_track(env);
        }
        if std::env::var_os("PENGUDROID_TEXTURE_VIEW").is_some() {
            enable_texture_view(env, thread, spec.package);
        }
        if let Ok(name) = std::env::var("PENGUDROID_LAUNCH_ACTIVITY") {
            launch_activity(env, thread, data, spec, &name);
        }
    } else {
        eprintln!("apphost: handleBindApplication threw");
    }
    ok
}

/// Print the state of every `SurfaceView` in the window hierarchy.
///
/// `SurfaceView` only creates its `SurfaceControl`/`BLASTBufferQueue` once it has
/// a frame and a `ViewRootImpl`, and it bails silently otherwise. These are the
/// fields that decide it, so printing them says exactly which precondition is
/// missing instead of guessing from the absence of a log line.
fn surface_video_dimensions(env: &JniEnv, view: JObject) -> (i32, i32) {
    let configured = |name: &str| {
        std::env::var(name).ok().and_then(|v| v.trim().parse::<i32>().ok())
            .filter(|v| *v > 0)
    };
    if let (Some(w), Some(h)) = (configured("PENGUDROID_VIDEO_W"), configured("PENGUDROID_VIDEO_H")) {
        return (w, h);
    }

    // The root frame is refreshed by relayout/dispatchResized. Use it to adapt
    // the forced 16:9 video frame when Android rotates the window. During early
    // startup this can still be 0x0, so retain the documented portrait default.
    let root_size = (|| -> Option<(i32, i32)> {
        let view_class = env.find_class("android/view/View")?;
        let get_root = env.method_id(view_class, "getViewRootImpl", "()Landroid/view/ViewRootImpl;")?;
        let root = env.call_object(view, get_root, &[])?;
        if root.is_null() { return None; }
        let class = env.find_class("android/view/ViewRootImpl")?;
        let width = env.field_id(class, "mWidth", "I").map(|f| env.int_field(root, f))?;
        let height = env.field_id(class, "mHeight", "I").map(|f| env.int_field(root, f))?;
        (width > 0 && height > 0).then_some((width, height))
    })();
    let (container_w, container_h) = root_size.unwrap_or((720, 1280));
    let (mut w, mut h) = if container_w.saturating_mul(9) > container_h.saturating_mul(16) {
        (container_h.saturating_mul(16) / 9, container_h)
    } else {
        (container_w, container_w.saturating_mul(9) / 16)
    };
    if let Some(configured_w) = configured("PENGUDROID_VIDEO_W") { w = configured_w; }
    if let Some(configured_h) = configured("PENGUDROID_VIDEO_H") { h = configured_h; }
    (w.max(1), h.max(1))
}

/// Milliseconds on `CLOCK_MONOTONIC`, the same clock the binder trace stamps its
/// lines with. Probe output and relayout traces come from one process, so this
/// is what makes "which came first" answerable from a log.
fn monotonic_ms() -> u128 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid `timespec`.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u128 * 1000 + ts.tv_nsec as u128 / 1_000_000
}

/// Print the fields of a `SurfaceView` that decide whether the video layer
/// exists at all.
///
/// `SurfaceView` only creates its `BLASTBufferQueue` once it has a frame
/// (`mHaveFrame`), is visible, and its `ViewRootImpl` has a valid surface; it
/// bails silently otherwise, so these fields are the only way to tell *which*
/// precondition is missing.
fn dump_surface_fields(env: &JniEnv, view: JObject, class: JClass) {
    if class.is_null() {
        return;
    }
    for (field, sig) in [
        ("mHaveFrame", "Z"),
        ("mSurfaceControl", "Landroid/view/SurfaceControl;"),
        ("mBlastBufferQueue", "Landroid/graphics/BLASTBufferQueue;"),
        ("mSurface", "Landroid/view/Surface;"),
        ("mSurfaceCreated", "Z"),
        ("mVisible", "Z"),
        ("mRequestedVisible", "Z"),
        ("mSurfaceWidth", "I"),
        ("mSurfaceHeight", "I"),
        ("mFormat", "I"),
    ] {
        let Some(id) = env.field_id(class, field, sig) else {
            continue;
        };
        let value = match sig {
            "Z" => format!("{}", env.boolean_field(view, id)),
            "I" => format!("{}", env.int_field(view, id)),
            _ => {
                let o = env.object_field(view, id);
                if o.is_null() {
                    "null".to_string()
                } else if field == "mSurface" {
                    // The native pointer is the only thing that says *which*
                    // surface the app will hand the decoder.
                    let native = env
                        .find_class("android/view/Surface")
                        .and_then(|c| env.field_id(c, "mNativeObject", "J"))
                        .map(|f| env.long_field(o, f))
                        .unwrap_or(-1);
                    format!("set (native={native:#x})")
                } else {
                    "set".to_string()
                }
            }
        };
        println!("views:   {field} = {value}");
    }
    let w = env
        .method_id(class, "getWidth", "()I")
        .and_then(|m| env.call_int(view, m, &[]))
        .unwrap_or(-1);
    let h = env
        .method_id(class, "getHeight", "()I")
        .and_then(|m| env.call_int(view, m, &[]))
        .unwrap_or(-1);
    println!("views:   size = {w}x{h}");
}

/// Print the `SurfaceView` fields that decide whether the video layer exists,
/// once per call.
///
/// `PENGUDROID_TRACE_SURFACE=1` calls this every couple of seconds: the
/// interesting question is *when* `mSurface` becomes valid, and that is only
/// visible as a timeline (a single dump at the end cannot tell "the decoder got
/// a stale surface" from "the decoder got ours").
fn trace_surface_views(env: &JniEnv) {
    let Some(wmg_class) = env.find_class("android/view/WindowManagerGlobal") else {
        return;
    };
    let Some(get_instance) = env.static_method_id(
        wmg_class,
        "getInstance",
        "()Landroid/view/WindowManagerGlobal;",
    ) else {
        return;
    };
    let Some(wmg) = env.call_static_object(wmg_class, get_instance, &[]) else {
        return;
    };
    let Some(roots_field) = env.field_id(wmg_class, "mViews", "Ljava/util/ArrayList;") else {
        return;
    };
    let roots = env.object_field(wmg, roots_field);
    if roots.is_null() {
        return;
    }
    let Some(list_class) = env.find_class("java/util/ArrayList") else {
        return;
    };
    let (Some(size_m), Some(get_m)) = (
        env.method_id(list_class, "size", "()I"),
        env.method_id(list_class, "get", "(I)Ljava/lang/Object;"),
    ) else {
        return;
    };
    let Some(view_class) = env.find_class("android/view/View") else {
        return;
    };
    let Some(get_class) = env.method_id(view_class, "getClass", "()Ljava/lang/Class;") else {
        return;
    };
    let Some(class_class) = env.find_class("java/lang/Class") else {
        return;
    };
    let Some(get_name) = env.method_id(class_class, "getName", "()Ljava/lang/String;") else {
        return;
    };

    fn walk(
        env: &JniEnv,
        view: JObject,
        depth: usize,
        get_class: JMethodId,
        get_name: JMethodId,
    ) {
        if view.is_null() || depth > 24 {
            return;
        }
        let class = env.call_object(view, get_class, &[]).unwrap_or(std::ptr::null_mut());
        if class.is_null() {
            return;
        }
        let name = env
            .call_object(class, get_name, &[])
            .and_then(|s| env.get_string(s))
            .unwrap_or_default();
        if name.contains("SurfaceView") {
            let native_of = |field: &str| -> i64 {
                let Some(id) = env.field_id(class, field, "Landroid/view/Surface;") else {
                    return -1;
                };
                let obj = env.object_field(view, id);
                if obj.is_null() {
                    return 0;
                }
                let Some(sc) = env.find_class("android/view/Surface") else {
                    return -2;
                };
                let Some(f) = env.field_id(sc, "mNativeObject", "J") else {
                    return -3;
                };
                env.long_field(obj, f)
            };
            let queue = env
                .field_id(class, "mBlastBufferQueue", "Landroid/graphics/BLASTBufferQueue;")
                .map(|id| env.object_field(view, id))
                .unwrap_or(std::ptr::null_mut());
            let queue_native = if queue.is_null() {
                0
            } else {
                env.find_class("android/graphics/BLASTBufferQueue")
                    .and_then(|c| env.field_id(c, "mNativeObject", "J"))
                    .map(|f| env.long_field(queue, f))
                    .unwrap_or(-1)
            };
            let have_frame = env
                .field_id(class, "mHaveFrame", "Z")
                .map(|f| env.boolean_field(view, f))
                .unwrap_or(false);
            let w = env
                .method_id(class, "getWidth", "()I")
                .and_then(|m| env.call_int(view, m, &[]))
                .unwrap_or(-1);
            let h = env
                .method_id(class, "getHeight", "()I")
                .and_then(|m| env.call_int(view, m, &[]))
                .unwrap_or(-1);
            println!(
                "surface-trace: {name} {w}x{h} haveFrame={have_frame} surface={:#x} queue={queue_native:#x}",
                native_of("mSurface") as usize
            );
        }
        // `getChildCount` must not be looked up on the object's own class: a
        // `SurfaceView` is not a `ViewGroup`, and the failed lookup leaves a
        // pending `NoSuchMethodError` behind, after which every JNI call on this
        // thread silently returns null. `Class.isInstance` is the safe test.
        let Some(view_group) = env.find_class("android/view/ViewGroup") else {
            return;
        };
        let is_group = env
            .find_class("java/lang/Class")
            .and_then(|c| env.method_id(c, "isInstance", "(Ljava/lang/Object;)Z"))
            .map(|m| env.call_boolean(view_group, m, &[JValue { l: view }]) == Some(true))
            .unwrap_or(false);
        if !is_group {
            return;
        }
        let (Some(count_m), Some(get_child)) = (
            env.method_id(view_group, "getChildCount", "()I"),
            env.method_id(view_group, "getChildAt", "(I)Landroid/view/View;"),
        ) else {
            return;
        };
        let count = env.call_int(view, count_m, &[]).unwrap_or(0);
        for i in 0..count {
            if let Some(child) = env.call_object(view, get_child, &[JValue { i }]) {
                walk(env, child, depth + 1, get_class, get_name);
            }
        }
    }

    let root_count = env.call_int(roots, size_m, &[]).unwrap_or(0);
    for i in 0..root_count {
        env.describe_exception();
        if let Some(root) = env.call_object(roots, get_m, &[JValue { i }]) {
            walk(env, root, 0, get_class, get_name);
        }
    }
}

fn report_surface_views(env: &JniEnv, verbose: bool) {
    let Some(wmg_class) = env.find_class("android/view/WindowManagerGlobal") else {
        return;
    };
    let Some(get_instance) = env.static_method_id(
        wmg_class,
        "getInstance",
        "()Landroid/view/WindowManagerGlobal;",
    ) else {
        return;
    };
    let Some(wmg) = env.call_static_object(wmg_class, get_instance, &[]) else {
        return;
    };
    let Some(roots_field) = env.field_id(wmg_class, "mViews", "Ljava/util/ArrayList;") else {
        return;
    };
    let roots = env.object_field(wmg, roots_field);
    if roots.is_null() {
        return;
    }
    let Some(list_class) = env.find_class("java/util/ArrayList") else {
        return;
    };
    let (Some(size_m), Some(get_m)) = (
        env.method_id(list_class, "size", "()I"),
        env.method_id(list_class, "get", "(I)Ljava/lang/Object;"),
    ) else {
        return;
    };
    let Some(view_class) = env.find_class("android/view/View") else {
        return;
    };
    let Some(get_class) = env.method_id(view_class, "getClass", "()Ljava/lang/Class;") else {
        return;
    };
    let Some(class_class) = env.find_class("java/lang/Class") else {
        return;
    };
    let Some(get_name) = env.method_id(class_class, "getName", "()Ljava/lang/String;") else {
        return;
    };
    let Some(is_instance) = env.method_id(class_class, "isInstance", "(Ljava/lang/Object;)Z") else {
        return;
    };
    let Some(view_group) = env.find_class("android/view/ViewGroup") else {
        return;
    };
    let (Some(child_count), Some(get_child)) = (
        env.method_id(view_group, "getChildCount", "()I"),
        env.method_id(view_group, "getChildAt", "(I)Landroid/view/View;"),
    ) else {
        return;
    };

    // Depth-first walk; the player's SurfaceView is a few levels down.
    fn walk(
        env: &JniEnv,
        view: JObject,
        depth: usize,
        get_class: JMethodId,
        get_name: JMethodId,
        child_count: JMethodId,
        get_child: JMethodId,
        is_instance: JMethodId,
        view_group: JObject,
        done: &mut bool,
        verbose: bool,
    ) {
        if *done || view.is_null() || depth > 24 {
            return;
        }
        // A failed reflective lookup leaves an exception pending, and every JNI
        // call after that silently returns null — which is exactly how the walk
        // "stopped" at the first `ViewStub` (measured: the tree dump ended there
        // and the `SurfaceView` was never reached, so the forced layout never
        // ran).
        env.describe_exception();
        let name = env
            .call_object(view, get_class, &[])
            .and_then(|c| env.call_object(c, get_name, &[]))
            .and_then(|s| env.get_string(s))
            .unwrap_or_default();
        // The object's own `Class` is a valid `jclass`, so no class lookup is
        // needed (and `FindClass` would not see app classes anyway).
        let class = env.call_object(view, get_class, &[]).unwrap_or(std::ptr::null_mut());
        if verbose {
            println!("views: {:width$}{name}", "", width = depth * 2);
        }
        // Print a `TextView`'s text. A view tree only says *which* screen is
        // up; its text is what says *why* (an error dialog's message, the
        // player's state line). Without it a failure is just "a dialog".
        if !class.is_null() {
            if let Some(text_view) = env.find_class("android/widget/TextView") {
                let is_text_view = env
                    .find_class("java/lang/Class")
                    .and_then(|c| env.method_id(c, "isInstance", "(Ljava/lang/Object;)Z"))
                    .map(|m| env.call_boolean(text_view, m, &[JValue { l: view }]) == Some(true))
                    .unwrap_or(false);
                if is_text_view {
                    let text = env
                        .method_id(class, "getText", "()Ljava/lang/CharSequence;")
                        .and_then(|m| env.call_object(view, m, &[]))
                        .filter(|t| !t.is_null())
                        .and_then(|t| {
                            let tc = env.call_object(t, get_class, &[])?;
                            let to_string =
                                env.method_id(tc, "toString", "()Ljava/lang/String;")?;
                            env.call_object(t, to_string, &[])
                                .and_then(|s| env.get_string(s))
                        })
                        .unwrap_or_default();
                    let text = text.replace('\n', " ");
                    let text = text.trim();
                    if !text.is_empty() {
                        let shown: String = text.chars().take(160).collect();
                        println!("views: {:width$}text: {shown}", "", width = (depth + 1) * 2);
                    }
                }
            }
        }
        // Where the view actually sits. This matters twice: a synthetic tap is
        // a guess without it (the dialog window is real but its content is drawn
        // from an offset, so display coordinates and window coordinates are not
        // the same thing), and a `SurfaceView` that measures 0 is only
        // explainable by looking at its parents.
        {
            let view_class = env.find_class("android/view/View").unwrap_or(std::ptr::null_mut());
            let read = |method: &str, sig: &str| -> i32 {
                if view_class.is_null() {
                    return -1;
                }
                env.method_id(view_class, method, sig)
                    .and_then(|m| env.call_int(view, m, &[]))
                    .unwrap_or(-1)
            };
            let indent = (depth + 1) * 2;
            // The view id names the widget (`exo_content_frame`, `exo_shutter`,
            // `player_view`, ...) which is the difference between "a container
            // that is legitimately empty" and "the video surface's parent".
            let id = read("getId", "()I");
            let mut id_name = String::new();
            if id > 0 {
                let resources = env
                    .method_id(view_class, "getResources", "()Landroid/content/res/Resources;")
                    .and_then(|m| env.call_object(view, m, &[]))
                    .filter(|r| !r.is_null());
                if let (Some(resources), Some(res_class)) =
                    (resources, env.find_class("android/content/res/Resources"))
                {
                    if let Some(name) = env
                        .method_id(res_class, "getResourceName", "(I)Ljava/lang/String;")
                        .and_then(|m| env.call_object(resources, m, &[JValue { i: id }]))
                        .and_then(|s| env.get_string(s))
                    {
                        id_name = format!(" ({name})");
                    }
                    // An unknown id throws `NotFoundException`; leaving it
                    // pending would make every later JNI call on this thread
                    // return null.
                    env.describe_exception();
                }
            }
            // The declared layout size explains a 0×0 measurement: a
            // `wrap_content` parent with a 0×0 child collapses, and no amount of
            // forcing the child's requested size helps until the container is
            // given a size. `-1` is MATCH_PARENT, `-2` WRAP_CONTENT.
            let lp = env
                .method_id(
                    view_class,
                    "getLayoutParams",
                    "()Landroid/view/ViewGroup$LayoutParams;",
                )
                .and_then(|m| env.call_object(view, m, &[]))
                .filter(|o| !o.is_null());
            let (mut lp_w, mut lp_h) = (-999, -999);
            if let Some(lp) = lp {
                if let Some(lp_class) = env.find_class("android/view/ViewGroup$LayoutParams") {
                    if let Some(f) = env.field_id(lp_class, "width", "I") {
                        lp_w = env.int_field(lp, f);
                    }
                    if let Some(f) = env.field_id(lp_class, "height", "I") {
                        lp_h = env.int_field(lp, f);
                    }
                }
            }
            println!(
                "views: {:indent$}bounds [{},{}] {}x{} vis={} lp={}x{}{}",
                "",
                read("getLeft", "()I"),
                read("getTop", "()I"),
                read("getWidth", "()I"),
                read("getHeight", "()I"),
                read("getVisibility", "()I"),
                lp_w,
                lp_h,
                if id > 0 { format!(" id={id:#x}{id_name}") } else { String::new() },
            );
        }
        if name.contains("SurfaceView") || name.contains("TextureView") {
            // A `SurfaceView` that measured 0 wide never sets `mHaveFrame`, so
            // `updateSurface()` returns immediately and neither the
            // `SurfaceControl` nor the `BLASTBufferQueue` is ever created — which
            // is exactly what NewPipe hits.
            //
            // The fix must not call `updateSurface()` here: this walk runs on the
            // input-pump thread, and `updateSurface()` ends in
            // `SurfaceHolder.Callback.surfaceCreated`, where ExoPlayer asserts it
            // is on the main thread (measured: "Player is accessed on the wrong
            // thread", so the player kept its stale surface).
            //
            // `SurfaceHolder.setFixedSize` is the right lever instead: it makes
            // `SurfaceView.onMeasure` ignore the parent's zero-width spec, and
            // the framework's own `OnPreDrawListener` then sets `mHaveFrame` and
            // calls `updateSurface()` on the main thread.
            // Default on: a `SurfaceView` created after the activity window
            // collapsed measures 0 wide, so without this no video layer is ever
            // built. `PENGUDROID_NO_FORCE_SURFACE=1` turns it off for debugging.
            if std::env::var_os("PENGUDROID_NO_FORCE_SURFACE").is_none() && !class.is_null() {
                // `PENGUDROID_SURFACE_W`/`_H` are the *window* size (the CLI
                // exports them for the guest), so the video layer needs its own
                // pair.
                // Derive a 16:9 fit from the current root frame so the forced
                // SurfaceView size follows orientation relayout. Explicit video
                // dimensions remain available for controlled runs.
                let (w, h) = surface_video_dimensions(env, view);
                // `SurfaceView.onMeasure` prefers `mRequestedWidth`/`Height`
                // over the parent's spec, so writing those two fields is enough
                // to make the view measure non-zero. They are written directly
                // rather than through `SurfaceHolder.setFixedSize`, because that
                // calls `requestLayout()`, which throws
                // `CalledFromWrongThreadException` from this thread.
                let mut fixed = false;
                if let Some(f) = env.field_id(class, "mRequestedWidth", "I") {
                    env.set_int_field(view, f, w);
                    fixed = true;
                }
                if let Some(f) = env.field_id(class, "mRequestedHeight", "I") {
                    env.set_int_field(view, f, h);
                }
                // Ask for a real traversal instead of laying the view out here.
                // `View.layout()` does not check the thread, but calling it from
                // this thread races the main thread's own traversal and was
                // measured to SIGSEGV inside the walk (`art: helper killed by
                // signal 11`). `mLayoutRequested` is what
                // `ViewRootImpl.requestLayout()` sets before scheduling, and
                // `scheduleTraversals()` does not check the thread.
                let scheduled = env
                    .method_id(class, "getViewRootImpl", "()Landroid/view/ViewRootImpl;")
                    .and_then(|m| env.call_object(view, m, &[]))
                    .filter(|vri| !vri.is_null())
                    .and_then(|vri| {
                        let vri_class = env.find_class("android/view/ViewRootImpl")?;
                        // The window this `SurfaceView` lives in reports
                        // `mWidth = 0` (measured), so `performMeasure` would
                        // build a 0-wide spec and every view would measure to
                        // nothing. `mWidth`/`mHeight` are what
                        // `getRootMeasureSpec` uses, so writing them is what
                        // makes the whole tree lay out at the window size.
                        let (ww, wh) = (
                            env.field_id(vri_class, "mWidth", "I")
                                .map(|f| env.int_field(vri, f))
                                .unwrap_or(0),
                            env.field_id(vri_class, "mHeight", "I")
                                .map(|f| env.int_field(vri, f))
                                .unwrap_or(0),
                        );
                        if ww <= 0 || wh <= 0 {
                            if let Some(f) = env.field_id(vri_class, "mWidth", "I") {
                                env.set_int_field(vri, f, 720);
                            }
                            if let Some(f) = env.field_id(vri_class, "mHeight", "I") {
                                env.set_int_field(vri, f, 1280);
                            }
                            // NOTE: writing `mWinFrame` here as well was tried
                            // and SIGSEGVs — the main thread reads that `Rect`
                            // in `relayoutWindow` at the same time. `mWidth`
                            // alone does not stick either, because
                            // `performTraversals` derives `desiredWindowWidth`
                            // from `mWinFrame`. See `docs/STATUS.md`.
                            println!("views:   ViewRootImpl.mWidth {ww} -> 720");
                        }
                        // `layoutRequested` is `mLayoutRequested && (!mStopped ||
                        // mReportNextDraw)`, so a stopped window silently drops
                        // the request.
                        if let Some(f) = env.field_id(vri_class, "mStopped", "Z") {
                            env.set_boolean_field(vri, f, false);
                        }
                        if let Some(f) = env.field_id(vri_class, "mAppVisible", "Z") {
                            env.set_boolean_field(vri, f, true);
                        }
                        if let Some(f) = env.field_id(vri_class, "mLayoutRequested", "Z") {
                            env.set_boolean_field(vri, f, true);
                        }
                        // `relayoutWindow` compares the frames it computed
                        // locally against `mLastLayoutFrame` and takes the
                        // *asynchronous* path when they are equal — one-way, so
                        // the window manager has to call back `IWindow.resized`.
                        // Making the remembered frame differ forces the
                        // synchronous `relayout` on the next traversal, whose
                        // reply this broker already answers correctly.
                        if let Some(f) =
                            env.field_id(vri_class, "mLastLayoutFrame", "Landroid/graphics/Rect;")
                        {
                            let rect = env.object_field(vri, f);
                            if !rect.is_null() {
                                if let Some(rect_class) = env.find_class("android/graphics/Rect") {
                                    if let Some(set) =
                                        env.method_id(rect_class, "set", "(IIII)V")
                                    {
                                        env.call_void(
                                            rect,
                                            set,
                                            &[
                                                JValue { i: -1 },
                                                JValue { i: -1 },
                                                JValue { i: 0 },
                                                JValue { i: 0 },
                                            ],
                                        );
                                    }
                                }
                            }
                        }
                        let m = env.method_id(vri_class, "scheduleTraversals", "()V")?;
                        env.call_void(vri, m, &[]).then_some(())
                    })
                    .is_some();
                // `performDraw` only dispatches the pre-draw listeners when the
                // dirty region is non-empty, and `SurfaceView`'s own pre-draw
                // listener is what sets `mHaveFrame` and calls
                // `updateSurface()` — on the main thread, which is what ExoPlayer
                // requires. `postInvalidate()` is the one invalidation entry
                // point that does not call `checkThread()`: it posts a message
                // to the main thread's handler instead.
                let invalidated = env
                    .method_id(class, "postInvalidate", "()V")
                    .map(|m| env.call_void(view, m, &[]))
                    .unwrap_or(false);
                if fixed {
                    if verbose {
                        println!(
                            "views:   requested {w}x{h} (traversal: {scheduled}, postInvalidate: {invalidated})"
                        );
                    }
                    // Read the state *now*: the next traversal runs on the main
                    // thread, and walking the tree while it does was measured to
                    // SIGSEGV. Reading fields is safe.
                    dump_surface_fields(env, view, class);
                    // Stop here: the main thread is about to traverse.
                    *done = true;
                    return;
                } else {
                    eprintln!("views:   mRequestedWidth not found");
                }
            }
            dump_surface_fields(env, view, class);
        }
        // `ViewGroup.getChildCount` must not be invoked on a plain `View`: JNI
        // does not check the receiver's class and ART faults on the mismatch.
        // Looking the method up on the object's own class avoided the fault but
        // left a pending `NoSuchMethodError` behind, which silently truncated
        // the walk at the first `ViewStub` — the `SurfaceView` was never
        // reached. `Class.isInstance` is the safe test and leaves no exception.
        if env.call_boolean(view_group, is_instance, &[JValue { l: view }]) != Some(true) {
            return;
        }
        let count = env.call_int(view, child_count, &[]).unwrap_or(0);
        for i in 0..count {
            if *done {
                return;
            }
            if verbose {
                println!("views: {:width$}child {i}/{count}", "", width = depth * 2);
            }
            if let Some(child) = env.call_object(view, get_child, &[JValue { i }]) {
                walk(env, child, depth + 1, get_class, get_name, child_count, get_child, is_instance, view_group, done, verbose);
            }
        }
    }

    let root_count = env.call_int(roots, size_m, &[]).unwrap_or(0);
    println!(
        "views: {root_count} window root(s) at +{} ms",
        monotonic_ms()
    );
    for i in 0..root_count {
        // A failed reflective lookup leaves an exception pending, and every JNI
        // call after that silently returns null — which is exactly how a walk
        // "stops" halfway through a tree.
        env.describe_exception();
        if let Some(root) = env.call_object(roots, get_m, &[JValue { i }]) {
            dump_root_state(env, root, i);
            let mut done = false;
            walk(env, root, 0, get_class, get_name, child_count, get_child, is_instance, view_group, &mut done, verbose);
            if done {
                break;
            }
        }
    }
}

/// Print the `ViewRootImpl` fields that decide whether a layout pass happens.
///
/// A `DecorView` that measures 0×0 means `performLayout` never ran (or ran with
/// `mWidth == 0`), and the reason is always one of these fields.
fn dump_root_state(env: &JniEnv, root: JObject, index: i32) {
    let Some(vri_class) = env.find_class("android/view/ViewRootImpl") else {
        return;
    };
    let Some(view_class) = env.find_class("android/view/View") else {
        return;
    };
    let Some(get_vri) = env.method_id(view_class, "getViewRootImpl", "()Landroid/view/ViewRootImpl;") else {
        return;
    };
    let Some(vri) = env.call_object(root, get_vri, &[]).filter(|v| !v.is_null()) else {
        println!("views: root {index} has no ViewRootImpl");
        return;
    };
    let mut parts = Vec::new();
    for (field, sig) in [
        ("mWidth", "I"),
        ("mHeight", "I"),
        // `mRelayoutSeq` is bumped every time `relayoutWindow` is about to run,
        // so a window whose frame stayed empty is either a window whose relayout
        // reply was not applied (`mRelayoutSeq > 0`) or one that never asked at
        // all (`== 0`). The other two decide whether a relayout is still due.
        ("mRelayoutSeq", "I"),
        ("mRelayoutRequested", "Z"),
        ("mForceNextWindowRelayout", "Z"),
        ("mAdded", "Z"),
        ("mStopped", "Z"),
        ("mAppVisible", "Z"),
        ("mLayoutRequested", "Z"),
        ("mFirst", "Z"),
        ("mTraversalScheduled", "Z"),
        ("mIsInTraversal", "Z"),
    ] {
        let Some(id) = env.field_id(vri_class, field, sig) else {
            continue;
        };
        let value = if sig == "Z" {
            format!("{}", env.boolean_field(vri, id))
        } else {
            format!("{}", env.int_field(vri, id))
        };
        parts.push(format!("{field}={value}"));
    }
    // `mWidth`/`mHeight` are copied from `mWinFrame`, so when they are 0 the
    // question is whether the relayout reply reached the window at all.
    if let Some(id) = env.field_id(vri_class, "mWinFrame", "Landroid/graphics/Rect;") {
        let rect = env.object_field(vri, id);
        if !rect.is_null() {
            if let Some(rect_class) = env.find_class("android/graphics/Rect") {
                let read = |name: &str| {
                    env.field_id(rect_class, name, "I")
                        .map(|f| env.int_field(rect, f))
                        .unwrap_or(-9999)
                };
                parts.push(format!(
                    "mWinFrame=({},{})-({},{})",
                    read("left"),
                    read("top"),
                    read("right"),
                    read("bottom")
                ));
            }
        }
    }
    println!("views: root {index} {}", parts.join(" "));
}

/// Tap the centre of the first `TextView` whose text contains `needle`.
///
/// Blind coordinates are fragile: a feed scrolls, rows move when more items load,
/// and a dialog can eat the first tap. Finding the view by its text and using its
/// *global* rectangle makes a navigation step deterministic — the "click item N"
/// problem stops depending on how many rows the server returned.
fn click_text(env: &JniEnv, needle: &str, inject: &dyn Fn(i32, f32, f32, i64)) -> bool {
    let Some(wmg_class) = env.find_class("android/view/WindowManagerGlobal") else {
        return false;
    };
    let (Some(get_instance), Some(roots_field)) = (
        env.static_method_id(wmg_class, "getInstance", "()Landroid/view/WindowManagerGlobal;"),
        env.field_id(wmg_class, "mViews", "Ljava/util/ArrayList;"),
    ) else {
        return false;
    };
    let Some(wmg) = env.call_static_object(wmg_class, get_instance, &[]) else {
        return false;
    };
    let roots = env.object_field(wmg, roots_field);
    if roots.is_null() {
        return false;
    }
    let Some(list_class) = env.find_class("java/util/ArrayList") else {
        return false;
    };
    let (Some(size_m), Some(get_m)) = (
        env.method_id(list_class, "size", "()I"),
        env.method_id(list_class, "get", "(I)Ljava/lang/Object;"),
    ) else {
        return false;
    };
    let Some(view_class) = env.find_class("android/view/View") else {
        return false;
    };
    let Some(get_class) = env.method_id(view_class, "getClass", "()Ljava/lang/Class;") else {
        return false;
    };
    let Some(class_class) = env.find_class("java/lang/Class") else {
        return false;
    };
    let (Some(get_name), Some(is_instance)) = (
        env.method_id(class_class, "getName", "()Ljava/lang/String;"),
        env.method_id(class_class, "isInstance", "(Ljava/lang/Object;)Z"),
    ) else {
        return false;
    };
    let Some(text_view) = env.find_class("android/widget/TextView") else {
        return false;
    };
    let Some(view_group) = env.find_class("android/view/ViewGroup") else {
        return false;
    };
    let (Some(get_text), Some(global_rect)) = (
        env.method_id(text_view, "getText", "()Ljava/lang/CharSequence;"),
        env.method_id(view_class, "getGlobalVisibleRect", "(Landroid/graphics/Rect;)Z"),
    ) else {
        return false;
    };
    let Some(rect_class) = env.find_class("android/graphics/Rect") else {
        return false;
    };
    let Some(rect_ctor) = env.method_id(rect_class, "<init>", "()V") else {
        return false;
    };
    let Some(clock_class) = env.find_class("android/os/SystemClock") else {
        return false;
    };
    let Some(uptime) = env.static_method_id(clock_class, "uptimeMillis", "()J") else {
        return false;
    };

    // Returns the global rect of the first matching `TextView`, or `None`.
    fn find(
        env: &JniEnv,
        view: JObject,
        needle: &str,
        depth: usize,
        text_view: JClass,
        view_group: JClass,
        view_class: JClass,
        get_class: JMethodId,
        get_name: JMethodId,
        is_instance: JMethodId,
        get_text: JMethodId,
        global_rect: JMethodId,
        rect_class: JClass,
        rect_ctor: JMethodId,
    ) -> Option<(i32, i32, i32, i32)> {
        if view.is_null() || depth > 24 {
            return None;
        }
        env.describe_exception();
        let class = env.call_object(view, get_class, &[]).unwrap_or(std::ptr::null_mut());
        if class.is_null() {
            return None;
        }
        if env.call_boolean(text_view, is_instance, &[JValue { l: view }]) == Some(true) {
            let text = env
                .call_object(view, get_text, &[])
                .filter(|t| !t.is_null())
                .and_then(|t| {
                    let tc = env.call_object(t, get_class, &[])?;
                    let to_string = env.method_id(tc, "toString", "()Ljava/lang/String;")?;
                    env.call_object(t, to_string, &[]).and_then(|s| env.get_string(s))
                })
                .unwrap_or_default();
            if text.contains(needle) {
                // `getGlobalVisibleRect` clips to the visible part, which is
                // exactly what a tap can reach.
                if let Some(rect) = env.new_object(rect_class, rect_ctor, &[]) {
                    if env.call_boolean(view, global_rect, &[JValue { l: rect }]) == Some(true) {
                        let read = |name: &str| {
                            env.field_id(rect_class, name, "I")
                                .map(|f| env.int_field(rect, f))
                                .unwrap_or(0)
                        };
                        return Some((read("left"), read("top"), read("right"), read("bottom")));
                    }
                }
            }
        }
        if env.call_boolean(view_group, is_instance, &[JValue { l: view }]) != Some(true) {
            return None;
        }
        let (Some(count_m), Some(get_child)) = (
            env.method_id(view_group, "getChildCount", "()I"),
            env.method_id(view_group, "getChildAt", "(I)Landroid/view/View;"),
        ) else {
            return None;
        };
        let count = env.call_int(view, count_m, &[]).unwrap_or(0);
        for i in 0..count {
            if let Some(child) = env.call_object(view, get_child, &[JValue { i }]) {
                if let Some(found) = find(
                    env, child, needle, depth + 1, text_view, view_group, view_class, get_class,
                    get_name, is_instance, get_text, global_rect, rect_class, rect_ctor,
                ) {
                    return Some(found);
                }
            }
        }
        None
    }

    let count = env.call_int(roots, size_m, &[]).unwrap_or(0);
    for i in 0..count {
        let Some(root) = env.call_object(roots, get_m, &[JValue { i }]) else {
            continue;
        };
        if let Some((left, top, right, bottom)) = find(
            env, root, needle, 0, text_view, view_group, view_class, get_class, get_name,
            is_instance, get_text, global_rect, rect_class, rect_ctor,
        ) {
            let x = (left + right) as f32 / 2.0;
            let y = (top + bottom) as f32 / 2.0;
            let now = env.call_static_long(clock_class, uptime, &[]).unwrap_or(0);
            inject(0, x, y, now); // ACTION_DOWN
            std::thread::sleep(std::time::Duration::from_millis(80));
            inject(1, x, y, now); // ACTION_UP
            println!(
                "apphost: click-text {:?} at {x},{y} (rect {left},{top}-{right},{bottom})"
                , needle
            );
            return true;
        }
    }
    false
}

/// `PENGUDROID_PUMP_RESIZED=1`: run the window-manager `resized` pump on its own
/// thread.
///
/// It cannot live in the input-pump loop: that loop is entered only after the
/// probe's sleeps, so a window that collapses while the player opens would stay
/// collapsed through the whole probe window. A separate thread also keeps the
/// pump running while the main thread is inside a traversal it is waiting on.
/// How many times the `resized` pump nudges one window before giving up.
const MAX_NUDGES: u32 = 3;

/// How many times the forced-surface probe repeats, so a slow first traversal
/// still ends with a measured `SurfaceView`.
const MAX_FORCE_PASSES: u32 = 4;

fn spawn_resized_pump(env: &JniEnv) {
    if std::env::var_os("PENGUDROID_PUMP_RESIZED").is_none() {
        return;
    }
    let Some(vm) = env.get_java_vm() else { return };
    let vm_addr = vm as usize;
    std::thread::spawn(move || {
        let vm = vm_addr as *mut c_void;
        let Some(env) = attach_current_thread(vm) else {
            return;
        };
        let mut seen: std::collections::HashMap<usize, (i32, u32)> =
            std::collections::HashMap::new();
        loop {
            pump_resized(&env, &mut seen);
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    });
}

/// `PENGUDROID_PUMP_RESIZED=1`: play the window manager's `IWindow.resized`.
///
/// An Android 13 window takes the `relayoutAsync` path whenever the frames it
/// computed locally did not change. The window manager is then supposed to call
/// `IWindow.resized` with the final frames; without it the client's
/// `mRelayoutSeq` moves on and the window keeps whatever `mWinFrame` it computed
/// from its *last measured size*. When that size is 0 the window is stuck: 0
/// measures to 0, which relayouts to a 0 frame, which measures to 0.
///
/// Replaying the callback through the emulated binder does not work in this
/// tree (the app's `IWindow.Stub.onTransact` answers `UNKNOWN_TRANSACTION`), so
/// the host plays the window manager directly: whenever `mRelayoutSeq` moves, it
/// calls `ViewRootImpl.dispatchResized` with the real surface frames. That is the
/// same nudge `force_window_frame` makes, through the framework's own entry point
/// and therefore on the main thread's message queue.
fn pump_resized(env: &JniEnv, seen: &mut std::collections::HashMap<usize, (i32, u32)>) {
    let Some(wmg_class) = env.find_class("android/view/WindowManagerGlobal") else {
        return;
    };
    let (Some(get_instance), Some(views_field)) = (
        env.static_method_id(wmg_class, "getInstance", "()Landroid/view/WindowManagerGlobal;"),
        env.field_id(wmg_class, "mViews", "Ljava/util/ArrayList;"),
    ) else {
        return;
    };
    let Some(wmg) = env.call_static_object(wmg_class, get_instance, &[]) else {
        return;
    };
    let views = env.object_field(wmg, views_field);
    if views.is_null() {
        return;
    }
    let Some(list_class) = env.find_class("java/util/ArrayList") else {
        return;
    };
    let (Some(size_m), Some(get_m)) = (
        env.method_id(list_class, "size", "()I"),
        env.method_id(list_class, "get", "(I)Ljava/lang/Object;"),
    ) else {
        return;
    };
    let Some(view_class) = env.find_class("android/view/View") else {
        return;
    };
    let Some(get_root) = env.method_id(view_class, "getViewRootImpl", "()Landroid/view/ViewRootImpl;")
    else {
        return;
    };
    let Some(vri_class) = env.find_class("android/view/ViewRootImpl") else {
        return;
    };
    let Some(seq_field) = env.field_id(vri_class, "mRelayoutSeq", "I") else {
        return;
    };
    let Some(merged_field) = env.field_id(
        vri_class,
        "mLastReportedMergedConfiguration",
        "Landroid/util/MergedConfiguration;",
    ) else {
        return;
    };
    let Some(dispatch) = env.method_id(
        vri_class,
        "dispatchResized",
        "(Landroid/window/ClientWindowFrames;ZLandroid/util/MergedConfiguration;Landroid/view/InsetsState;ZZIII)V",
    ) else {
        eprintln!("apphost: resized pump: ViewRootImpl.dispatchResized not found");
        return;
    };
    let count = env.call_int(views, size_m, &[]).unwrap_or(0);
    for index in 0..count {
        env.describe_exception();
        let Some(view) = env.call_object(views, get_m, &[JValue { i: index as i32 }]) else {
            continue;
        };
        if view.is_null() {
            continue;
        }
        let Some(root) = env.call_object(view, get_root, &[]) else {
            continue;
        };
        if root.is_null() {
            continue;
        }
        let seq = env.int_field(root, seq_field);
        // A nudge makes the client relayout, which bumps `mRelayoutSeq`, which
        // would nudge again — measured as an endless `dispatchResized` stream
        // every 250 ms. Cap it so a window that refuses to take the frame does
        // not spin forever.
        // `-1` so the first sight of a window always counts as a change.
        let (previous, nudges) = seen
            .get(&(index as usize))
            .copied()
            .unwrap_or((-1, 0));
        if previous == seq || nudges >= MAX_NUDGES {
            continue;
        }
        seen.insert(index as usize, (seq, nudges + 1));
        // A window at width 0 is self-sustaining: `relayoutWindow` sends
        // `mMeasuredWidth` as the requested size, the local frames come back 0,
        // and the next measure is 0 again. Give the traversal the real size to
        // measure with, then hand it the frames the window manager would.
        if env
            .field_id(vri_class, "mWidth", "I")
            .map(|f| env.int_field(root, f))
            .unwrap_or(0)
            > 0
        {
            continue;
        }
        // Force the *synchronous* relayout path instead of writing frames.
        //
        // `relayoutAsync` (oneway) means the client keeps the frames it computed
        // locally, and those come from `mMeasuredWidth`, which is 0 once the
        // window has collapsed. `mForceNextWindowRelayout` makes
        // `performTraversals` call `IWindowSession.relayout` (code 5), whose
        // reply this host fills with the real 720x1280 frames — so the client
        // reads a correct frame without us touching any `Rect`.
        if let Some(f) = env.field_id(vri_class, "mForceNextWindowRelayout", "Z") {
            env.set_boolean_field(root, f, true);
        }
        if let Some(f) = env.field_id(vri_class, "mLayoutRequested", "Z") {
            env.set_boolean_field(root, f, true);
        }
        let ok = env
            .method_id(vri_class, "scheduleTraversals", "()V")
            .map(|m| env.call_void(root, m, &[]))
            .unwrap_or(false);
        let _ = (&dispatch, &merged_field, &seq_field);
        println!(
            "apphost: nudge relayout root {index} seq={seq} ok={ok} at +{} ms",
            monotonic_ms()
        );
    }
}

/// `PENGUDROID_TRACE_ROOTS=<ms>`: a timeline of the window roots.
///
/// The one-shot probe says where the state *ended up*; a timeline says *when* it
/// changed, which is what separates "the reply never landed" from "the frame was
/// reset afterwards". It runs on the input-pump thread and only reads fields, so
/// it does not race the main thread's layout the way calling `layout()` did.
fn trace_roots(env: &JniEnv) {
    let Some(wmg_class) = env.find_class("android/view/WindowManagerGlobal") else {
        return;
    };
    let (Some(get_instance), Some(roots_field)) = (
        env.static_method_id(wmg_class, "getInstance", "()Landroid/view/WindowManagerGlobal;"),
        // `mViews` holds the decor views (one per window); `mRoots` holds the
        // `ViewRootImpl`s. The state lives on the `ViewRootImpl`, and the decor
        // view reaches it through `getViewRootImpl()`.
        env.field_id(wmg_class, "mViews", "Ljava/util/ArrayList;"),
    ) else {
        return;
    };
    let Some(wmg) = env.call_static_object(wmg_class, get_instance, &[]) else {
        return;
    };
    let roots = env.object_field(wmg, roots_field);
    if roots.is_null() {
        return;
    }
    let Some(list_class) = env.find_class("java/util/ArrayList") else {
        return;
    };
    let (Some(size_m), Some(get_m)) = (
        env.method_id(list_class, "size", "()I"),
        env.method_id(list_class, "get", "(I)Ljava/lang/Object;"),
    ) else {
        return;
    };
    let count = env.call_int(roots, size_m, &[]).unwrap_or(0);
    // The identity of the decor view is what tells "the same window lost its
    // frame" apart from "a different window replaced it".
    let identity = {
        let system = env.find_class("java/lang/System");
        let method = system.and_then(|c| {
            env.static_method_id(c, "identityHashCode", "(Ljava/lang/Object;)I")
        });
        match (system, method) {
            (Some(class), Some(method)) => env
                .call_object(roots, get_m, &[JValue { i: 0 }])
                .filter(|view| !view.is_null())
                .and_then(|view| env.call_static_int(class, method, &[JValue { l: view }]))
                .unwrap_or(0),
            _ => 0,
        }
    };
    println!(
        "roots: {count} at +{} ms (view0={identity:#x})",
        monotonic_ms()
    );
    for i in 0..count {
        env.describe_exception();
        if let Some(root) = env.call_object(roots, get_m, &[JValue { i }]) {
            dump_root_state(env, root, i);
        }
    }
}

/// Turn on the app's "use `TextureView` for video" preference.
///
/// `SurfaceView` needs a `BLASTBufferQueue`, which is a `SurfaceFlinger` client —
/// there is no SurfaceFlinger here, so the player's surface is never created and
/// decoded frames have nowhere to go. `TextureView` instead hands the frames to
/// hwui as a GL texture, so the app's own renderer composites the video into the
/// window we already present. NewPipe exposes exactly this as a user setting
/// ("Use TextureView"), stored in the default preferences file under
/// `texture_view`; this flips it before the player is built.
fn enable_texture_view(env: &JniEnv, thread: JObject, package: &str) {
    let Some(thread_class) = env.find_class("android/app/ActivityThread") else {
        return;
    };
    let Some(field) = env.field_id(
        thread_class,
        "mInitialApplication",
        "Landroid/app/Application;",
    ) else {
        eprintln!("apphost: ActivityThread.mInitialApplication not found");
        return;
    };
    let app = env.object_field(thread, field);
    if app.is_null() {
        eprintln!("apphost: no Application to set preferences on");
        return;
    }
    let Some(ctx_class) = env.find_class("android/content/Context") else {
        return;
    };
    let Some(get_prefs) = env.method_id(
        ctx_class,
        "getSharedPreferences",
        "(Ljava/lang/String;I)Landroid/content/SharedPreferences;",
    ) else {
        eprintln!("apphost: Context.getSharedPreferences missing");
        return;
    };
    // `PreferenceManager.getDefaultSharedPreferencesName(context)` is
    // `<package>_preferences`; using the name directly avoids loading androidx
    // classes through the app's class loader.
    let name = env.new_string(&format!("{package}_preferences"));
    let Some(prefs) = env.call_object(app, get_prefs, &[JValue { l: name }, JValue { i: 0 }])
    else {
        eprintln!("apphost: getSharedPreferences threw");
        return;
    };
    let Some(sp_class) = env.find_class("android/content/SharedPreferences") else {
        return;
    };
    let Some(edit) = env.method_id(
        sp_class,
        "edit",
        "()Landroid/content/SharedPreferences$Editor;",
    ) else {
        return;
    };
    let Some(editor) = env.call_object(prefs, edit, &[]) else {
        eprintln!("apphost: SharedPreferences.edit threw");
        return;
    };
    let Some(editor_class) = env.find_class("android/content/SharedPreferences$Editor") else {
        return;
    };
    let Some(put) = env.method_id(
        editor_class,
        "putBoolean",
        "(Ljava/lang/String;Z)Landroid/content/SharedPreferences$Editor;",
    ) else {
        return;
    };
    let key = env.new_string("texture_view");
    env.call_object(editor, put, &[JValue { l: key }, JValue { i: 1 }]);
    let Some(commit) = env.method_id(editor_class, "commit", "()Z") else {
        return;
    };
    let ok = env.call_boolean(editor, commit, &[]);
    println!("apphost: texture_view preference committed = {ok:?}");
}

/// `PENGUDROID_PROBE_AUDIO=1`: build an `AudioTrack`, play half a second of PCM
/// and report whether the playback head advanced.
///
/// This is the audio path end to end without an app: `AudioTrack`'s Java side
/// (constructor, `native_setup`, buffer sizes), the shim's JNI host
/// (`crates/pengudroid-shim/src/audiotrack.rs`) and the `pw-cat` sink behind
/// it. `getPlaybackHeadPosition` only moves when the sink really consumes the
/// PCM, so a non-zero delta is proof that samples left the process.
fn probe_audio_track(env: &JniEnv) {
    let Some(track_class) = env.find_class("android/media/AudioTrack") else {
        eprintln!("audio: AudioTrack not found");
        return;
    };
    let rate = 44_100i32;
    let mono = 0x4i32; // AudioFormat.CHANNEL_OUT_MONO
    let pcm16 = 2i32; // AudioFormat.ENCODING_PCM_16BIT
    let buffer_bytes = rate * 2; // one second of mono s16
    let Some(ctor) = env.method_id(
        track_class,
        "<init>",
        "(IIIIII)V",
    ) else {
        eprintln!("audio: AudioTrack(int,int,int,int,int,int) constructor missing");
        return;
    };
    // The deprecated stream-type constructor is the shortest path to a live
    // track; it builds the same AudioAttributes the `Builder` would.
    let Some(track) = env.new_object(
        track_class,
        ctor,
        &[
            JValue { i: 3 }, // STREAM_MUSIC
            JValue { i: rate },
            JValue { i: mono },
            JValue { i: pcm16 },
            JValue { i: buffer_bytes },
            JValue { i: 1 }, // MODE_STREAM
        ],
    ) else {
        eprintln!("audio: new AudioTrack threw");
        env.describe_exception();
        return;
    };
    let (Some(get_state), Some(play), Some(write), Some(position)) = (
        env.method_id(track_class, "getState", "()I"),
        env.method_id(track_class, "play", "()V"),
        env.method_id(track_class, "write", "([BII)I"),
        env.method_id(track_class, "getPlaybackHeadPosition", "()I"),
    ) else {
        eprintln!("audio: AudioTrack methods missing");
        return;
    };
    println!(
        "audio: state={:?} (1 = STATE_INITIALIZED)",
        env.call_int(track, get_state, &[])
    );
    if !env.call_void(track, play, &[]) {
        eprintln!("audio: play() threw");
        env.describe_exception();
        return;
    }
    // Half a second of a 440 Hz sine, in s16 mono.
    let frames = rate / 2;
    let mut pcm = Vec::with_capacity(frames as usize * 2);
    for n in 0..frames {
        let t = n as f32 / rate as f32;
        let sample = ((t * 440.0 * std::f32::consts::TAU).sin() * 12_000.0) as i16;
        pcm.extend_from_slice(&sample.to_le_bytes());
    }
    let Some(array) = env.new_byte_array(pcm.len() as i32) else {
        eprintln!("audio: cannot allocate the PCM array");
        return;
    };
    env.set_byte_array_region(array, 0, &pcm);
    let written = env.call_int(
        track,
        write,
        &[JValue { l: array }, JValue { i: 0 }, JValue { i: pcm.len() as i32 }],
    );
    println!("audio: write() accepted {written:?} of {} bytes", pcm.len());
    let first = env.call_int(track, position, &[]).unwrap_or(-1);
    std::thread::sleep(std::time::Duration::from_millis(400));
    let second = env.call_int(track, position, &[]).unwrap_or(-1);
    println!(
        "audio: playback head {first} -> {second} frames ({} advanced)",
        second - first
    );
    let Some(stop) = env.method_id(track_class, "stop", "()V") else {
        return;
    };
    env.call_void(track, stop, &[]);
    if let Some(release) = env.method_id(track_class, "release", "()V") {
        env.call_void(track, release, &[]);
    }
}

/// `PENGUDROID_PROBE_MEDIA=1`: ask the framework how many codecs it sees.
///
/// `MediaCodecList` reaches the codec service through `libmedia_jni` →
/// `Codec2Client` → HIDL, so this is the cheapest way to tell whether the
/// codec2 plumbing is in place.
fn probe_media_codecs(env: &JniEnv) {
    let Some(list_class) = env.find_class("android/media/MediaCodecList") else {
        eprintln!("media: MediaCodecList not found");
        return;
    };
    if let Some(count) = env.static_method_id(list_class, "getCodecCount", "()I") {
        match env.call_static_int(list_class, count, &[]) {
            Some(n) => println!("media: MediaCodecList.getCodecCount() = {n}"),
            None => eprintln!("media: getCodecCount threw"),
        }
    }
    // `getCodecInfos()` is an *instance* method (`new MediaCodecList(kind)`),
    // not a static one; asking for it as static throws `NoSuchMethodError`.
    let Some(ctor) = env.method_id(list_class, "<init>", "(I)V") else {
        eprintln!("media: MediaCodecList(int) constructor missing");
        return;
    };
    let Some(list) = env.new_object(list_class, ctor, &[JValue { i: 1 }]) else {
        eprintln!("media: new MediaCodecList(ALL_CODECS) threw");
        return;
    };
    if let Some(infos) = env.method_id(
        list_class,
        "getCodecInfos",
        "()[Landroid/media/MediaCodecInfo;",
    ) {
        if let Some(array) = env.call_object(list, infos, &[]) {
            if !array.is_null() {
                let len = env.array_length(array);
                println!("media: getCodecInfos() returned {len}");
                let Some(info_class) = env.find_class("android/media/MediaCodecInfo") else {
                    return;
                };
                let Some(get_name) = env.method_id(info_class, "getName", "()Ljava/lang/String;") else {
                    return;
                };
                for i in 0..len.min(8) {
                    let item = env.array_get(array, i);
                    if item.is_null() {
                        continue;
                    }
                    if let Some(name) = env.call_object(item, get_name, &[]).and_then(|n| env.get_string(n)) {
                        println!("media:   codec {i}: {name}");
                    }
                }
            }
        }
    }
    // `createDecoderByType` goes through `Codec2Client::CreateComponentByName`
    // → `IComponentStore::createComponent`, so it exercises the real service.
    if let Some(mc_class) = env.find_class("android/media/MediaCodec") {
        if let Some(create) = env.static_method_id(
            mc_class,
            "createDecoderByType",
            "(Ljava/lang/String;)Landroid/media/MediaCodec;",
        ) {
            let mime = env.new_string("video/avc");
            match env.call_static_object(mc_class, create, &[JValue { l: mime }]) {
                Some(codec) if !codec.is_null() => {
                    println!("media: createDecoderByType(video/avc) ok");
                    probe_codec_configure(env, codec);
                }
                _ => eprintln!("media: createDecoderByType(video/avc) failed"),
            }
        }
    }
}

/// Drive `configure` + `start` on a freshly created decoder.
///
/// Both go through `IComponent` on the local `BnHwComponent`, so they exercise
/// the component side of the codec2 service.
fn probe_codec_configure(env: &JniEnv, codec: JObject) {
    let Some(fmt_class) = env.find_class("android/media/MediaFormat") else {
        return;
    };
    let Some(create_fmt) = env.static_method_id(
        fmt_class,
        "createVideoFormat",
        "(Ljava/lang/String;II)Landroid/media/MediaFormat;",
    ) else {
        return;
    };
    let mime = env.new_string("video/avc");
    let Some(format) = env.call_static_object(
        fmt_class,
        create_fmt,
        &[JValue { l: mime }, JValue { i: 320 }, JValue { i: 240 }],
    ) else {
        eprintln!("media: createVideoFormat threw");
        return;
    };
    let Some(mc_class) = env.find_class("android/media/MediaCodec") else {
        return;
    };
    // `CCodec` logs "ByteBuffer mode decoder w/o color format set: using default
    // planar color format", and then the store hands the decoder a *graphic*
    // gralloc block while CCodec expects linear output. Asking for flexible YUV
    // makes CCodec request a linear block pool instead.
    //
    // In surface mode the opposite is true: setting a colour format forces CCodec
    // to keep ByteBuffer output (`2D-BB-Output`), which makes MediaCodec copy
    // every frame through `SoftwareRenderer` instead of rendering straight into
    // the surface. So the key is only set for the ByteBuffer probe.
    let surface_mode = std::env::var_os("PENGUDROID_PROBE_SURFACE").is_some();
    if !surface_mode {
        if let Some(set_int) = env.method_id(fmt_class, "setInteger", "(Ljava/lang/String;I)V") {
            let key = env.new_string("color-format");
            let ok = env.call_void(format, set_int, &[JValue { l: key }, JValue { i: 21 }]);
            println!("media: set color-format=21 (flexible YUV) = {ok}");
        }
    } else {
        println!("media: surface mode, leaving color-format unset");
    }
    if let Some(configure) = env.method_id(
        mc_class,
        "configure",
        "(Landroid/media/MediaFormat;Landroid/view/Surface;Landroid/media/MediaCrypto;I)V",
    ) {
        // In surface mode the decoder writes straight into the consumer's
        // BufferQueue, so neither a colour format nor a ByteBuffer copy is
        // involved. `ImageReader` is the smallest in-process consumer that can
        // hand the planes back to us without a GL context, which makes it the
        // cheapest way to prove the surface path end to end.
        let mut reader: Option<JObject> = None;
        let mut surface: JObject = std::ptr::null_mut();
        if std::env::var_os("PENGUDROID_PROBE_SURFACE").is_some() {
            match make_image_reader(env, 320, 240, 2) {
                Some((r, s)) => {
                    reader = Some(r);
                    surface = s;
                    println!("media: ImageReader surface ready");
                }
                None => eprintln!("media: ImageReader setup failed; falling back to ByteBuffer"),
            }
        }
        let ok = env.call_void(
            codec,
            configure,
            &[
                JValue { l: format },
                JValue { l: surface },
                JValue { l: std::ptr::null_mut() },
                JValue { i: 0 },
            ],
        );
        println!(
            "media: MediaCodec.configure = {ok} ({} mode)",
            if reader.is_some() { "surface" } else { "bytebuffer" }
        );
        if let Some(start) = env.method_id(mc_class, "start", "()V") {
            let ok = env.call_void(codec, start, &[]);
            println!("media: MediaCodec.start = {ok}");
            if ok {
                probe_codec_decode(env, mc_class, fmt_class, codec, reader);
            }
        }
        return;
    }
}

/// Build an `ImageReader` in `ImageFormat.YUV_420_888` and its `Surface`.
fn make_image_reader(env: &JniEnv, width: i32, height: i32, max_images: i32) -> Option<(JObject, JObject)> {
    let Some(reader_class) = env.find_class("android/media/ImageReader") else {
        eprintln!("media: ImageReader class not found");
        return None;
    };
    let Some(new_instance) = env.static_method_id(
        reader_class,
        "newInstance",
        "(IIII)Landroid/media/ImageReader;",
    ) else {
        eprintln!("media: ImageReader.newInstance(IIII) missing");
        return None;
    };
    const IMAGE_FORMAT_YUV_420_888: i32 = 35;
    // `MediaCodec` renders ByteBuffer output through `SoftwareRenderer`, which
    // calls `Surface::setBuffersFormat(HAL_PIXEL_FORMAT_RGB_565)` before
    // queueing — so the consumer has to be configured for that format, or it
    // rejects every buffer with "Producer output buffer format 0x4 doesn't match
    // the ImageReader's configured buffer format 0x23".
    const PIXEL_FORMAT_RGB_565: i32 = 4;
    let _ = IMAGE_FORMAT_YUV_420_888;
    let Some(reader) = env.call_static_object(
        reader_class,
        new_instance,
        &[
            JValue { i: width },
            JValue { i: height },
            JValue { i: PIXEL_FORMAT_RGB_565 },
            JValue { i: max_images },
        ],
    ) else {
        eprintln!("media: ImageReader.newInstance threw");
        return None;
    };
    let Some(get_surface) = env.method_id(reader_class, "getSurface", "()Landroid/view/Surface;") else {
        eprintln!("media: ImageReader.getSurface missing");
        return None;
    };
    let Some(surface) = env.call_object(reader, get_surface, &[]) else {
        eprintln!("media: ImageReader.getSurface threw");
        return None;
    };
    Some((reader, surface))
}

/// Feed one H.264 access unit through the decoder and report what comes out.
///
/// `tools/make_h264.py` writes the stream: SPS + PPS + one IDR frame coded as a
/// single I_PCM macroblock, so no encoder is needed on the host.
fn probe_codec_decode(
    env: &JniEnv,
    mc_class: JClass,
    fmt_class: JClass,
    codec: JObject,
    reader: Option<JObject>,
) {
    let path = std::env::var("PENGUDROID_H264").unwrap_or_else(|_| "/tmp/test.h264".to_string());
    let Ok(data) = std::fs::read(&path) else {
        eprintln!("media: cannot read {path}");
        return;
    };
    let Some(deq_in) = env.method_id(mc_class, "dequeueInputBuffer", "(J)I") else {
        eprintln!("media: dequeueInputBuffer missing");
        return;
    };
    let Some(idx) = env.call_int(codec, deq_in, &[JValue { j: 1_000_000 }]) else {
        eprintln!("media: dequeueInputBuffer threw");
        return;
    };
    if idx < 0 {
        eprintln!("media: dequeueInputBuffer = {idx}");
        return;
    }
    let Some(get_in) = env.method_id(mc_class, "getInputBuffer", "(I)Ljava/nio/ByteBuffer;") else {
        return;
    };
    let Some(buf) = env.call_object(codec, get_in, &[JValue { i: idx }]) else {
        eprintln!("media: getInputBuffer threw");
        return;
    };
    let dst = env.direct_buffer_address(buf);
    if dst.is_null() {
        eprintln!("media: getInputBuffer is not direct");
        return;
    }
    unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len()) };
    let Some(queue) = env.method_id(mc_class, "queueInputBuffer", "(IIIJI)V") else {
        return;
    };
    let ok = env.call_void(
        codec,
        queue,
        &[
            JValue { i: idx },
            JValue { i: 0 },
            JValue { i: data.len() as i32 },
            JValue { j: 0 },
            JValue { i: 0 },
        ],
    );
    println!("media: queueInputBuffer({} bytes) = {ok}", data.len());

    // Signal end of stream so the decoder drains whatever it is holding,
    // unless PENGUDROID_NO_EOS asks for a plain steady-state dequeue.
    if std::env::var_os("PENGUDROID_NO_EOS").is_none() {
    if let Some(idx) = env.call_int(codec, deq_in, &[JValue { j: 1_000_000 }]) {
        if idx >= 0 {
            let queued = env.call_void(
                codec,
                queue,
                &[
                    JValue { i: idx },
                    JValue { i: 0 },
                    JValue { i: 0 },
                    JValue { j: 0 },
                    JValue { i: 4 }, // BUFFER_FLAG_END_OF_STREAM
                ],
            );
            println!("media: queueInputBuffer(EOS) = {queued}");
        }
    }
    }

    let Some(info_class) = env.find_class("android/media/MediaCodec$BufferInfo") else {
        return;
    };
    let Some(info_ctor) = env.method_id(info_class, "<init>", "()V") else {
        return;
    };
    let Some(info) = env.new_object(info_class, info_ctor, &[]) else {
        return;
    };
    let Some(deq_out) = env.method_id(
        mc_class,
        "dequeueOutputBuffer",
        "(Landroid/media/MediaCodec$BufferInfo;J)I",
    ) else {
        return;
    };
    let Some(size_field) = env.field_id(info_class, "size", "I") else {
        return;
    };
    let Some(flags_field) = env.field_id(info_class, "flags", "I") else {
        return;
    };
    for round in 0..6 {
        let out = env.call_int(codec, deq_out, &[JValue { l: info }, JValue { j: 5_000_000 }]);
        let size = env.int_field(info, size_field);
        let flags = env.int_field(info, flags_field);
        println!("media: dequeueOutputBuffer[{round}] = {out:?} size={size} flags={flags}");
        if out == Some(-2) {
            // INFO_OUTPUT_FORMAT_CHANGED: the decoder accepted the stream.
            if let Some(get_fmt) = env.method_id(mc_class, "getOutputFormat", "()Landroid/media/MediaFormat;") {
                if let Some(fmt) = env.call_object(codec, get_fmt, &[]) {
                    if let Some(get_int) = env.method_id(fmt_class, "getInteger", "(Ljava/lang/String;)I") {
                        for key in ["width", "height"] {
                            let k = env.new_string(key);
                            if let Some(v) = env.call_int(fmt, get_int, &[JValue { l: k }]) {
                                println!("media: output format {key}={v}");
                            }
                        }
                    }
                }
            }
            continue;
        }
        if out != Some(0) || flags & 4 != 0 {
            break;
        }
        if let Some(index) = out.filter(|&i| i >= 0) {
            if let Some(reader) = reader {
                // Surface mode: hand the buffer to the consumer, then pull the
                // planes back through `ImageReader`. That is what proves the
                // decoder really rendered into the surface's BufferQueue.
                if let Some(release) = env.method_id(mc_class, "releaseOutputBuffer", "(IZ)V") {
                    env.call_void(
                        codec,
                        release,
                        &[JValue { i: index }, JValue { i: 1 }],
                    );
                }
                report_surface_image(env, reader, index);
            } else {
                report_output_bytes(env, mc_class, codec, index, size);
            }
        }
    }
}

/// Pull the frame the decoder just queued to an `ImageReader` and summarise it.
///
/// Uses `acquireLatestImage` with a short retry loop: the queue and the app run
/// on different threads, so the first attempt after `releaseOutputBuffer` can
/// legitimately find the image not yet available.
fn report_surface_image(env: &JniEnv, reader: JObject, index: i32) {
    let Some(reader_class) = env.find_class("android/media/ImageReader") else {
        return;
    };
    let Some(acquire) = env.method_id(reader_class, "acquireLatestImage", "()Landroid/media/Image;") else {
        eprintln!("media: acquireLatestImage missing");
        return;
    };
    let mut image: Option<JObject> = None;
    for _ in 0..40 {
        if let Some(img) = env.call_object(reader, acquire, &[]) {
            if !img.is_null() {
                image = Some(img);
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let Some(image) = image else {
        println!("media: surface[{index}] no image available");
        return;
    };
    let Some(image_class) = env.find_class("android/media/Image") else {
        return;
    };
    let width = env
        .method_id(image_class, "getWidth", "()I")
        .and_then(|m| env.call_int(image, m, &[]))
        .unwrap_or(0);
    let height = env
        .method_id(image_class, "getHeight", "()I")
        .and_then(|m| env.call_int(image, m, &[]))
        .unwrap_or(0);
    let planes = env
        .method_id(image_class, "getPlanes", "()[Landroid/media/Image$Plane;")
        .and_then(|m| env.call_object(image, m, &[]));
    let mut plane_stats = Vec::new();
    if let Some(planes) = planes {
        let count = env.array_length(planes);
        let plane_class = env.find_class("android/media/Image$Plane");
        if let (Some(plane_class), Some(get_buffer)) = (
            plane_class,
            plane_class.and_then(|c| env.method_id(c, "getBuffer", "()Ljava/nio/ByteBuffer;")),
        ) {
            for i in 0..count {
                let plane = env.array_get(planes, i);
                if plane.is_null() {
                    continue;
                }
                let Some(buf) = env.call_object(plane, get_buffer, &[]) else {
                    continue;
                };
                let addr = env.direct_buffer_address(buf);
                if addr.is_null() {
                    plane_stats.push(format!("p{i}:not-direct"));
                    continue;
                }
                // Plane 0 is the whole frame: `width * height` bytes for a YUV
                // Y plane, twice that for RGB_565 (2 bytes per pixel).
                let len = if count == 1 {
                    (width * height * 2) as usize
                } else if i == 0 {
                    (width * height) as usize
                } else {
                    ((width / 2) * (height / 2)) as usize
                };
                if len == 0 {
                    continue;
                }
                let bytes = unsafe { std::slice::from_raw_parts(addr as *const u8, len) };
                let sum: u64 = bytes.iter().map(|&b| b as u64).sum();
                let head: Vec<String> = bytes.iter().take(8).map(|b| format!("{b:02x}")).collect();
                plane_stats.push(format!(
                    "p{i}:{}B mean={:.1} head={}",
                    len,
                    sum as f64 / len as f64,
                    head.join(" ")
                ));
            }
        }
    }
    println!(
        "media: surface[{index}] {width}x{height} planes={} | {}",
        env.array_length(planes.unwrap_or(std::ptr::null_mut())).max(0),
        plane_stats.join(" | ")
    );
    if let Some(close) = env.method_id(image_class, "close", "()V") {
        env.call_void(image, close, &[]);
    }
}

/// Read the decoded frame out of the output `ByteBuffer` and summarise it.
///
/// This is the end-to-end check for the video path: `size` alone only proves a
/// buffer of the right length came back, whereas the statistics prove the
/// decoder wrote pixels into it (the generated stream is a known gradient).
fn report_output_bytes(env: &JniEnv, mc_class: JClass, codec: JObject, round: i32, size: i32) {
    let Some(get_out) = env.method_id(mc_class, "getOutputBuffer", "(I)Ljava/nio/ByteBuffer;") else {
        eprintln!("media: getOutputBuffer missing");
        return;
    };
    let Some(buf) = env.call_object(codec, get_out, &[JValue { i: round }]) else {
        eprintln!("media: getOutputBuffer({round}) threw");
        return;
    };
    let addr = env.direct_buffer_address(buf);
    if addr.is_null() {
        eprintln!("media: output buffer {round} is not direct");
        return;
    }
    let len = size.max(0) as usize;
    let bytes = unsafe { std::slice::from_raw_parts(addr as *const u8, len) };
    let sum: u64 = bytes.iter().map(|&b| b as u64).sum();
    let mut distinct = std::collections::BTreeSet::new();
    for &b in bytes.iter().take(4096) {
        distinct.insert(b);
    }
    println!(
        "media: frame[{round}] {} bytes, mean={:.1}, min={} max={}, distinct(first 4K)={}",
        len,
        if len > 0 { sum as f64 / len as f64 } else { 0.0 },
        bytes.iter().copied().min().unwrap_or(0),
        bytes.iter().copied().max().unwrap_or(0),
        distinct.len()
    );
    let head: Vec<String> = bytes.iter().take(16).map(|b| format!("{b:02x}")).collect();
    println!("media: frame[{round}] head={}", head.join(" "));
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
    spawn_draw_monitor(env);
    spawn_input_pump(env);
    spawn_service_host(env);
    spawn_resized_pump(env);
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

/// How long the service thread sleeps between queue polls.
const SERVICE_POLL_MS: u64 = 100;
/// One broker record: a 20-byte header, two names, and the raw transaction.
const SERVICE_RECORD_CAP: usize = 8192;

/// Which lifecycle the app asked for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ServiceOp {
    Start,
    Bind,
}

/// One `startService`/`bindService` request popped from the binder broker.
struct ServiceRequest {
    package: String,
    class: String,
    op: ServiceOp,
    /// The transaction's data buffer, and where the `Intent` starts in it.
    ///
    /// The Intent is what carries the action (`Service.onBind` branches on it)
    /// and the extras (`onStartCommand` reads `should_start_foreground_extra`),
    /// so it is unmarshalled with the framework's own `Parcel.unmarshall`
    /// rather than rebuilt from the component name.
    parcel: Vec<u8>,
    intent_offset: usize,
}

/// `__pengudroid_binder_take_service_request`'s signature.
type TakeServiceRequest = unsafe extern "C" fn(*mut u8, usize, *mut usize) -> i32;

/// The host's miniature `ActiveServices`.
///
/// On a device the sequence is: `ActivityManagerService.startService` registers
/// the service, "bring up" posts `ActivityThread$H.CREATE_SERVICE`, the app
/// calls `handleCreateService`, and `ActiveServices` later publishes the
/// `IServiceConnection` callback (`connected`) after the app's `publishService`.
/// There is no system server here, so the two `IActivityManager` transactions the
/// app makes are answered and recorded by the binder broker
/// (`binder::note_service_request`); this type does the rest in-process over JNI.
struct ServiceHost {
    env: JniEnv,
    thread: JObject,
    thread_class: JClass,
    loaded_apk: JObject,
    app_info: JObject,
    compat: JObject,
    take: TakeServiceRequest,
    /// Services created here, keyed by class name, with the `IBinder` token
    /// `handleCreateService` filed them under. The references are local refs on
    /// the service thread, which stays attached to the VM for its whole life, so
    /// they remain valid.
    created: Vec<(String, JObject, JObject)>,
}

impl ServiceHost {
    fn new(env: JniEnv, thread: JObject, take: TakeServiceRequest) -> Option<Self> {
        let thread_class = env.find_class("android/app/ActivityThread")?;
        let data_field = env.field_id(
            thread_class,
            "mBoundApplication",
            "Landroid/app/ActivityThread$AppBindData;",
        )?;
        let data = env.object_field(thread, data_field);
        if data.is_null() {
            eprintln!("service: ActivityThread.mBoundApplication is null");
            return None;
        }
        let bind_class = env.find_class("android/app/ActivityThread$AppBindData")?;
        let loaded_apk = env.object_field(
            data,
            env.field_id(bind_class, "info", "Landroid/app/LoadedApk;")?,
        );
        let compat = env.object_field(
            data,
            env.field_id(
                bind_class,
                "compatInfo",
                "Landroid/content/res/CompatibilityInfo;",
            )?,
        );
        if loaded_apk.is_null() {
            eprintln!("service: AppBindData.info is null (no LoadedApk)");
            return None;
        }
        let apk_class = env.find_class("android/app/LoadedApk")?;
        let app_info = env.object_field(
            loaded_apk,
            env.field_id(
                apk_class,
                "mApplicationInfo",
                "Landroid/content/pm/ApplicationInfo;",
            )?,
        );
        if app_info.is_null() {
            eprintln!("service: LoadedApk.mApplicationInfo is null");
            return None;
        }
        Some(Self {
            env,
            thread,
            thread_class,
            loaded_apk,
            app_info,
            compat,
            take,
            created: Vec::new(),
        })
    }

    /// Pop one request; `None` when the queue is empty.
    fn take_request(&self) -> Option<ServiceRequest> {
        let mut buf = vec![0u8; SERVICE_RECORD_CAP];
        let mut len = 0usize;
        let rc = unsafe { (self.take)(buf.as_mut_ptr(), buf.len(), &mut len) };
        if rc != 0 {
            return None;
        }
        let len = len.min(buf.len());
        let record = &buf[..len];
        if record.len() < 20 {
            return None;
        }
        let word = |i: usize| -> usize {
            u32::from_ne_bytes([
                record[i * 4],
                record[i * 4 + 1],
                record[i * 4 + 2],
                record[i * 4 + 3],
            ]) as usize
        };
        let bind = word(0) != 0;
        let package_len = word(1);
        let class_len = word(2);
        let intent_offset = word(3);
        let parcel_len = word(4);
        let mut at = 20usize;
        if at + package_len + class_len + parcel_len > record.len() {
            return None;
        }
        let package = String::from_utf8_lossy(&record[at..at + package_len]).into_owned();
        at += package_len;
        let class = String::from_utf8_lossy(&record[at..at + class_len]).into_owned();
        at += class_len;
        let parcel = record[at..at + parcel_len].to_vec();
        if package.is_empty() || class.is_empty() {
            return None;
        }
        Some(ServiceRequest {
            package,
            class,
            op: if bind {
                ServiceOp::Bind
            } else {
                ServiceOp::Start
            },
            parcel,
            intent_offset,
        })
    }

    fn dispatch(&mut self, request: ServiceRequest) {
        let Some(intent) = self.request_intent(&request) else {
            return;
        };
        let Some((service, token)) = self.ensure_service(&request, intent) else {
            return;
        };
        let Some(service_class) = self.env.find_class("android/app/Service") else {
            return;
        };
        match request.op {
            ServiceOp::Start => {
                // `onStartCommand` is `H.SERVICE_ARGS` on a device. It has to run
                // on the main thread for the same reason `onCreate` does: NewPipe
                // builds its `Player` there, and that creates `Handler`s.
                if self.post_service_args(token, intent) {
                    println!("service: posted SERVICE_ARGS for {}", request.class);
                } else {
                    eprintln!("service: could not post SERVICE_ARGS for {}", request.class);
                }
            }
            ServiceOp::Bind => {
                let Some(on_bind) = self.env.method_id(
                    service_class,
                    "onBind",
                    "(Landroid/content/Intent;)Landroid/os/IBinder;",
                ) else {
                    eprintln!("service: Service.onBind not found");
                    return;
                };
                // `onBind` runs on a device's main thread too, but unlike
                // `onCreate` it is not allowed to block on anything here, and the
                // binder it returns is needed *now* to call `connected`. NewPipe's
                // `onBind` only inspects the action and returns a field.
                let binder = match self.env.call_object(service, on_bind, &[JValue { l: intent }]) {
                    Some(b) => b,
                    None => {
                        eprintln!("service: {}.onBind threw", request.class);
                        return;
                    }
                };
                if binder.is_null() {
                    // A null binder is "this service does not support binding",
                    // so a real system server never calls `connected`.
                    println!("service: {}.onBind returned null", request.class);
                    return;
                }
                println!("service: {}.onBind returned a binder", request.class);
                self.connect_dispatchers(&request, binder);
            }
        }
    }

    /// The `Intent` the app sent with the request, unmarshalled from the
    /// transaction's own bytes.
    ///
    /// Rebuilding it from the component name loses the action (`onBind` returns
    /// null for an unknown action — which is exactly what NewPipe did) and the
    /// extras (`onStartCommand` would take its "useless intent" path).
    fn request_intent(&self, request: &ServiceRequest) -> Option<JObject> {
        if !request.parcel.is_empty()
            && request.intent_offset > 0
            && request.intent_offset < request.parcel.len()
        {
            if let Some(intent) = self.unmarshall_intent(request) {
                return Some(intent);
            }
            eprintln!(
                "service: cannot unmarshall the Intent of {}; using a bare one",
                request.class
            );
        }
        self.component_intent(request)
    }

    fn unmarshall_intent(&self, request: &ServiceRequest) -> Option<JObject> {
        let parcel_class = self.env.find_class("android/os/Parcel")?;
        // `unmarshall` is an *instance* method in this framework (`obtain()`
        // first, then `unmarshall(bytes, offset, length)`); there is no static
        // `Parcel.unmarshall(...)` overload.
        let obtain = self
            .env
            .static_method_id(parcel_class, "obtain", "()Landroid/os/Parcel;")?;
        let parcel = self.env.call_static_object(parcel_class, obtain, &[])?;
        if parcel.is_null() {
            return None;
        }
        let unmarshall = self
            .env
            .method_id(parcel_class, "unmarshall", "([BII)V")?;
        let bytes = self.env.new_byte_array(request.parcel.len() as i32)?;
        self.env.set_byte_array_region(bytes, 0, &request.parcel);
        let offset = request.intent_offset as i32;
        let length = (request.parcel.len() - request.intent_offset) as i32;
        if !self.env.call_void(
            parcel,
            unmarshall,
            &[JValue { l: bytes }, JValue { i: offset }, JValue { i: length }],
        ) {
            return None;
        }
        if let Some(reset) = self.env.method_id(parcel_class, "setDataPosition", "(I)V") {
            self.env.call_void(parcel, reset, &[JValue { i: 0 }]);
        }
        let intent_class = self.env.find_class("android/content/Intent")?;
        let creator_field = self.env.static_field_id(
            intent_class,
            "CREATOR",
            "Landroid/os/Parcelable$Creator;",
        )?;
        let creator = self.env.static_object_field(intent_class, creator_field);
        if creator.is_null() {
            eprintln!("service: Intent.CREATOR is null");
            return None;
        }
        let creator_class = self.env.find_class("android/os/Parcelable$Creator")?;
        let create = self.env.method_id(
            creator_class,
            "createFromParcel",
            "(Landroid/os/Parcel;)Ljava/lang/Object;",
        )?;
        let intent = self.env.call_object(creator, create, &[JValue { l: parcel }])?;
        if intent.is_null() {
            return None;
        }
        if let Some(to_string) = self.env.method_id(intent_class, "toString", "()Ljava/lang/String;") {
            if let Some(text) = self
                .env
                .call_object(intent, to_string, &[])
                .and_then(|s| self.env.get_string(s))
            {
                let shown: String = text.chars().take(400).collect();
                println!("service: {} intent = {}", request.class, shown.replace('\n', " "));
            }
        }
        Some(intent)
    }

    /// Create the service once and return its instance and token.
    fn ensure_service(
        &mut self,
        request: &ServiceRequest,
        intent: JObject,
    ) -> Option<(JObject, JObject)> {
        if let Some((_, service, token)) = self
            .created
            .iter()
            .find(|(name, _, _)| name == &request.class)
        {
            return Some((*service, *token));
        }
        let info = self.service_info(request)?;
        // A token only has to be a stable `IBinder`; `handleCreateService` uses it
        // as the key in `ActivityThread.mServices`.
        let token = new_instance(&self.env, "android/os/Binder").map(|(_, b)| b)?;
        let (data_class, data) = new_instance(&self.env, "android/app/ActivityThread$CreateServiceData")?;
        self.set_field(data_class, data, "info", "Landroid/content/pm/ServiceInfo;", info);
        self.set_field(
            data_class,
            data,
            "compatInfo",
            "Landroid/content/res/CompatibilityInfo;",
            self.compat,
        );
        self.set_field(data_class, data, "token", "Landroid/os/IBinder;", token);
        self.set_field(data_class, data, "intent", "Landroid/content/Intent;", intent);

        println!("service: creating {}", request.class);
        let _ = std::io::Write::flush(&mut std::io::stdout());
        if !self.post_create_service(data) {
            return None;
        }
        // `handleCreateService` runs on the main thread now, so wait for it to
        // publish the instance in `mServices`. It is quick (no I/O), but the
        // main thread may be busy with a traversal, so allow a few seconds.
        for _ in 0..250 {
            if let Some(service) = self.service_for_token(token) {
                if !service.is_null() {
                    println!("service: created {}", request.class);
                    self.created
                        .push((request.class.clone(), service, token));
                    return Some((service, token));
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        eprintln!(
            "service: {} never appeared in ActivityThread.mServices",
            request.class
        );
        None
    }

    /// Send `ActivityThread$H.CREATE_SERVICE` so the framework creates the
    /// service on the main thread.
    ///
    /// This is not optional: `handleCreateService` instantiates the service (and
    /// runs `onCreate`), and a `Service` that touches `new Handler()` there needs
    /// a `Looper`. NewPipe's `PlayerService` does exactly that through
    /// `MediaBrowserServiceCompat`, so calling `handleCreateService` from this
    /// thread fails with "Can't create handler inside thread that has not called
    /// Looper.prepare()". `H` is the message the app's own binder thread posts.
    fn post_create_service(&self, data: JObject) -> bool {
        self.post_to_h("CREATE_SERVICE", 114, data)
    }

    /// `ActivityThread$H.SERVICE_ARGS`: run `Service.onStartCommand` on the main
    /// thread with the intent the app asked for.
    fn post_service_args(&self, token: JObject, intent: JObject) -> bool {
        let Some((data_class, data)) = new_instance(&self.env, "android/app/ActivityThread$ServiceArgsData")
        else {
            eprintln!("service: ActivityThread$ServiceArgsData not found");
            return false;
        };
        self.set_field(data_class, data, "token", "Landroid/os/IBinder;", token);
        self.set_field(data_class, data, "args", "Landroid/content/Intent;", intent);
        // Nothing here calls `stopSelfResult`, the only caller of these two.
        if let Some(id) = self.env.field_id(data_class, "startId", "I") {
            self.env.set_int_field(data, id, 1);
        }
        if let Some(id) = self.env.field_id(data_class, "flags", "I") {
            self.env.set_int_field(data, id, 0);
        }
        self.post_to_h("SERVICE_ARGS", 115, data)
    }

    /// Post a `Message` to `ActivityThread.mH`, the app's main-thread handler.
    fn post_to_h(&self, what_field: &str, fallback: i32, data: JObject) -> bool {
        let Some(handler_class) = self.env.find_class("android/app/ActivityThread$H") else {
            eprintln!("service: ActivityThread$H not found");
            return false;
        };
        let Some(handler_field) = self.env.field_id(
            self.thread_class,
            "mH",
            "Landroid/app/ActivityThread$H;",
        ) else {
            eprintln!("service: ActivityThread.mH not found");
            return false;
        };
        let handler = self.env.object_field(self.thread, handler_field);
        if handler.is_null() {
            eprintln!("service: ActivityThread.mH is null");
            return false;
        }
        // Read the message code out of the framework instead of hardcoding it:
        // it is a `static final int` on `H` and stable, but the value is not part
        // of any public API.
        let what = self
            .env
            .static_field_id(handler_class, what_field, "I")
            .map(|id| self.env.static_int_field(handler_class, id))
            .unwrap_or(fallback);
        let Some(message_class) = self.env.find_class("android/os/Message") else {
            return false;
        };
        let Some(obtain) = self.env.static_method_id(
            message_class,
            "obtain",
            "(Landroid/os/Handler;ILjava/lang/Object;)Landroid/os/Message;",
        ) else {
            eprintln!("service: Message.obtain(Handler,int,Object) not found");
            return false;
        };
        let Some(send) = self.env.method_id(message_class, "sendToTarget", "()V") else {
            eprintln!("service: Message.sendToTarget not found");
            return false;
        };
        let Some(message) = self.env.call_static_object(
            message_class,
            obtain,
            &[JValue { l: handler }, JValue { i: what }, JValue { l: data }],
        ) else {
            eprintln!("service: Message.obtain threw");
            return false;
        };
        if message.is_null() {
            eprintln!("service: Message.obtain returned null");
            return false;
        }
        self.env.call_void(message, send, &[])
    }

    fn set_field(&self, class: JClass, obj: JObject, field: &str, sig: &str, value: JObject) {
        match self.env.field_id(class, field, sig) {
            Some(id) => self.env.set_object_field(obj, id, value),
            None => eprintln!("service: field {field} not found"),
        }
    }

    /// Build the `ServiceInfo` `handleCreateService` needs.
    ///
    /// There is no package manager to ask (`IPackageManager.getServiceInfo` is
    /// not answered by the broker), so only the fields the lifecycle actually
    /// reads are filled in: the class name, the package, and the same
    /// `ApplicationInfo` the bound `LoadedApk` holds — which is what makes
    /// `getPackageInfoNoCheck` reuse the app's existing `LoadedApk` instead of
    /// building a second one with no class loader.
    fn service_info(&self, request: &ServiceRequest) -> Option<JObject> {
        let (class, info) = new_instance(&self.env, "android/content/pm/ServiceInfo")?;
        self.set_field(
            class,
            info,
            "name",
            "Ljava/lang/String;",
            self.env.new_string(&request.class),
        );
        self.set_field(
            class,
            info,
            "packageName",
            "Ljava/lang/String;",
            self.env.new_string(&request.package),
        );
        self.set_field(
            class,
            info,
            "applicationInfo",
            "Landroid/content/pm/ApplicationInfo;",
            self.app_info,
        );
        if let Some(id) = self.env.field_id(class, "exported", "Z") {
            self.env.set_boolean_field(info, id, true);
        }
        if let Some(id) = self.env.field_id(class, "enabled", "Z") {
            self.env.set_boolean_field(info, id, true);
        }
        Some(info)
    }

    fn component_intent(&self, request: &ServiceRequest) -> Option<JObject> {
        let (class, intent) = new_instance(&self.env, "android/content/Intent")?;
        let set = self.env.method_id(
            class,
            "setClassName",
            "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;",
        )?;
        let package = self.env.new_string(&request.package);
        let name = self.env.new_string(&request.class);
        let out = self
            .env
            .call_object(intent, set, &[JValue { l: package }, JValue { l: name }])?;
        if out.is_null() {
            return None;
        }
        Some(intent)
    }

    fn component_name(&self, request: &ServiceRequest) -> Option<JObject> {
        let class = self.env.find_class("android/content/ComponentName")?;
        let ctor = self.env.method_id(class, "<init>", "(Ljava/lang/String;Ljava/lang/String;)V")?;
        let package = self.env.new_string(&request.package);
        let name = self.env.new_string(&request.class);
        self.env
            .new_object(class, ctor, &[JValue { l: package }, JValue { l: name }])
    }

    fn service_for_token(&self, token: JObject) -> Option<JObject> {
        let field = self.env.field_id(
            self.thread_class,
            "mServices",
            "Landroid/util/ArrayMap;",
        )?;
        let map = self.env.object_field(self.thread, field);
        if map.is_null() {
            return None;
        }
        let map_class = self.env.find_class("android/util/ArrayMap")?;
        let get = self
            .env
            .method_id(map_class, "get", "(Ljava/lang/Object;)Ljava/lang/Object;")?;
        self.env.call_object(map, get, &[JValue { l: token }])
    }

    /// Fire the connection callbacks the app registered before `bindService`.
    ///
    /// `ContextImpl.bindServiceCommon` puts the `IServiceConnection` into
    /// `LoadedApk.mServices` *before* transacting with the activity manager, and
    /// keeps it there because the broker's reply says the bind was accepted. The
    /// value is a `LoadedApk$ServiceDispatcher`, whose `connected` posts the
    /// app's `ServiceConnection.onServiceConnected` onto the main thread — the
    /// same thing `ActiveServices` does after `publishService`.
    fn connect_dispatchers(&self, request: &ServiceRequest, binder: JObject) {
        let Some(component) = self.component_name(request) else {
            return;
        };
        let Some(apk_class) = self.env.find_class("android/app/LoadedApk") else {
            return;
        };
        let Some(field) = self.env.field_id(apk_class, "mServices", "Landroid/util/ArrayMap;") else {
            eprintln!("service: LoadedApk.mServices not found");
            return;
        };
        let outer = self.env.object_field(self.loaded_apk, field);
        if outer.is_null() {
            eprintln!("service: LoadedApk.mServices is null");
            return;
        }
        let Some(map_class) = self.env.find_class("android/util/ArrayMap") else {
            return;
        };
        let (Some(size_m), Some(value_at)) = (
            self.env.method_id(map_class, "size", "()I"),
            self.env.method_id(map_class, "valueAt", "(I)Ljava/lang/Object;"),
        ) else {
            return;
        };
        let Some(dispatcher_class) = self.env.find_class("android/app/LoadedApk$ServiceDispatcher") else {
            eprintln!("service: LoadedApk$ServiceDispatcher not found");
            return;
        };
        let Some(connected) = self.env.method_id(
            dispatcher_class,
            "connected",
            "(Landroid/content/ComponentName;Landroid/os/IBinder;Z)V",
        ) else {
            eprintln!("service: LoadedApk$ServiceDispatcher.connected not found");
            return;
        };
        // `mServices` is `ArrayMap<Context, ArrayMap<IServiceConnection,
        // ServiceDispatcher>>`; the inner maps hold the dispatchers.
        let contexts = self.env.call_int(outer, size_m, &[]).unwrap_or(0);
        let mut dispatched = 0;
        for i in 0..contexts {
            let Some(inner) = self.env.call_object(outer, value_at, &[JValue { i }]) else {
                continue;
            };
            if inner.is_null() {
                continue;
            }
            let bindings = self.env.call_int(inner, size_m, &[]).unwrap_or(0);
            for j in 0..bindings {
                let Some(dispatcher) = self
                    .env
                    .call_object(inner, value_at, &[JValue { i: j }])
                else {
                    continue;
                };
                if dispatcher.is_null() {
                    continue;
                }
                if self.env.call_void(
                    dispatcher,
                    connected,
                    &[JValue { l: component }, JValue { l: binder }, JValue { z: 0 }],
                ) {
                    dispatched += 1;
                }
            }
        }
        println!(
            "service: connected {dispatched} pending binding(s) for {}",
            request.class
        );
        if dispatched == 0 {
            eprintln!(
                "service: no ServiceDispatcher was registered for {}",
                request.class
            );
        }
    }
}

/// Act on the `startService`/`bindService` requests the binder broker records.
///
/// The app makes those calls while the main thread is inside `Looper.loop()`, so
/// they cannot be handled inline; this thread owns the service lifecycle the way
/// `system_server` would, and the framework posts the `ServiceConnection`
/// callbacks back to the main thread itself.
fn spawn_service_host(env: &JniEnv) {
    let Some(vm) = env.get_java_vm() else {
        return;
    };
    let Some(sym) = shim_symbol("__pengudroid_binder_take_service_request") else {
        eprintln!("service: the binder broker has no service request queue");
        return;
    };
    // SAFETY: the shim exports the function with exactly this signature.
    let take: TakeServiceRequest = unsafe { std::mem::transmute(sym) };
    let vm_addr = vm as usize;
    std::thread::spawn(move || {
        let vm = vm_addr as *mut c_void;
        let Some(env) = attach_current_thread(vm) else {
            return;
        };
        // `loop_forever` runs right after the bind, but `ActivityThread` publishes
        // itself before then; poll briefly so a race cannot lose every request.
        let mut thread = std::ptr::null_mut();
        for _ in 0..100 {
            if let Some(current) = current_activity_thread(&env) {
                if !current.is_null() {
                    thread = current;
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if thread.is_null() {
            eprintln!("service: ActivityThread.currentActivityThread() is null");
            return;
        }
        let Some(mut host) = ServiceHost::new(env, thread, take) else {
            return;
        };
        println!("service: watching the binder broker for startService/bindService");
        loop {
            while let Some(request) = host.take_request() {
                host.dispatch(request);
            }
            std::thread::sleep(std::time::Duration::from_millis(SERVICE_POLL_MS));
        }
    });
}

/// Forward compositor input into the app's window.
///
/// The gleshim subscribes to the daemon's seat and queues events; this thread
/// drains that queue and turns each one into a `MotionEvent` delivered through
/// `ViewRootImpl.dispatchInputEvent`, which is the same entry point the input
/// channel uses on a device.
/// Detect display/window size changes after launch. A rotation can resize an
/// existing ViewRoot without creating a new BLASTBufferQueue, so revisit the
/// SurfaceView constraints only when the root geometry actually changes.
fn refresh_surface_views_after_resize(env: &JniEnv, previous: &mut Option<Vec<(i32, i32)>>) {
    let Some(wmg_class) = env.find_class("android/view/WindowManagerGlobal") else { return };
    let (Some(get_instance), Some(roots_field)) = (
        env.static_method_id(wmg_class, "getInstance", "()Landroid/view/WindowManagerGlobal;"),
        env.field_id(wmg_class, "mRoots", "Ljava/util/ArrayList;"),
    ) else { return };
    let Some(wmg) = env.call_static_object(wmg_class, get_instance, &[]) else { return };
    let roots = env.object_field(wmg, roots_field);
    if roots.is_null() { return; }
    let Some(list_class) = env.find_class("java/util/ArrayList") else { return };
    let (Some(size_method), Some(get_method)) = (
        env.method_id(list_class, "size", "()I"),
        env.method_id(list_class, "get", "(I)Ljava/lang/Object;"),
    ) else { return; };
    let Some(root_class) = env.find_class("android/view/ViewRootImpl") else { return };
    let (Some(width_field), Some(height_field)) = (
        env.field_id(root_class, "mWidth", "I"),
        env.field_id(root_class, "mHeight", "I"),
    ) else { return; };
    let count = env.call_int(roots, size_method, &[]).unwrap_or(0).max(0);
    let mut sizes = Vec::with_capacity(count as usize);
    for index in 0..count {
        let Some(root) = env.call_object(roots, get_method, &[JValue { i: index }]) else { continue };
        if root.is_null() { continue; }
        sizes.push((env.int_field(root, width_field), env.int_field(root, height_field)));
    }
    let changed = previous.as_ref().is_some_and(|old| old != &sizes);
    *previous = Some(sizes);
    if changed {
        println!("apphost: root geometry changed; refreshing SurfaceView constraints");
        report_surface_views(env, false);
    }
}

fn spawn_input_pump(env: &JniEnv) {
    let Some(vm) = env.get_java_vm() else {
        return;
    };
    let vm_addr = vm as usize;
    std::thread::spawn(move || {
        let vm = vm_addr as *mut c_void;
        // The gleshim is in `LD_PRELOAD`, so its poller is in the global scope.
        let sym = unsafe {
            libc::dlsym(
                libc::RTLD_DEFAULT,
                b"pengudroid_input_poll\0".as_ptr() as *const libc::c_char,
            )
        };
        if sym.is_null() {
            eprintln!("apphost: input pump: pengudroid_input_poll not found");
            return;
        }
        type Poll = unsafe extern "C" fn(*mut u8) -> i32;
        let poll: Poll = unsafe { std::mem::transmute(sym) };
        let Some(env) = attach_current_thread(vm) else {
            eprintln!("apphost: input pump: cannot attach the thread");
            return;
        };
        let Some(wmg_class) = env.find_class("android/view/WindowManagerGlobal") else {
            eprintln!("apphost: input pump: WindowManagerGlobal not found");
            return;
        };
        let Some(root_class) = env.find_class("android/view/ViewRootImpl") else {
            eprintln!("apphost: input pump: ViewRootImpl not found");
            return;
        };
        let Some(motion_class) = env.find_class("android/view/MotionEvent") else {
            return;
        };
        let Some(clock_class) = env.find_class("android/os/SystemClock") else {
            return;
        };
        let (Some(get_instance), Some(roots_field)) = (
            env.static_method_id(wmg_class, "getInstance", "()Landroid/view/WindowManagerGlobal;"),
            env.field_id(wmg_class, "mRoots", "Ljava/util/ArrayList;"),
        ) else {
            return;
        };
        let Some(list_class) = env.find_class("java/util/ArrayList") else {
            return;
        };
        let (Some(size_m), Some(get_m)) = (
            env.method_id(list_class, "size", "()I"),
            env.method_id(list_class, "get", "(I)Ljava/lang/Object;"),
        ) else {
            return;
        };
        let Some(obtain) = env.static_method_id(
            motion_class,
            "obtain",
            "(JJIFFI)Landroid/view/MotionEvent;",
        ) else {
            eprintln!("apphost: input pump: MotionEvent.obtain not found");
            return;
        };
        let Some(uptime) = env.static_method_id(clock_class, "uptimeMillis", "()J") else {
            return;
        };
        let Some(dispatch) = env.method_id(root_class, "dispatchInputEvent", "(Landroid/view/InputEvent;)V")
        else {
            eprintln!("apphost: input pump: dispatchInputEvent not found");
            return;
        };
        // `MotionEvent.setAxisValue` does not exist in this framework (only the
        // `getAxisValue` accessors do), so a wheel delta cannot be attached to a
        // synthesized event. `PointerAxis` is therefore dropped instead of
        // dispatching a scroll with a zero delta — which would look like the app
        // ignoring the wheel anyway. A real fix needs either that hidden setter or
        // an `InputManager.injectInputEvent` path.
        let set_axis_value: Option<JMethodId> = None;
        let Some(key_class) = env.find_class("android/view/KeyEvent") else {
            return;
        };
        // Include physical-keyboard source, scan code and meta state so EditText
        // and shortcut handlers receive Shift/Ctrl chords correctly.
        let Some(key_ctor) = env.method_id(key_class, "<init>", "(JJIIIIIIII)V") else {
            return;
        };

        const ACTION_DOWN: i32 = 0;
        const ACTION_UP: i32 = 1;
        const ACTION_MOVE: i32 = 2;
        const ACTION_HOVER_MOVE: i32 = 7;
        const ACTION_SCROLL: i32 = 8;

        let mut keyboard = crate::keyboard::Keyboard::default();
        let mut down_time = 0i64;
        let mut pressed = false;
        let mut last = (0.0f32, 0.0f32);
        let mut buf = [0u8; 16];

        // Deliver one motion event to every window root. Each root recycles the
        // event it is handed, so a fresh instance is obtained per root.
        let inject = |action: i32, x: f32, y: f32, down_time: i64| {
            let Some(wmg) = env.call_static_object(wmg_class, get_instance, &[]) else {
                return;
            };
            let roots = env.object_field(wmg, roots_field);
            if roots.is_null() {
                return;
            }
            let count = env.call_int(roots, size_m, &[]).unwrap_or(0);
            for i in 0..count {
                let Some(root) = env.call_object(roots, get_m, &[JValue { i }]) else {
                    continue;
                };
                if root.is_null() {
                    continue;
                }
                let now = env.call_static_long(clock_class, uptime, &[]).unwrap_or(0);
                let Some(event) = env.call_static_object(
                    motion_class,
                    obtain,
                    &[
                        JValue { j: down_time },
                        JValue { j: now },
                        JValue { i: action },
                        JValue { f: x },
                        JValue { f: y },
                        JValue { i: 0 },
                    ],
                ) else {
                    continue;
                };
                if event.is_null() {
                    continue;
                }
                env.call_void(root, dispatch, &[JValue { l: event }]);
            }
        };

        // The same, but with a wheel delta attached: `setAxisValue` has to run on
        // the instance that is actually dispatched, which is per-root.
        let inject_scroll = |x: f32, y: f32, delta: f32| {
            let Some(wmg) = env.call_static_object(wmg_class, get_instance, &[]) else {
                return;
            };
            let roots = env.object_field(wmg, roots_field);
            if roots.is_null() {
                return;
            }
            let count = env.call_int(roots, size_m, &[]).unwrap_or(0);
            for i in 0..count {
                let Some(root) = env.call_object(roots, get_m, &[JValue { i }]) else {
                    continue;
                };
                if root.is_null() {
                    continue;
                }
                let now = env.call_static_long(clock_class, uptime, &[]).unwrap_or(0);
                let Some(event) = env.call_static_object(
                    motion_class,
                    obtain,
                    &[
                        JValue { j: now },
                        JValue { j: now },
                        JValue { i: ACTION_SCROLL },
                        JValue { f: x },
                        JValue { f: y },
                        JValue { i: 0 },
                    ],
                ) else {
                    continue;
                };
                if event.is_null() {
                    continue;
                }
                if let Some(set_axis) = set_axis_value {
                    // AXIS_VSCROLL = 9.
                    env.call_void(event, set_axis, &[JValue { i: 9 }, JValue { f: delta }]);
                }
                env.call_void(root, dispatch, &[JValue { l: event }]);
            }
        };

        // `PENGUDROID_SYNTH_CLICK="x,y[@delayMs][;x,y[@delayMs]]…"` injects a
        // sequence of taps. There is no way to click a window from a sandboxed
        // session, so this is how the input path is exercised end to end; a
        // sequence is needed because a real flow starts with a one-off dialog
        // (NewPipe's "Keep Android Open") that has to be dismissed before the
        // screen we actually want to inspect even exists.
        //
        // `@delayMs` is the wait *before* that tap (default 8000 ms), so
        // `"665,682;145,300@12000"` means "tap OK after 8 s, then open the first
        // feed item 12 s later".
        eprintln!("apphost: input pump: reached the click/probe stage");
        let spec = std::env::var("PENGUDROID_SYNTH_CLICK").unwrap_or_default();
        if !spec.is_empty()
            || std::env::var_os("PENGUDROID_PROBE_VIEWS").is_some()
            || std::env::var_os("PENGUDROID_FORCE_SURFACE").is_some()
            || std::env::var_os("PENGUDROID_CLICK_TEXT").is_some()
        {
            let mut taps: Vec<(f32, f32, u64)> = Vec::new();
            for item in spec.split(';') {
                let (coords, delay) = match item.split_once('@') {
                    Some((c, d)) => (c, d.trim().parse::<u64>().unwrap_or(8000)),
                    None => (item, 8000),
                };
                let mut parts = coords.split(',').filter_map(|p| p.trim().parse::<f32>().ok());
                if let (Some(x), Some(y)) = (parts.next(), parts.next()) {
                    taps.push((x, y, delay));
                }
            }
            for (step, (x, y, delay)) in taps.iter().enumerate() {
                std::thread::sleep(std::time::Duration::from_millis(*delay));
                let now = env.call_static_long(clock_class, uptime, &[]).unwrap_or(0);
                inject(ACTION_DOWN, *x, *y, now);
                std::thread::sleep(std::time::Duration::from_millis(80));
                inject(ACTION_UP, *x, *y, now);
                println!(
                    "apphost: synthetic click {step} at {x},{y} injected at +{} ms",
                    monotonic_ms()
                );
            }
            // `PENGUDROID_CLICK_TEXT=<substring>[@delayMs]`: click a widget by its
            // text instead of by guessed coordinates. This is what makes "open the
            // third feed item" reproducible when the feed's contents change.
            if let Ok(spec) = std::env::var("PENGUDROID_CLICK_TEXT") {
                let (needle, delay) = match spec.split_once('@') {
                    Some((n, d)) => (n, d.trim().parse::<u64>().unwrap_or(3000)),
                    None => (spec.as_str(), 3000),
                };
                if !needle.trim().is_empty() {
                    std::thread::sleep(std::time::Duration::from_millis(delay));
                    let clicked = click_text(&env, needle.trim(), &inject);
                    println!(
                        "apphost: click-text {:?} injected at +{} ms",
                        needle.trim(),
                        monotonic_ms()
                    );
                    if !clicked {
                        println!("apphost: click-text {:?} found no match", needle.trim());
                    }
                }
            }
            // The probe runs even without a tap, so a baseline ("how many
            // windows exist with no interaction at all") is obtainable.
            // The forced-surface pass runs unconditionally: without it a
            // `SurfaceView` created after the activity window collapsed stays at
            // 0 wide, never gets `mHaveFrame`, and the video layer is never
            // created. Only the dump is gated, by `PENGUDROID_PROBE_VIEWS`.
            let verbose_views = std::env::var_os("PENGUDROID_PROBE_VIEWS").is_some();
            if true
                || std::env::var_os("PENGUDROID_FORCE_SURFACE").is_some()
            {
                // A playing stream can die within a few seconds, so the wait is
                // adjustable: 12 s is right for a static screen, 2–3 s is what a
                // live player leaves you.
                let delay = std::env::var("PENGUDROID_PROBE_DELAY")
                    .ok()
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .unwrap_or(12_000);
                std::thread::sleep(std::time::Duration::from_millis(delay));
                report_surface_views(&env, verbose_views);
                // The forced size only takes effect on a *later* traversal, which
                // runs on the main thread; measured, the view goes from 0x405 to
                // 720x405 between the first and the second pass. Repeat so a slow
                // first traversal cannot leave the video layer uncreated.
                // `updateSurface()` only builds the controls while
                // `mSurfaceControl == null`, so repeated passes are safe.
                for pass in 2..=MAX_FORCE_PASSES {
                    std::thread::sleep(std::time::Duration::from_millis(1500));
                    if verbose_views {
                        println!("views: --- pass {pass} ---");
                    }
                    report_surface_views(&env, verbose_views);
                }
            }
        }

        // Per-app keymap: a bound key becomes a gesture instead of a key press.
        // The GUI writes this file, and a missing file simply means no mapping.
        let keymap = APP_PACKAGE.get().and_then(|package| {
            // The launcher resolves the host path before the prefix overrides
            // the guest's HOME; fall back to the documented location otherwise.
            let path = std::env::var_os("PENGUDROID_KEYMAP_PATH")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| pengudroid_input::keymap::keymap_path(package));
            let text = std::fs::read_to_string(&path).ok()?;
            match pengudroid_input::keymap::Keymap::parse(&text) {
                Ok(map) if !map.bindings.is_empty() => {
                    println!(
                        "input: keymap {} ({} binding(s))",
                        path.display(),
                        map.bindings.len()
                    );
                    Some(map)
                }
                Ok(_) => None,
                Err(e) => {
                    // A typo must be visible, not silently ignored: the user
                    // would otherwise think the mapping is broken.
                    eprintln!("input: keymap {} ignored: {e}", path.display());
                    None
                }
            }
        });
        let mut mapped_held = crate::keymap_gesture::HeldPoints::default();
        // Mapped gestures share one pointer, so the press time of the most
        // recent `Down` is enough to keep `MotionEvent`'s downTime consistent.
        let mut mapped_down = 0i64;

        let mut last_trace = std::time::Instant::now();
        // `PENGUDROID_TRACE_ROOTS=<ms>`: a timeline of the window roots. The
        // single probe pass only says where the state ended up; this says *when*
        // it changed, which is what separates "the reply never landed" from
        // "the frame was reset afterwards".
        let root_trace_ms = std::env::var("PENGUDROID_TRACE_ROOTS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|v| *v > 0);
        let mut last_roots = std::time::Instant::now();
        let trace_input = std::env::var_os("PENGUDROID_TRACE_INPUT").is_some();
        let mut input_heartbeat = std::time::Instant::now();
        let mut last_geometry = std::time::Instant::now();
        let mut root_sizes = None;
        loop {
            if last_geometry.elapsed() >= std::time::Duration::from_millis(500) {
                last_geometry = std::time::Instant::now();
                if let Some(_frame) = env.local_frame(64) {
                    refresh_surface_views_after_resize(&env, &mut root_sizes);
                }
            }
            if trace_input && input_heartbeat.elapsed() >= std::time::Duration::from_secs(2) {
                input_heartbeat = std::time::Instant::now();
                println!("input: pump alive");
            }
            if let Some(ms) = root_trace_ms {
                if last_roots.elapsed().as_millis() >= ms as u128 {
                    last_roots = std::time::Instant::now();
                    trace_roots(&env);
                }
            }
            if std::env::var_os("PENGUDROID_TRACE_SURFACE").is_some()
                && last_trace.elapsed() >= std::time::Duration::from_secs(2)
            {
                last_trace = std::time::Instant::now();
                trace_surface_views(&env);
            }
            if unsafe { poll(buf.as_mut_ptr()) } == 0 {
                std::thread::sleep(std::time::Duration::from_millis(8));
                continue;
            }
            let Some(_local_frame) = env.local_frame(64) else {
                eprintln!("apphost: input pump: cannot allocate JNI local frame");
                return;
            };
            let tag = u32::from_le_bytes(buf[4..8].try_into().unwrap());
            let a = f32::from_le_bytes(buf[8..12].try_into().unwrap());
            let b = f32::from_le_bytes(buf[12..16].try_into().unwrap());
            if trace_input {
                let surface = u32::from_le_bytes(buf[0..4].try_into().unwrap());
                println!("input: received surface={surface} tag={tag} a={a} b={b}");
            }
            // Mouse wheel: Wayland's axis value is positive when scrolling down,
            // while `MotionEvent.AXIS_VSCROLL` is positive when scrolling up, so
            // sign flips. This is what lets the user scroll a feed at all.
            if tag == 5 && a as u8 == 0 {
                inject_scroll(last.0, last.1, -b);
                continue;
            }
            // Keyboard: `a` is the Linux keycode, `b` is 1 for press.
            if tag == 6 {
                // A bound key drives a touch gesture instead of the Android
                // key. Consuming it here is what makes a touch-only game
                // playable without also typing into the app.
                if let Some(map) = &keymap {
                    if let Some(action) = map.action_for(a as u16) {
                        let gesture_steps = crate::keymap_gesture::steps(
                            action,
                            a as u16,
                            b != 0.0,
                            &mut mapped_held,
                        );
                        if !gesture_steps.is_empty() {
                            println!(
                                "input: keymap code={a} -> {} touch step(s)",
                                gesture_steps.len()
                            );
                        }
                        for step in gesture_steps {
                            if step.delay_ms > 0 {
                                std::thread::sleep(std::time::Duration::from_millis(
                                    step.delay_ms,
                                ));
                            }
                            match step.event {
                                crate::keymap_gesture::Event::Down { x, y } => {
                                    mapped_down =
                                        env.call_static_long(clock_class, uptime, &[]).unwrap_or(0);
                                    inject(ACTION_DOWN, x, y, mapped_down);
                                }
                                crate::keymap_gesture::Event::Move { x, y } => {
                                    inject(ACTION_MOVE, x, y, mapped_down);
                                }
                                crate::keymap_gesture::Event::Up { x, y } => {
                                    inject(ACTION_UP, x, y, mapped_down);
                                }
                            }
                        }
                        continue;
                    }
                }
                let now = env.call_static_long(clock_class, uptime, &[]).unwrap_or(0);
                let Some(key) = keyboard.event(a as u32, b != 0.0, now) else {
                    continue;
                };
                let Some(wmg) = env.call_static_object(wmg_class, get_instance, &[]) else {
                    continue;
                };
                let roots = env.object_field(wmg, roots_field);
                if roots.is_null() {
                    continue;
                }
                println!("input: key code={} pressed={} android={} meta={:#x}",
                    a, b != 0.0, key.code, key.meta);
                let Some(root) = keyboard_root(&env, roots, root_class, size_m, get_m) else {
                    continue;
                };
                // Each window manager focus notification is queued on this
                // root's main Handler before its input event. Without it VRI
                // drops every physical key with "no window focus".
                if let Some(event) = env.new_object(key_class, key_ctor, &[
                    JValue { j: key.down_time }, JValue { j: now },
                    JValue { i: key.action }, JValue { i: key.code },
                    JValue { i: key.repeat }, JValue { i: key.meta },
                    JValue { i: -1 }, // KeyCharacterMap.VIRTUAL_KEYBOARD
                    JValue { i: a as i32 }, JValue { i: 0 },
                    JValue { i: 0x101 }, // InputDevice.SOURCE_KEYBOARD
                ]) {
                    if !event.is_null() {
                        env.call_void(root, dispatch, &[JValue { l: event }]);
                    }
                }
                continue;
            }
            let (action, x, y) = match tag {
                1 | 3 => {
                    last = (a, b);
                    (if pressed { ACTION_MOVE } else { ACTION_HOVER_MOVE }, a, b)
                }
                // BTN_LEFT (0x110) only; other buttons are ignored.
                4 if a as u32 == 0x110 => {
                    if b != 0.0 {
                        pressed = true;
                        down_time = env.call_static_long(clock_class, uptime, &[]).unwrap_or(0);
                        println!("input: tap DOWN at {:.0},{:.0}", last.0, last.1);
                        (ACTION_DOWN, last.0, last.1)
                    } else {
                        pressed = false;
                        println!("input: tap UP at {:.0},{:.0}", last.0, last.1);
                        (ACTION_UP, last.0, last.1)
                    }
                }
                _ => {
                    if tag != 2 {
                        println!("input: unhandled tag={tag} a={a} b={b}");
                    }
                    continue;
                }
            };
            inject(action, x, y, down_time);
        }
    });
}

/// Select the top focusable app window and deliver the window-manager focus
/// callback. LayoutParams types preserve app/dialog ordering even when a dialog
/// was added during onCreate, before its activity's base window was attached.
fn keyboard_root(env: &JniEnv, roots: JObject, root_class: JClass,
    size: JMethodId, get: JMethodId) -> Option<JObject> {
    let view_class = env.find_class("android/view/View")?;
    let params_class = env.find_class("android/view/WindowManager$LayoutParams")?;
    let params_field = env.field_id(root_class, "mWindowAttributes", "Landroid/view/WindowManager$LayoutParams;")?;
    let view_field = env.field_id(root_class, "mView", "Landroid/view/View;")?;
    let flags_field = env.field_id(params_class, "flags", "I")?;
    let type_field = env.field_id(params_class, "type", "I")?;
    let visibility = env.method_id(view_class, "getVisibility", "()I")?;
    let has_focus = env.method_id(view_class, "hasWindowFocus", "()Z")?;
    let change_focus = env.method_id(root_class, "windowFocusChanged", "(Z)V")?;
    let count = env.call_int(roots, size, &[])?;
    let mut windows = Vec::new();
    let mut target = None;
    for i in 0..count {
        let Some(root) = env.call_object(roots, get, &[JValue { i }]) else { continue };
        if root.is_null() { continue; }
        let view = env.object_field(root, view_field);
        let params = env.object_field(root, params_field);
        if view.is_null() || params.is_null() { continue; }
        windows.push((root, view));
        let flags = env.int_field(params, flags_field);
        // FLAG_NOT_FOCUSABLE, and hidden roots, cannot take physical keys.
        if flags & 8 != 0 || env.call_int(view, visibility, &[]) != Some(0) { continue; }
        let rank = (env.int_field(params, type_field), i);
        if target.as_ref().map_or(true, |(best, _)| rank > *best) {
            target = Some((rank, root));
        }
    }
    let (_, target) = target?;
    for (root, view) in windows {
        let focused = root == target;
        if env.call_boolean(view, has_focus, &[]) != Some(focused) {
            env.call_void(root, change_focus, &[JValue { z: focused as u8 }]);
        }
    }
    Some(target)
}

/// Poll the first `ViewRootImpl` from a host thread and print what the draw
/// path is doing.
///
/// The main thread is inside `Looper.loop()` by the time this runs, so the only
/// way to observe `performDraw` from the outside is to read the fields it
/// mutates. `mAttachInfo.mDrawingTime` is written by `ViewRootImpl.draw()`, so a
/// non-zero value proves the draw path was entered.
fn spawn_draw_monitor(env: &JniEnv) {
    if std::env::var_os("PENGUDROID_TRACE_DRAW").is_none() {
        return;
    }
    let Some(vm) = env.get_java_vm() else {
        eprintln!("apphost: draw monitor: no JavaVM");
        return;
    };
    let vm_addr = vm as usize;
    std::thread::spawn(move || {
        let vm = vm_addr as *mut c_void;
        let Some(env) = attach_current_thread(vm) else {
            eprintln!("apphost: draw monitor: attach failed");
            return;
        };
        let Some(wmg_class) = env.find_class("android/view/WindowManagerGlobal") else {
            return;
        };
        let Some(root_class) = env.find_class("android/view/ViewRootImpl") else {
            return;
        };
        let Some(attach_class) = env.find_class("android/view/View$AttachInfo") else {
            return;
        };
        let Some(rect_class) = env.find_class("android/graphics/Rect") else {
            return;
        };
        let Some(surface_class) = env.find_class("android/view/Surface") else {
            return;
        };
        let (Some(get_instance), Some(roots_field)) = (
            env.static_method_id(wmg_class, "getInstance", "()Landroid/view/WindowManagerGlobal;"),
            env.field_id(wmg_class, "mRoots", "Ljava/util/ArrayList;"),
        ) else {
            return;
        };
        let Some(list_class) = env.find_class("java/util/ArrayList") else {
            return;
        };
        let (Some(size_m), Some(get_m)) = (
            env.method_id(list_class, "size", "()I"),
            env.method_id(list_class, "get", "(I)Ljava/lang/Object;"),
        ) else {
            return;
        };
        let attach_field = env.field_id(root_class, "mAttachInfo", "Landroid/view/View$AttachInfo;");
        let drawing_time = env.field_id(attach_class, "mDrawingTime", "J");
        let dirty_field = env.field_id(root_class, "mDirty", "Landroid/graphics/Rect;");
        let is_drawing = env.field_id(root_class, "mIsDrawing", "Z");
        let first_field = env.field_id(root_class, "mFirst", "Z");
        let width_field = env.field_id(root_class, "mWidth", "I");
        let height_field = env.field_id(root_class, "mHeight", "I");
        let surface_field = env.field_id(root_class, "mSurface", "Landroid/view/Surface;");
        let renderer_field = env.field_id(attach_class, "mThreadedRenderer", "Landroid/view/ThreadedRenderer;");
        let is_valid = env.method_id(surface_class, "isValid", "()Z");
        let left = env.field_id(rect_class, "left", "I");
        let top = env.field_id(rect_class, "top", "I");
        let right = env.field_id(rect_class, "right", "I");
        let bottom = env.field_id(rect_class, "bottom", "I");
        let mut last = String::new();
        for tick in 0..40 {
            std::thread::sleep(std::time::Duration::from_millis(500));
            let Some(wmg) = env.call_static_object(wmg_class, get_instance, &[]) else {
                continue;
            };
            let roots = env.object_field(wmg, roots_field);
            if roots.is_null() {
                continue;
            }
            let count = env.call_int(roots, size_m, &[]).unwrap_or(0);
            if count == 0 {
                continue;
            }
            let Some(root) = env.call_object(roots, get_m, &[JValue { i: 0 }]) else {
                continue;
            };
            if root.is_null() {
                continue;
            }
            let attach = attach_field.map(|f| env.object_field(root, f)).unwrap_or(std::ptr::null_mut());
            let time = match (drawing_time, attach.is_null()) {
                (Some(f), false) => env.long_field(attach, f),
                _ => -1,
            };
            let dirty = dirty_field.map(|f| env.object_field(root, f)).unwrap_or(std::ptr::null_mut());
            let dirty_str = if dirty.is_null() {
                "null".to_string()
            } else {
                format!(
                    "[{},{},{},{}]",
                    left.map(|f| env.int_field(dirty, f)).unwrap_or(0),
                    top.map(|f| env.int_field(dirty, f)).unwrap_or(0),
                    right.map(|f| env.int_field(dirty, f)).unwrap_or(0),
                    bottom.map(|f| env.int_field(dirty, f)).unwrap_or(0),
                )
            };
            let surface = surface_field.map(|f| env.object_field(root, f)).unwrap_or(std::ptr::null_mut());
            let valid = match (is_valid, surface.is_null()) {
                (Some(m), false) => env.call_boolean(surface, m, &[]).unwrap_or(false),
                _ => false,
            };
            let renderer = renderer_field.map(|f| env.object_field(attach, f)).unwrap_or(std::ptr::null_mut());
            let line = format!(
                "tick={tick} drawingTime={time} dirty={dirty_str} isDrawing={:?} first={:?} w={:?} h={:?} surfaceValid={valid} renderer={}",
                is_drawing.map(|f| env.boolean_field(root, f)),
                first_field.map(|f| env.boolean_field(root, f)),
                width_field.map(|f| env.int_field(root, f)),
                height_field.map(|f| env.int_field(root, f)),
                class_name_of(&env, renderer).unwrap_or_else(|| "null".into()),
            );
            if line != last {
                println!("apphost: draw monitor {line}");
                last = line;
            }
        }
    });
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
    // `PENGUDROID_INTENT_DATA=<uri>`: give the launch intent a data URI and the
    // `VIEW` action, the way a share/opener intent arrives on a device. That is
    // how an activity that handles external URLs (NewPipe's `RouterActivity`)
    // can be pointed at a specific stream without any UI interaction.
    if let Ok(uri) = std::env::var("PENGUDROID_INTENT_DATA") {
        let text = env.new_string(&uri);
        let uri_class = env.find_class("android/net/Uri");
        let parse = uri_class.and_then(|c| {
            env.static_method_id(c, "parse", "(Ljava/lang/String;)Landroid/net/Uri;")
        });
        let set_data = env.method_id(
            intent_class,
            "setData",
            "(Landroid/net/Uri;)Landroid/content/Intent;",
        );
        let set_action = env.method_id(
            intent_class,
            "setAction",
            "(Ljava/lang/String;)Landroid/content/Intent;",
        );
        if let (Some(uri_class), Some(parse), Some(set_data)) = (uri_class, parse, set_data) {
            if let Some(parsed) = env.call_static_object(uri_class, parse, &[JValue { l: text }]) {
                env.call_object(intent, set_data, &[JValue { l: parsed }]);
                if let Some(set_action) = set_action {
                    let view = env.new_string("android.intent.action.VIEW");
                    env.call_object(intent, set_action, &[JValue { l: view }]);
                }
                println!("apphost: launch intent data = {uri}");
            } else {
                eprintln!("apphost: Uri.parse returned null for {uri}");
            }
        } else {
            eprintln!("apphost: cannot attach a data URI to the launch intent");
        }
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
            // `handleResumeActivity` calls `makeVisible()` only when it decides the
            // activity will be visible; drive it explicitly so the decor view is
            // attached and the first traversal reaches `performDraw`.
            force_visible(env, record);
            true
        }
        None => {
            eprintln!("apphost: handleLaunchActivity threw");
            false
        }
    }
}

/// Force `ViewRootImpl`'s window frame to the surface size.
///
/// The frame reaches `ViewRootImpl` through the `ClientWindowFrames`
/// out-parameter of `IWindowSession.relayout`, which the broker answers. When
/// that reply is not picked up (or the traversal runs before it), the frame
/// stays `0x0`; `performMeasure` then measures the whole tree to nothing and
/// `performDraw` finds an empty dirty rect, so hwui is never asked to draw.
///
/// Setting it here is the same kind of nudge the rest of this file makes: the
/// framework object is mutated in place through JNI, so the very next traversal
/// measures against the real surface.
fn force_window_frame(env: &JniEnv, root: JObject, width: i32, height: i32) {
    let Some(root_class) = env.find_class("android/view/ViewRootImpl") else {
        return;
    };
    let Some(rect_class) = env.find_class("android/graphics/Rect") else {
        return;
    };
    let set = env.method_id(rect_class, "set", "(IIII)V");

    // Android 13 moved the frame into `mClientWindowFrames.frame`; older trees
    // still have a plain `mWinFrame`. Try both.
    let mut target = env
        .field_id(root_class, "mWinFrame", "Landroid/graphics/Rect;")
        .map(|f| env.object_field(root, f));
    if target.map_or(true, |r| r.is_null()) {
        target = env
            .field_id(
                root_class,
                "mClientWindowFrames",
                "Landroid/window/ClientWindowFrames;",
            )
            .map(|f| env.object_field(root, f))
            .filter(|frames| !frames.is_null())
            .and_then(|frames| {
                let frames_class = env.find_class("android/window/ClientWindowFrames")?;
                let field = env.field_id(frames_class, "frame", "Landroid/graphics/Rect;")?;
                Some(env.object_field(frames, field))
            });
    }
    let Some(rect) = target.filter(|r| !r.is_null()) else {
        eprintln!("apphost: no window frame field on ViewRootImpl");
        return;
    };
    let Some(set) = set else {
        return;
    };
    if env.call_void(
        rect,
        set,
        &[
            JValue { i: 0 },
            JValue { i: 0 },
            JValue { i: width },
            JValue { i: height },
        ],
    ) {
        println!("apphost: window frame set to {width}x{height}");
    } else {
        eprintln!("apphost: Rect.set on the window frame threw");
    }
}

/// `PENGUDROID_DUMP_ROOTS=<ms>`: print every window's real frame after a delay.
///
/// The `dump_draw_state` call in `force_visible` runs *before* the first
/// traversal, so it only sees the pre-traversal sentinels (`w=-1`,
/// `mViewVisibility=GONE`). This attaches to the VM on a timer and reports what
/// `ViewRootImpl` actually settled on, which is the only way to see the frame a
/// second window (a dialog) was given.
fn schedule_roots_dump(env: &JniEnv) {
    let Some(ms) = std::env::var("PENGUDROID_DUMP_ROOTS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
    else {
        return;
    };
    let Some(vm) = env.get_java_vm() else { return };
    let vm_addr = vm as usize;
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(ms));
        let vm = vm_addr as *mut c_void;
        let Some(env) = attach_current_thread(vm) else { return };
        let (Some(wmg_class), Some(root_class)) = (
            env.find_class("android/view/WindowManagerGlobal"),
            env.find_class("android/view/ViewRootImpl"),
        ) else {
            return;
        };
        let (Some(get_instance), Some(roots_field)) = (
            env.static_method_id(wmg_class, "getInstance", "()Landroid/view/WindowManagerGlobal;"),
            env.field_id(wmg_class, "mRoots", "Ljava/util/ArrayList;"),
        ) else {
            return;
        };
        let Some(list_class) = env.find_class("java/util/ArrayList") else { return };
        let (Some(list_get), Some(list_size)) = (
            env.method_id(list_class, "get", "(I)Ljava/lang/Object;"),
            env.method_id(list_class, "size", "()I"),
        ) else {
            return;
        };
        let Some(wmg) = env.call_static_object(wmg_class, get_instance, &[]) else { return };
        let roots = env.object_field(wmg, roots_field);
        let count = env.call_int(roots, list_size, &[]).unwrap_or(0);
        let int = |name: &str, obj: JObject| -> Option<i32> {
            let f = env.field_id(root_class, name, "I")?;
            Some(env.int_field(obj, f))
        };
        for i in 0..count {
            let Some(root) = env.call_object(roots, list_get, &[JValue { i }]) else { continue };
            if root.is_null() {
                continue;
            }
            let mut frame = String::from("none");
            let rect_source = env
                .field_id(root_class, "mWinFrame", "Landroid/graphics/Rect;")
                .map(|f| env.object_field(root, f))
                .filter(|r| !r.is_null());
            if let (Some(rect), Some(rect_class)) = (rect_source, env.find_class("android/graphics/Rect")) {
                if let Some(desc) = env.method_id(rect_class, "toString", "()Ljava/lang/String;") {
                    frame = env
                        .call_object(rect, desc, &[])
                        .and_then(|s| env.get_string(s))
                        .unwrap_or_else(|| "?".into());
                }
            }
            println!(
                "apphost: root {i} settled w={:?} h={:?} mWinFrame={frame}",
                int("mWidth", root),
                int("mHeight", root),
            );
        }
    });
}

/// `Object.getClass().getName()` for diagnostics.
fn class_name_of(env: &JniEnv, obj: JObject) -> Option<String> {
    if obj.is_null() {
        return None;
    }
    let obj_class = env.find_class("java/lang/Object")?;
    let get_class = env.method_id(obj_class, "getClass", "()Ljava/lang/Class;")?;
    let cls = env.call_object(obj, get_class, &[])?;
    let class_class = env.find_class("java/lang/Class")?;
    let get_name = env.method_id(class_class, "getName", "()Ljava/lang/String;")?;
    env.call_object(cls, get_name, &[])
        .and_then(|n| env.get_string(n))
}

/// Print the flags `performTraversals` consults before it decides to draw.
///
/// `performDraw` is skipped when `cancelDraw` is true, and `cancelDraw` is
/// `mAttachInfo.mTreeObserver.dispatchOnPreDraw() || !isViewVisible`. This says
/// which of the two fired and who the pre-draw listeners are.
fn dump_draw_state(env: &JniEnv, root: JObject, index: usize) {
    let Some(root_class) = env.find_class("android/view/ViewRootImpl") else {
        return;
    };
    let boolean = |name: &str| -> Option<bool> {
        let f = env.field_id(root_class, name, "Z")?;
        Some(env.boolean_field(root, f))
    };
    let int = |name: &str| -> Option<i32> {
        let f = env.field_id(root_class, name, "I")?;
        Some(env.int_field(root, f))
    };
    println!(
        "apphost: root {index} flags first={:?} added={:?} stopped={:?} inTraversal={:?} appVisible={:?} reportNextDraw={:?} w={:?} h={:?} viewVisibility={:?}",
        boolean("mFirst"),
        boolean("mAdded"),
        boolean("mStopped"),
        boolean("mIsInTraversal"),
        boolean("mAppVisible"),
        boolean("mReportNextDraw"),
        int("mWidth"),
        int("mHeight"),
        int("mViewVisibility"),
    );
    let Some(attach_field) = env.field_id(root_class, "mAttachInfo", "Landroid/view/View$AttachInfo;") else {
        return;
    };
    let attach = env.object_field(root, attach_field);
    if attach.is_null() {
        println!("apphost: root {index} mAttachInfo is null");
        return;
    }
    let Some(attach_class) = env.find_class("android/view/View$AttachInfo") else {
        return;
    };
    if let Some(f) = env.field_id(attach_class, "mThreadedRenderer", "Landroid/view/ThreadedRenderer;") {
        let renderer = env.object_field(attach, f);
        println!(
            "apphost: root {index} mThreadedRenderer = {}",
            class_name_of(env, renderer).unwrap_or_else(|| "null".into())
        );
        if !renderer.is_null() {
            // `isEnabled` is declared on the superclass; `GetMethodID` searches
            // the hierarchy, so asking the concrete class works.
            if let Some(renderer_class) = env.find_class("android/view/ThreadedRenderer") {
                if let Some(is_enabled) = env.method_id(renderer_class, "isEnabled", "()Z") {
                    println!(
                        "apphost: root {index} mThreadedRenderer.isEnabled() = {:?}",
                        env.call_boolean(renderer, is_enabled, &[])
                    );
                }
            }
        }
    }
    if let Some(f) = env.field_id(attach_class, "mWindowVisibility", "I") {
        println!(
            "apphost: root {index} mWindowVisibility = {}",
            env.int_field(attach, f)
        );
    }
    if let Some(f) = env.field_id(attach_class, "mDisplayState", "I") {
        println!(
            "apphost: root {index} mDisplayState = {} (0=UNKNOWN 1=OFF 2=ON)",
            env.int_field(attach, f)
        );
    }
    if let Some(display_field) = env.field_id(root_class, "mDisplay", "Landroid/view/Display;") {
        let display = env.object_field(root, display_field);
        if !display.is_null() {
            if let Some(display_class) = env.find_class("android/view/Display") {
                if let Some(get_state) = env.method_id(display_class, "getState", "()I") {
                    println!(
                        "apphost: root {index} mDisplay.getState() = {:?}",
                        env.call_int(display, get_state, &[])
                    );
                }
            }
        }
    }
    let Some(obs_field) = env.field_id(attach_class, "mTreeObserver", "Landroid/view/ViewTreeObserver;") else {
        return;
    };
    let observer = env.object_field(attach, obs_field);
    if observer.is_null() {
        println!("apphost: root {index} mTreeObserver is null");
        return;
    }
    let Some(obs_class) = env.find_class("android/view/ViewTreeObserver") else {
        return;
    };
    for (field_name, label) in [
        ("mOnPreDrawListeners", "preDraw"),
        ("mOnGlobalLayoutListeners", "globalLayout"),
    ] {
        let Some(f) = env.field_id(obs_class, field_name, "Landroid/view/ViewTreeObserver$CopyOnWriteArray;") else {
            continue;
        };
        let array = env.object_field(observer, f);
        if array.is_null() {
            println!("apphost: root {index} {label} listeners = null");
            continue;
        }
        let Some(array_class) = env.find_class("android/view/ViewTreeObserver$CopyOnWriteArray") else {
            continue;
        };
        let Some(data_field) = env.field_id(array_class, "mData", "[Ljava/lang/Object;") else {
            continue;
        };
        let data = env.object_field(array, data_field);
        if data.is_null() {
            println!("apphost: root {index} {label} listeners = empty");
            continue;
        }
        let len = env.array_length(data);
        let mut names = Vec::new();
        for i in 0..len {
            let item = env.array_get(data, i);
            if item.is_null() {
                continue;
            }
            names.push(class_name_of(env, item).unwrap_or_else(|| "?".into()));
        }
        println!(
            "apphost: root {index} {label} listeners ({len}) = {}",
            if names.is_empty() { "-".to_string() } else { names.join(", ") }
        );
    }
}

/// Attach the decor view and ask for a traversal.
///
/// Without this the window is created but `ViewRootImpl` never reaches
/// `performDraw`: `isViewVisible()` stays false, so hwui is initialised (it
/// creates its EGL surface) and then never draws a frame.
fn force_visible(env: &JniEnv, record: JObject) {
    let Some(record_class) = env.find_class("android/app/ActivityThread$ActivityClientRecord") else {
        return;
    };
    let Some(activity_field) = env.field_id(record_class, "activity", "Landroid/app/Activity;") else {
        return;
    };
    let activity = env.object_field(record, activity_field);
    if activity.is_null() {
        return;
    }
    let Some(activity_class) = env.find_class("android/app/Activity") else {
        return;
    };
    if let Some(make_visible) = env.method_id(activity_class, "makeVisible", "()V") {
        if !env.call_void(activity, make_visible, &[]) {
            eprintln!("apphost: Activity.makeVisible threw");
            return;
        }
        println!("apphost: Activity.makeVisible() done");
    }

    // Reach the real `ViewRootImpl` through
    // `WindowManagerGlobal.getInstance().mRoots` (both @hide/private; JNI ignores
    // access modifiers). `View.getViewRootImpl()` is null for this decor view.
    let Some(wmg_class) = env.find_class("android/view/WindowManagerGlobal") else {
        return;
    };
    let Some(get_instance) = env.static_method_id(
        wmg_class,
        "getInstance",
        "()Landroid/view/WindowManagerGlobal;",
    ) else {
        return;
    };
    let Some(wmg) = env.call_static_object(wmg_class, get_instance, &[]) else {
        return;
    };
    if wmg.is_null() {
        return;
    }
    let Some(roots_field) = env.field_id(wmg_class, "mRoots", "Ljava/util/ArrayList;") else {
        return;
    };
    let roots = env.object_field(wmg, roots_field);
    if roots.is_null() {
        return;
    }
    let Some(list_class) = env.find_class("java/util/ArrayList") else {
        return;
    };
    let Some(size_m) = env.method_id(list_class, "size", "()I") else {
        return;
    };
    let Some(get_m) = env.method_id(list_class, "get", "(I)Ljava/lang/Object;") else {
        return;
    };
    let count = env.call_int(roots, size_m, &[]).unwrap_or(0);
    println!("apphost: WindowManagerGlobal.mRoots size = {count}");
    let Some(root_class) = env.find_class("android/view/ViewRootImpl") else {
        return;
    };
    let Some(schedule) = env.method_id(root_class, "scheduleTraversals", "()V") else {
        return;
    };
    for index in 0..count {
        let Some(root) = env.call_object(roots, get_m, &[JValue { i: index }]) else {
            continue;
        };
        if root.is_null() {
            continue;
        }
        // `isViewVisible()` is false while `mAppVisible` is false, and
        // `performTraversals` then skips `performDraw` entirely. On a device the
        // window manager flips this; here the field is set directly.
        if let Some(f) = env.field_id(root_class, "mAppVisible", "Z") {
            env.set_boolean_field(root, f, true);
        }
        // `isViewVisible()` also needs the root view to be VISIBLE.
        if let Some(view_field) = env.field_id(root_class, "mView", "Landroid/view/View;") {
            let view = env.object_field(root, view_field);
            if !view.is_null() {
                if let Some(view_class) = env.find_class("android/view/View") {
                    if let Some(get_vis) = env.method_id(view_class, "getVisibility", "()I") {
                        let vis = env.call_int(view, get_vis, &[]).unwrap_or(-1);
                        println!("apphost: root {index} mView.getVisibility() = {vis} (0 = VISIBLE)");
                    }
                }
            }
        }
        // `performDraw` also requires a valid surface. The SurfaceControl we
        // hand back from `relayout` does not survive `SurfaceControl.readFromParcel`
        // (the parcel layout is version specific), so build one through the
        // framework's own builder - that path goes through
        // `SurfaceComposerClient.createSurface`, which the broker answers - and
        // copy it into `mSurface`.
        if let Some(sc_field) = env.field_id(root_class, "mSurfaceControl", "Landroid/view/SurfaceControl;") {
            let sc = env.object_field(root, sc_field);
            let valid = if sc.is_null() {
                false
            } else {
                env.find_class("android/view/SurfaceControl")
                    .and_then(|c| env.method_id(c, "isValid", "()Z"))
                    .and_then(|m| env.call_boolean(sc, m, &[]))
                    .unwrap_or(false)
            };
            println!("apphost: root {index} mSurfaceControl.isValid() = {valid}");
            if !valid {
                if let Some(built) = build_surface_control(env) {
                    // `relayoutWindow` ends with
                    // `if (mSurfaceControl.isValid()) { ... } else { destroySurface(); }`,
                    // so fixing only `mSurface` is not enough: the next traversal
                    // releases it again. The control has to be valid too.
                    if let Some(sc_field) =
                        env.field_id(root_class, "mSurfaceControl", "Landroid/view/SurfaceControl;")
                    {
                        env.set_object_field(root, sc_field, built);
                        println!("apphost: root {index} mSurfaceControl := built");
                    }
                    // With BLAST enabled `relayoutWindow` builds a `BLASTBufferQueue`
                    // (which needs a real consumer); disabling it makes the same
                    // method take the plain `mSurface.copyFrom(mSurfaceControl)` path.
                    // `PENGUDROID_BLAST=1` keeps BLAST enabled so the window root
                    // builds a `BLASTBufferQueue` like a device does; the default
                    // stays on the legacy `copyFrom` path, which is what the
                    // window has been validated on so far.
                    if std::env::var_os("PENGUDROID_BLAST").is_none() {
                        if let Some(blast_field) = env.field_id(root_class, "mForceDisableBLAST", "Z") {
                            env.set_boolean_field(root, blast_field, true);
                            println!("apphost: root {index} mForceDisableBLAST := true");
                        }
                    } else {
                        println!("apphost: root {index} BLAST left enabled (PENGUDROID_BLAST)");
                    }
                    if let Some(surface_field) =
                        env.field_id(root_class, "mSurface", "Landroid/view/Surface;")
                    {
                        let surface = env.object_field(root, surface_field);
                        if !surface.is_null() {
                            if let Some(surface_class) = env.find_class("android/view/Surface") {
                                if let Some(copy_from) = env.method_id(
                                    surface_class,
                                    "copyFrom",
                                    "(Landroid/view/SurfaceControl;)V",
                                ) {
                                    if env.call_void(surface, copy_from, &[JValue { l: built }]) {
                                        println!("apphost: root {index} mSurface.copyFrom(built) done");
                                        if let Some(is_valid) =
                                            env.method_id(surface_class, "isValid", "()Z")
                                        {
                                            let ok = env.call_boolean(surface, is_valid, &[]);
                                            println!("apphost: root {index} mSurface.isValid() = {ok:?}");
                                        }
                                    } else {
                                        eprintln!("apphost: root {index} mSurface.copyFrom threw");
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        force_window_frame(env, root, SURFACE_W, SURFACE_H);
        dump_draw_state(env, root, index as usize);
        schedule_roots_dump(env);
        // `performTraversals` must run inside a Choreographer frame callback:
        // calling it directly makes `Choreographer.getFrameTimeNanos()` throw
        // "must only be called as part of a callback while a frame is in
        // progress". `scheduleTraversals()` posts the framework's own
        // TraversalRunnable, which is the correct entry point.
        if env.call_void(root, schedule, &[]) {
            println!("apphost: scheduleTraversals() done for root {index}");
        } else {
            eprintln!("apphost: scheduleTraversals threw for root {index}");
        }
    }
}

/// Build a `SurfaceControl` through the framework's own builder.
///
/// `SurfaceControl.Builder.build()` calls
/// `SurfaceComposerClient.createSurface`, which the broker answers with a real
/// handle, so the resulting object is valid even though the one we hand back
/// from `relayout` is not.
fn build_surface_control(env: &JniEnv) -> Option<JObject> {
    let builder_class = env.find_class("android/view/SurfaceControl$Builder")?;
    let ctor = env.method_id(builder_class, "<init>", "()V")?;
    let builder = env.new_object(builder_class, ctor, &[])?;
    let name = env.new_string("pengudroid");
    if let Some(set_name) = env.method_id(
        builder_class,
        "setName",
        "(Ljava/lang/String;)Landroid/view/SurfaceControl$Builder;",
    ) {
        env.call_object(builder, set_name, &[JValue { l: name }]);
    }
    if let Some(set_size) = env.method_id(
        builder_class,
        "setBufferSize",
        "(II)Landroid/view/SurfaceControl$Builder;",
    ) {
        env.call_object(
            builder,
            set_size,
            &[JValue { i: SURFACE_W }, JValue { i: SURFACE_H }],
        );
    }
    let build = env.method_id(builder_class, "build", "()Landroid/view/SurfaceControl;")?;
    let sc = env.call_object(builder, build, &[])?;
    if sc.is_null() {
        eprintln!("apphost: SurfaceControl.Builder.build returned null");
        return None;
    }
    println!("apphost: built a SurfaceControl via SurfaceControl.Builder");
    Some(sc)
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

/// Drive the real AIDL proxy so focus tests cover request argument parsing,
/// rather than only the hosted policy's Rust state machine.
pub fn probe_audio_focus(env: &JniEnv) -> bool {
    let run = || -> Option<()> {
        let _frame = env.local_frame(32)?;
        let sm = env.find_class("android/os/ServiceManager")?;
        let get = env.static_method_id(sm, "getService", "(Ljava/lang/String;)Landroid/os/IBinder;")?;
        let name = env.new_string("audio");
        let binder = env.call_static_object(sm, get, &[JValue { l: name }])?;
        if binder.is_null() { return None; }
        let stub = env.find_class("android/media/IAudioService$Stub")?;
        let as_interface = env.static_method_id(stub, "asInterface", "(Landroid/os/IBinder;)Landroid/media/IAudioService;")?;
        let service = env.call_static_object(stub, as_interface, &[JValue { l: binder }])?;
        if service.is_null() { return None; }
        let (builder_class, builder) = new_instance(env, "android/media/AudioAttributes$Builder")?;
        let build = env.method_id(builder_class, "build", "()Landroid/media/AudioAttributes;")?;
        let attrs = env.call_object(builder, build, &[])?;
        let (_, client) = new_instance(env, "android/os/Binder")?;
        let client_id = env.new_string("pengudroid-focus-probe");
        let package = env.new_string("org.pengudroid.probe");
        let class = env.find_class("android/media/IAudioService")?;
        let request = env.method_id(class, "requestAudioFocus",
            "(Landroid/media/AudioAttributes;ILandroid/os/IBinder;Landroid/media/IAudioFocusDispatcher;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;ILandroid/media/audiopolicy/IAudioPolicyCallback;I)I")?;
        let null = std::ptr::null_mut();
        let result = env.call_int(service, request, &[
            JValue { l: attrs }, JValue { i: 1 }, JValue { l: client }, JValue { l: null },
            JValue { l: client_id }, JValue { l: package }, JValue { l: null },
            JValue { i: 0 }, JValue { l: null }, JValue { i: 33 },
        ])?;
        let current = env.method_id(class, "getCurrentAudioFocus", "()I")?;
        if result != 1 || env.call_int(service, current, &[])? != 1 { return None; }
        let abandon = env.method_id(class, "abandonAudioFocus",
            "(Landroid/media/IAudioFocusDispatcher;Ljava/lang/String;Landroid/media/AudioAttributes;Ljava/lang/String;)I")?;
        let result = env.call_int(service, abandon, &[
            JValue { l: null }, JValue { l: client_id }, JValue { l: attrs }, JValue { l: package },
        ])?;
        if result != 1 || env.call_int(service, current, &[])? != 0 { return None; }
        println!("audio: AIDL focus granted, current gain verified, abandon cleared");
        Some(())
    };
    run().is_some()
}

/// Exercise Android's local layout, the path that used to turn a good frame
/// into 0x0 after a subsequent relayout. This checks computed output, not just
/// the configuration fields written by the host.
/// Exercise the real framework constructor and destroy path twice, as a
/// SurfaceView recreation would. Consumer exit is checked by the ART test log.
pub fn probe_blast_lifecycle(env: &JniEnv) -> bool {
    let run = || -> Option<()> {
        let _frame = env.local_frame(32)?;
        let class = env.find_class("android/graphics/BLASTBufferQueue")?;
        let ctor = env.method_id(class, "<init>", "(Ljava/lang/String;Z)V")?;
        let destroy = env.method_id(class, "destroy", "()V")?;
        let native = env.field_id(class, "mNativeObject", "J")?;
        let update = env.static_method_id(class, "nativeUpdate", "(JJJJI)V")?;
        let mut previous = 0;
        for _ in 0..2 {
            let name = env.new_string("PenguDroid lifecycle probe");
            let queue = env.new_object(class, ctor, &[JValue { l: name }, JValue { z: 0 }])?;
            let pointer = env.long_field(queue, native);
            if pointer == 0 || pointer == previous { return None; }
            previous = pointer;
            for (width, height) in [(720, 405), (1280, 720)] {
                if !env.call_static_void(class, update, &[
                    JValue { j: pointer }, JValue { j: 0 },
                    JValue { j: width }, JValue { j: height }, JValue { i: 4 },
                ]) { return None; }
            }
            // Exercise destruction after the consumer has had a chance to start.
            std::thread::sleep(std::time::Duration::from_millis(100));
            if !env.call_void(queue, destroy, &[]) || env.long_field(queue, native) != 0 {
                return None;
            }
        }
        // Also cover a consumer waiting between daemon connection attempts.
        std::thread::sleep(std::time::Duration::from_millis(500));
        println!("blast: real framework create/destroy/recreate passed");
        Some(())
    };
    if run().is_some() { true } else {
        env.describe_exception();
        false
    }
}

pub fn probe_window_configuration(env: &JniEnv) -> bool {
    let run = || -> Option<()> {
        let _frame = env.local_frame(48)?;
        let config = new_configuration(env)?;
        let config_class = env.find_class("android/content/res/Configuration")?;
        let window_field = env.field_id(config_class, "windowConfiguration", "Landroid/app/WindowConfiguration;")?;
        let window = env.object_field(config, window_field);
        if window.is_null() { return None; }
        let window_class = env.find_class("android/app/WindowConfiguration")?;
        let get_bounds = env.method_id(window_class, "getBounds", "()Landroid/graphics/Rect;")?;
        let bounds = env.call_object(window, get_bounds, &[])?;
        let (params_class, params) = new_instance(env, "android/view/WindowManager$LayoutParams")?;
        for (name, value) in [("width", -1), ("height", -1), ("type", 1)] {
            env.set_int_field(params, env.field_id(params_class, name, "I")?, value);
        }
        let (insets_class, insets) = new_instance(env, "android/view/InsetsState")?;
        let set_display = env.method_id(insets_class, "setDisplayFrame", "(Landroid/graphics/Rect;)V")?;
        env.call_void(insets, set_display, &[JValue { l: bounds }]);
        let (_, safe) = new_instance(env, "android/graphics/Rect")?;
        let safe_method = env.method_id(insets_class, "getDisplayCutoutSafe", "(Landroid/graphics/Rect;)V")?;
        env.call_void(insets, safe_method, &[JValue { l: safe }]);
        let (_, visible) = new_instance(env, "android/view/InsetsVisibilities")?;
        let (frames_class, frames) = new_instance(env, "android/window/ClientWindowFrames")?;
        let (layout_class, layout) = new_instance(env, "android/view/WindowLayout")?;
        let compute = env.method_id(layout_class, "computeFrames",
            "(Landroid/view/WindowManager$LayoutParams;Landroid/view/InsetsState;Landroid/graphics/Rect;Landroid/graphics/Rect;IIILandroid/view/InsetsVisibilities;FLandroid/window/ClientWindowFrames;)V")?;
        if !env.call_void(layout, compute, &[
            JValue { l: params }, JValue { l: insets }, JValue { l: safe }, JValue { l: bounds },
            JValue { i: 1 }, JValue { i: SURFACE_W }, JValue { i: SURFACE_H },
            JValue { l: visible }, JValue { f: 1.0 }, JValue { l: frames },
        ]) { return None; }
        let frame = env.object_field(frames, env.field_id(frames_class, "frame", "Landroid/graphics/Rect;")?);
        if frame.is_null() { return None; }
        let rect_class = env.find_class("android/graphics/Rect")?;
        let width = env.method_id(rect_class, "width", "()I")?;
        let height = env.method_id(rect_class, "height", "()I")?;
        let (w, h) = (env.call_int(frame, width, &[])?, env.call_int(frame, height, &[])?);
        println!("window: local WindowLayout.computeFrames = {w}x{h}");
        (w == SURFACE_W && h == SURFACE_H).then_some(())
    };
    run().is_some()
}

/// Headless end-to-end check of service lookup, native parcel decoding and
/// Android's own Unicode conversion. Called by the ART diagnostic entry point.
pub fn probe_keyboard(env: &JniEnv) -> bool {
    let run = || -> Option<()> {
        install_virtual_keyboard(env)?;
        let _frame = env.local_frame(16)?;
        let class = env.find_class("android/view/KeyCharacterMap")?;
        let load = env.static_method_id(class, "load", "(I)Landroid/view/KeyCharacterMap;")?;
        let map = env.call_static_object(class, load, &[JValue { i: -1 }])?;
        if map.is_null() { return None; }
        let get = env.method_id(class, "get", "(II)I")?;
        for (code, meta, expected) in [(29, 0, 97), (29, 1, 65), (29, 0x100000, 65),
            (29, 0x100001, 97), (8, 1, 33), (29, 0x3000, 0)] {
            if env.call_int(map, get, &[JValue { i: code }, JValue { i: meta }])? != expected {
                return None;
            }
        }
        let device_class = env.find_class("android/view/InputDevice")?;
        let get_device = env.static_method_id(device_class, "getDevice", "(I)Landroid/view/InputDevice;")?;
        let device = env.call_static_object(device_class, get_device, &[JValue { i: -1 }])?;
        if device.is_null() { return None; }
        let virtual_method = env.method_id(device_class, "isVirtual", "()Z")?;
        if env.call_boolean(device, virtual_method, &[]) != Some(true) { return None; }
        // A genuinely absent device stays absent; only KeyCharacterMap.load
        // (not InputDevice lookup) is allowed to use the virtual fallback.
        let missing = env.call_static_object(device_class, get_device, &[JValue { i: 123456 }])?;
        if !missing.is_null() { return None; }
        println!("keyboard: InputManager -> InputDevice(-1) -> native KeyCharacterMap: 6 Unicode cases passed");
        Some(())
    };
    run().is_some()
}

/// Publish a real InputDevice and native KeyCharacterMap through IInputManager.
/// The framework reads the map itself; key events still travel through VRI.
fn install_virtual_keyboard(env: &JniEnv) -> Option<()> {
    let _frame = env.local_frame(32)?;
    let parcel_class = env.find_class("android/os/Parcel")?;
    let obtain = env.static_method_id(parcel_class, "obtain", "()Landroid/os/Parcel;")?;
    let parcel = env.call_static_object(parcel_class, obtain, &[])?;
    let raw = crate::keyboard::virtual_character_map_parcel();
    let bytes = env.new_byte_array(raw.len() as i32)?;
    env.set_byte_array_region(bytes, 0, &raw);
    let unmarshall = env.method_id(parcel_class, "unmarshall", "([BII)V")?;
    if !env.call_void(parcel, unmarshall, &[JValue { l: bytes }, JValue { i: 0 }, JValue { i: raw.len() as i32 }]) {
        return None;
    }
    let reset = env.method_id(parcel_class, "setDataPosition", "(I)V")?;
    env.call_void(parcel, reset, &[JValue { i: 0 }]);
    let map_class = env.find_class("android/view/KeyCharacterMap")?;
    let ctor = env.method_id(map_class, "<init>", "(Landroid/os/Parcel;)V")?;
    let map = env.new_object(map_class, ctor, &[JValue { l: parcel }])?;
    let recycle = env.method_id(parcel_class, "recycle", "()V")?;
    env.call_void(parcel, recycle, &[]);
    if map.is_null() { return None; }
    // Exercise the actual libinput map, not a Rust model of its result. A
    // sysroot with a different native parcel format fails here before launch.
    let get = env.method_id(map_class, "get", "(II)I")?;
    for (code, meta, expected) in [(29, 0, 97), (29, 1, 65), (29, 0x100000, 65),
        (29, 0x100001, 97), (8, 1, 33), (29, 0x3000, 0)] {
        let actual = env.call_int(map, get, &[JValue { i: code }, JValue { i: meta }])?;
        if actual != expected {
            eprintln!("apphost: keyboard map mismatch: code={code} meta={meta:#x} expected={expected} actual={actual}");
            return None;
        }
    }
    let device_class = env.find_class("android/view/InputDevice")?;
    let ctor = env.method_id(device_class, "<init>",
        "(IIILjava/lang/String;IILjava/lang/String;ZIILandroid/view/KeyCharacterMap;ZZZZZ)V")?;
    let name = env.new_string("PenguDroid Virtual Keyboard");
    let descriptor = env.new_string("pengudroid:virtual-keyboard");
    let device = env.new_object(device_class, ctor, &[
        JValue { i: -1 }, JValue { i: 1 }, JValue { i: 0 }, JValue { l: name },
        JValue { i: 0 }, JValue { i: 0 }, JValue { l: descriptor }, JValue { z: 0 },
        JValue { i: 0x101 }, JValue { i: 2 }, JValue { l: map },
        JValue { z: 0 }, JValue { z: 0 }, JValue { z: 0 }, JValue { z: 0 }, JValue { z: 0 },
    ])?;
    if device.is_null() { return None; }
    let data = marshal_object(env, device, "android/view/InputDevice")?;
    install_reply(&data, "__pengudroid_binder_set_input_device");
    println!("apphost: virtual keyboard installed; native character map validated");
    Some(())
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
    // NewPipe's `PackageValidator` (the media3 `MediaBrowserServiceCompat`
    // caller check) reads `PackageInfo.signatures` for the `"android"` package
    // while its `PlayerService` is being created and throws "Platform signature
    // not found" when the array is null; `PlayerService` then never starts.
    // There is no signing certificate here, so install one fixed placeholder.
    // The validator only compares this signature with the caller's, and both
    // answers come from this same reply, so their SHA-256 hashes match.
    if let Some(signature_class) = env.find_class("android/content/pm/Signature") {
        if let Some(signature_ctor) = env.method_id(signature_class, "<init>", "([B)V") {
            if let Some(bytes) = env.new_byte_array(32) {
                let digest: Vec<u8> = (0u8..32).map(|b| b.wrapping_mul(7)).collect();
                env.set_byte_array_region(bytes, 0, &digest);
                if let Some(signature) =
                    env.new_object(signature_class, signature_ctor, &[JValue { l: bytes }])
                {
                    if let Some(array) =
                        env.new_object_array(1, signature_class, std::ptr::null_mut())
                    {
                        env.set_object_array_element(array, 0, signature);
                        if let Some(id) = env.field_id(
                            pi_class,
                            "signatures",
                            "[Landroid/content/pm/Signature;",
                        ) {
                            env.set_object_field(pi, id, array);
                        }
                    }
                }
            }
        }
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

/// Install the platform singletons `system_server` normally creates.
///
/// Both `MediaFrameworkPlatformInitializer` and `TelephonyFrameworkInitializer`
/// only *hold* their manager object; `SystemServer` sets it once during boot and
/// nothing creates it lazily. The framework dereferences them unconditionally,
/// so without this a `MediaSession` cannot be constructed (NewPipe's
/// `PlayerService.onCreate` dies) and `TelephonyManager.getITelephony` NPEs
/// (ExoPlayer's `DefaultBandwidthMeter` dies in `onStartCommand`).
fn install_platform_service_managers(env: &JniEnv) {
    install_platform_singleton(
        env,
        "android/media/MediaFrameworkPlatformInitializer",
        "android/media/MediaServiceManager",
        "setMediaServiceManager",
    );
    install_platform_singleton(
        env,
        "android/telephony/TelephonyFrameworkInitializer",
        "android/os/TelephonyServiceManager",
        "setTelephonyServiceManager",
    );
}

fn install_platform_singleton(
    env: &JniEnv,
    initializer_name: &str,
    manager_name: &str,
    setter: &str,
) {
    let Some(initializer) = env.find_class(initializer_name) else {
        eprintln!("apphost: {initializer_name} not found");
        return;
    };
    let Some(manager_class) = env.find_class(manager_name) else {
        eprintln!("apphost: {manager_name} not found");
        return;
    };
    let Some(ctor) = env.method_id(manager_class, "<init>", "()V") else {
        eprintln!("apphost: {manager_name}() missing");
        return;
    };
    let Some(manager) = env.new_object(manager_class, ctor, &[]) else {
        eprintln!("apphost: new {manager_name}() threw");
        return;
    };
    let Some(set) = env.static_method_id(initializer, setter, &format!("(L{manager_name};)V"))
    else {
        eprintln!("apphost: {setter} not found");
        return;
    };
    if env.call_static_void(initializer, set, &[JValue { l: manager }]) {
        println!("apphost: installed {manager_name}");
    } else {
        eprintln!("apphost: {setter} threw");
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
        set_int(
            field,
            if field.contains("Height") {
                SURFACE_H
            } else {
                SURFACE_W
            },
        );
    }
    // `DisplayInfo` has no `densityDpi` field in this build; the density is
    // derived from the logical size and the display metrics.
    // `Display.getRealMetrics()` reads `logicalDensityDpi`, and a zero there
    // makes `DisplayMetrics.density` zero — which turns
    // `ThreadedRenderer.setLightCenter`'s `zRatio` into Infinity and aborts the
    // first traversal with "lightZ must be a finite positive".
    set_int("logicalDensityDpi", DENSITY_DPI);
    // Display.STATE_ON = 2, Display.TYPE_INTERNAL = 1, Surface.ROTATION_0 = 0.
    set_int("state", 2);
    set_int("committedState", 2);
    set_int("type", 1);
    set_int("rotation", 0);
    set_int("modeId", 1);
    // `DisplayInfo.getDefaultMode()` resolves `defaultModeId`, *not* `modeId`.
    // Leaving it at 0 makes `findMode(0)` throw "Unable to locate mode 0" the
    // first time an app asks `Display.getSupportedRefreshRates()` — which is
    // exactly what Roblox's lifecycle observer does on activity post-create.
    set_int("defaultModeId", 1);
    set_int("userPreferredModeId", 1);
    // `DisplayInfo.getDefaultMode()` reads
    // `supportedModes`; an empty array makes `findMode` throw
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
                    JValue { i: SURFACE_W },
                    JValue { i: SURFACE_H },
                    JValue { f: 60.0 },
                    JValue { l: rates },
                ]
            } else {
                &[
                    JValue { i: 1 },
                    JValue { i: SURFACE_W },
                    JValue { i: SURFACE_H },
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
    install_relayout_frames_reply(env);
    install_resized_payload(env);
}

/// Build the `ClientWindowFrames` half of the `relayout` reply.
///
/// `IWindowSession$Stub$Proxy.relayout` takes a `ClientWindowFrames` carrying
/// the four rectangles the window manager computed. The shim cannot build it by
/// hand reliably (the field order is a framework implementation detail), so the
/// host writes it with the framework's own `writeToParcel` and hands the bytes
/// over; the shim appends the `SurfaceControl` half, which carries binder
/// objects and therefore cannot be marshalled.
///
/// The frame is the whole surface: `ViewRootImpl` copies it into `mWinFrame`,
/// and a 0x0 frame makes every view measure to nothing and `performDraw` find
/// an empty dirty region, so nothing is ever rendered.
fn install_relayout_frames_reply(env: &JniEnv) {
    let Some(frames) = build_client_window_frames(env) else {
        return;
    };
    let Some(parcel_class) = env.find_class("android/os/Parcel") else {
        return;
    };
    let Some(obtain) = env.static_method_id(parcel_class, "obtain", "()Landroid/os/Parcel;") else {
        return;
    };
    let Some(parcel) = env.call_static_object(parcel_class, obtain, &[]) else {
        return;
    };
    let Some(write_typed) = env.method_id(
        parcel_class,
        "writeTypedObject",
        "(Landroid/os/Parcelable;I)V",
    ) else {
        eprintln!("apphost: Parcel.writeTypedObject not found");
        return;
    };
    env.call_void(
        parcel,
        write_typed,
        &[JValue { l: frames }, JValue { i: 0 }],
    );
    // A null `MergedConfiguration`: `ViewRootImpl` keeps the one it allocated.
    env.call_void(
        parcel,
        write_typed,
        &[JValue { l: std::ptr::null_mut() }, JValue { i: 0 }],
    );
    let Some(marshall) = env.method_id(parcel_class, "marshall", "()[B") else {
        return;
    };
    let Some(bytes) = env.call_object(parcel, marshall, &[]) else {
        eprintln!("apphost: Parcel.marshall for ClientWindowFrames failed");
        return;
    };
    if let Some(data) = env.byte_array_copy(bytes) {
        let words: Vec<String> = data
            .chunks_exact(4)
            .map(|w| format!("{}", i32::from_ne_bytes([w[0], w[1], w[2], w[3]])))
            .collect();
        println!(
            "apphost: relayout frames parcel ({} bytes) = [{}]",
            data.len(),
            words.join(", ")
        );
        install_reply(&data, "__pengudroid_binder_set_relayout_frames");
    }
}

/// Build the `ClientWindowFrames` the window manager reports for a window.
fn build_client_window_frames(env: &JniEnv) -> Option<JObject> {
    let rect_class = env.find_class("android/graphics/Rect")?;
    let frames_class = env.find_class("android/window/ClientWindowFrames")?;
    let ctor = env.method_id(frames_class, "<init>", "()V")?;
    let frames = env.new_object(frames_class, ctor, &[])?;
    // `Rect(int left, int top, int right, int bottom)`.
    let rect_ctor = env.method_id(rect_class, "<init>", "(IIII)V")?;
    for name in ["frame", "displayFrame", "parentFrame", "attachedFrame"] {
        let f = env.field_id(frames_class, name, "Landroid/graphics/Rect;")?;
        let rect = env.new_object(
            rect_class,
            rect_ctor,
            &[
                JValue { i: 0 },
                JValue { i: 0 },
                JValue { i: SURFACE_W },
                JValue { i: SURFACE_H },
            ],
        )?;
        env.set_object_field(frames, f, rect);
    }
    if let Some(f) = env.field_id(frames_class, "compatScale", "F") {
        env.set_float_field(frames, f, 1.0);
    }
    Some(frames)
}

/// Install the body of `IWindow.resized`, the callback that answers
/// `IWindowSession.relayoutAsync`.
///
/// `ViewRootImpl` uses `relayoutAsync` when only the layout params changed
/// (measured: NewPipe's player switching to immersive mode). It deliberately
/// empties `mWinFrame` and waits for the window manager to send the new frame
/// through this callback. Without it the window stays 0x0 forever, every
/// `SurfaceView` measures to nothing and the decoder has no surface.
///
/// `IWindow.resized` in this framework is
/// `(ClientWindowFrames, boolean reportDraw, MergedConfiguration, InsetsState,
/// boolean forceLayout, boolean alwaysConsumeSystemBars, int displayId,
/// int syncSeqId, int syncInputSeqId)`; the host marshals it because the first
/// and fourth are framework types, and the shim prefixes the interface token.
fn install_resized_payload(env: &JniEnv) {
    let Some(frames) = build_client_window_frames(env) else {
        eprintln!("apphost: IWindow.resized: no ClientWindowFrames");
        return;
    };
    let Some(parcel_class) = env.find_class("android/os/Parcel") else {
        return;
    };
    let Some(obtain) = env.static_method_id(parcel_class, "obtain", "()Landroid/os/Parcel;") else {
        return;
    };
    let Some(parcel) = env.call_static_object(parcel_class, obtain, &[]) else {
        return;
    };
    let (Some(write_typed), Some(write_int)) = (
        env.method_id(
            parcel_class,
            "writeTypedObject",
            "(Landroid/os/Parcelable;I)V",
        ),
        env.method_id(parcel_class, "writeInt", "(I)V"),
    ) else {
        eprintln!("apphost: IWindow.resized: Parcel writers missing");
        return;
    };
    env.call_void(
        parcel,
        write_typed,
        &[JValue { l: frames }, JValue { i: 0 }],
    );
    env.call_void(parcel, write_int, &[JValue { i: 1 }]); // reportDraw
    // A null `MergedConfiguration` means "keep the current one".
    env.call_void(
        parcel,
        write_typed,
        &[JValue { l: std::ptr::null_mut() }, JValue { i: 0 }],
    );
    // An empty `InsetsState`: no system bars, which is what the app already
    // sees, and the client only reads it to update `mAttachInfo`.
    let insets = new_instance(env, "android/view/InsetsState")
        .map(|(_, obj)| obj)
        .filter(|o| !o.is_null());
    env.call_void(
        parcel,
        write_typed,
        &[
            JValue {
                l: insets.unwrap_or(std::ptr::null_mut()),
            },
            JValue { i: 0 },
        ],
    );
    env.call_void(parcel, write_int, &[JValue { i: 1 }]); // forceLayout
    env.call_void(parcel, write_int, &[JValue { i: 0 }]); // alwaysConsumeSystemBars
    env.call_void(parcel, write_int, &[JValue { i: 0 }]); // displayId
    env.call_void(parcel, write_int, &[JValue { i: 0 }]); // syncSeqId
    env.call_void(parcel, write_int, &[JValue { i: 0 }]); // syncInputSeqId
    let Some(marshall) = env.method_id(parcel_class, "marshall", "()[B") else {
        return;
    };
    let Some(bytes) = env.call_object(parcel, marshall, &[]) else {
        eprintln!("apphost: Parcel.marshall for IWindow.resized failed");
        return;
    };
    if let Some(data) = env.byte_array_copy(bytes) {
        let words: Vec<String> = data
            .chunks_exact(4)
            .map(|w| format!("{}", i32::from_ne_bytes([w[0], w[1], w[2], w[3]])))
            .collect();
        println!(
            "apphost: IWindow.resized payload ({} bytes, insets={}) = [{}]",
            data.len(),
            if insets.is_some() { "set" } else { "null" },
            words.join(", ")
        );
        install_reply(&data, "__pengudroid_binder_set_resized_payload");
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
    install_settings_replies(env);
}

/// The `Settings.Secure`/`Global` keys a device has to answer.
///
/// These are the ones apps actually read during startup. `android_id` is the
/// load-bearing one: Roblox reads it in `ActivitySplash.onCreate`
/// (`Settings.Secure.getString(cr, "android_id")`) and keeps it in a static for
/// the rest of the process, so a null there is a device without an identity.
/// The rest are cheap to answer and stop apps falling back to "unknown".
const SETTINGS_VALUES: &[(&str, &str)] = &[
    ("android_id", "a1b2c3d4e5f60718"),
    ("device_name", "PenguDroid"),
    ("bluetooth_name", "PenguDroid"),
    ("adb_enabled", "0"),
    ("device_provisioned", "1"),
    ("user_setup_complete", "1"),
];

/// Install one marshalled `Bundle` per `Settings` key.
///
/// `Settings.NameValueCache.getStringForUser` asks the provider with
/// `call("GET_secure", key, null)` and reads `reply.getString(key)`, so the
/// reply Bundle has to carry that key.
fn install_settings_replies(env: &JniEnv) {
    for (key, value) in SETTINGS_VALUES {
        let Some((bundle_class, bundle)) = new_instance(env, "android/os/Bundle") else {
            return;
        };
        let Some(put) = env.method_id(
            bundle_class,
            "putString",
            "(Ljava/lang/String;Ljava/lang/String;)V",
        ) else {
            return;
        };
        let k = env.new_string(key);
        let v = env.new_string(value);
        env.call_void(bundle, put, &[JValue { l: k }, JValue { l: v }]);
        let Some(data) = marshal_object(env, bundle, "android/os/Bundle") else {
            continue;
        };
        install_setting(key, &data);
    }
}

/// Hand one key's marshalled `Bundle` to the binder broker.
fn install_setting(key: &str, data: &[u8]) {
    // SAFETY: the shim is loaded in this process and copies both buffers.
    unsafe {
        let name = std::ffi::CString::new("__pengudroid_binder_set_setting").ok();
        let Some(name) = name else { return };
        let sym = libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr());
        if sym.is_null() {
            eprintln!("apphost: binder broker does not export __pengudroid_binder_set_setting");
            return;
        }
        type Install = unsafe extern "C" fn(*const u8, usize, *const u8, usize);
        let install: Install = std::mem::transmute(sym);
        install(key.as_ptr(), key.len(), data.as_ptr(), data.len());
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
    // Parameter types, not just names: an AIDL parcel's field order *is* the
    // parameter order, so a reply that is "shifted by one word" is really a
    // question about this list.
    let get_params = env.method_id(method_cls, "getParameterTypes", "()[Ljava/lang/Class;");
    if let Some(methods) = env.call_object(class, get_methods, &[]) {
        for i in 0..env.array_length(methods) {
            let m = env.array_get(methods, i);
            if m.is_null() {
                continue;
            }
            if let Some(n) = env.call_object(m, m_name, &[]).and_then(|s| env.get_string(s)) {
                let mut params = Vec::new();
                if let Some(get_params) = get_params {
                    if let Some(types) = env.call_object(m, get_params, &[]) {
                        for j in 0..env.array_length(types) {
                            let ty = env.array_get(types, j);
                            if ty.is_null() {
                                continue;
                            }
                            params.push(
                                env.call_object(ty, get_name, &[])
                                    .and_then(|s| env.get_string(s))
                                    .unwrap_or_default(),
                            );
                        }
                    }
                }
                println!("  {n}({})", params.join(", "));
            }
        }
    }

    // Constructors are not in `getDeclaredMethods`, and their signatures are
    // what a host-side builder needs (`Surface(long)` is the one that wraps a
    // native `Surface*`, for instance).
    println!("--- constructors of {name}");
    let ctors = env
        .method_id(
            class_cls,
            "getDeclaredConstructors",
            "()[Ljava/lang/reflect/Constructor;",
        )
        .and_then(|m| env.call_object(class, m, &[]));
    let params = env
        .find_class("java/lang/reflect/Constructor")
        .and_then(|c| env.method_id(c, "getParameterTypes", "()[Ljava/lang/Class;"));
    if let (Some(ctors), Some(params)) = (ctors, params) {
        for i in 0..env.array_length(ctors) {
            let c = env.array_get(ctors, i);
            if c.is_null() {
                continue;
            }
            let mut sig = String::from("(");
            if let Some(types) = env.call_object(c, params, &[]) {
                for j in 0..env.array_length(types) {
                    let t = env.array_get(types, j);
                    let n = env
                        .call_object(t, get_name, &[])
                        .and_then(|s| env.get_string(s))
                        .unwrap_or_default();
                    sig.push_str(&n);
                    sig.push(',');
                }
            }
            sig.push(')');
            println!("  <init>{sig}");
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The dp fields must follow the pixel size and the density, not a stale
    /// constant.
    ///
    /// They were once the landscape 1280x720 values (487x274 dp) while the
    /// density was 420, which made a 720 px surface only 274 dp wide — narrower
    /// than a Material `AlertDialog`'s 280 dp minimum, so dialogs overflowed the
    /// window and lost their buttons.
    #[test]
    fn dp_matches_the_surface_and_density() {
        assert_eq!(DENSITY_DPI, 320);
        assert_eq!(SURFACE_W, 720);
        assert_eq!(SURFACE_H, 1280);
        assert_eq!(SURFACE_W_DP, 360);
        assert_eq!(SURFACE_H_DP, 640);

        // `dp = px * 160 / dpi` must round-trip back to the pixel size.
        assert_eq!(SURFACE_W_DP * DENSITY_DPI / 160, SURFACE_W);
        assert_eq!(SURFACE_H_DP * DENSITY_DPI / 160, SURFACE_H);
    }

    /// 360 dp is wide enough for the narrowest thing the framework will draw.
    #[test]
    fn the_screen_is_wide_enough_for_a_material_dialog() {
        // `AlertDialog`'s minimum width is 280 dp.
        assert!(SURFACE_W_DP >= 280, "{SURFACE_W_DP} dp is too narrow");
    }
}
