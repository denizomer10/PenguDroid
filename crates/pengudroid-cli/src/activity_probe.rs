//! Real API-37 generated Binder proxies and Intent/Parcel round trips.
//!
//! No APK, compositor, fake Stub numbers, or Activity callback implementation is
//! needed here. This gate tests admission and transfer, not application rendering.
use super::*;

const PACKAGE: &str = "org.pengudroid.activityprobe";
const ACTIVITY: &str = "org.pengudroid.activityprobe.Next";

pub fn run(env: &JniEnv) -> bool {
    match probe(env) {
        Some(()) => true,
        None => {
            eprintln!("activity: API-37 transition roundtrip failed");
            false
        }
    }
}

fn service(env: &JniEnv, name: &str, stub: &str, interface: &str) -> Option<JObject> {
    let manager = env.find_class("android/os/ServiceManager")?;
    let get = env.static_method_id(manager, "getService", "(Ljava/lang/String;)Landroid/os/IBinder;")?;
    let binder = env.call_static_object(manager, get, &[JValue { l: env.new_string(name) }])?;
    if binder.is_null() { return None; }
    as_interface(env, binder, stub, interface)
}

fn as_interface(env: &JniEnv, binder: JObject, stub: &str, interface: &str) -> Option<JObject> {
    let class = env.find_class(stub)?;
    let convert = env.static_method_id(class, "asInterface", &format!("(Landroid/os/IBinder;)L{interface};"))?;
    env.call_static_object(class, convert, &[JValue { l: binder }]).filter(|obj| !obj.is_null())
}

fn take_request(scratch: &mut [u8]) -> Option<ActivityRequestRecord> {
    let take: unsafe extern "C" fn(*mut u8, usize, *mut usize) -> i32 =
        unsafe { std::mem::transmute(shim_symbol("__pengudroid_binder_take_activity_request")?) };
    let mut len = 0;
    if unsafe { take(scratch.as_mut_ptr(), scratch.len(), &mut len) } != 0 || len > scratch.len() {
        return None;
    }
    parse_activity_request(&scratch[..len])
}

fn probe(env: &JniEnv) -> Option<()> {
    let _frame = env.local_frame(128)?;
    require_android17(env)?;
    install_application_transactions(env)?;
    install_hosted_package(env, PACKAGE)?;
    install_hosted_activities([ACTIVITY])?;
    let task_class = env.find_class("android/app/IActivityTaskManager")?;
    let task = service(env, "activity_task", "android/app/IActivityTaskManager$Stub", "android/app/IActivityTaskManager")?;
    let (_, caller_binder) = new_instance(env, "android/os/Binder")?;
    let caller = as_interface(env, caller_binder, "android/app/IApplicationThread$Stub", "android/app/IApplicationThread")?;
    let start = env.method_id(task_class, "startActivity",
        "(Landroid/app/IApplicationThread;Ljava/lang/String;Ljava/lang/String;Landroid/content/Intent;Ljava/lang/String;Landroid/os/IBinder;Ljava/lang/String;IILandroid/app/ProfilerInfo;Landroid/os/Bundle;)I")?;
    let intent_class = env.find_class("android/content/Intent")?;
    let (_, intent) = new_instance(env, "android/content/Intent")?;
    let set_component = env.method_id(intent_class, "setClassName", "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;")?;
    env.call_object(intent, set_component, &[JValue { l: env.new_string(PACKAGE) }, JValue { l: env.new_string(ACTIVITY) }])?;
    let set_action = env.method_id(intent_class, "setAction", "(Ljava/lang/String;)Landroid/content/Intent;")?;
    env.call_object(intent, set_action, &[JValue { l: env.new_string("org.pengudroid.OPEN") }])?;
    let uri_class = env.find_class("android/net/Uri")?;
    let parse_uri = env.static_method_id(uri_class, "parse", "(Ljava/lang/String;)Landroid/net/Uri;")?;
    let uri = env.call_static_object(uri_class, parse_uri, &[JValue { l: env.new_string("https://example.invalid/video?id=37") }])?;
    let set_data = env.method_id(intent_class, "setData", "(Landroid/net/Uri;)Landroid/content/Intent;")?;
    env.call_object(intent, set_data, &[JValue { l: uri }])?;
    let put = env.method_id(intent_class, "putExtra", "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;")?;
    env.call_object(intent, put, &[JValue { l: env.new_string("marker") }, JValue { l: env.new_string("intent-extra-preserved") }])?;
    let set_flags = env.method_id(intent_class, "setFlags", "(I)Landroid/content/Intent;")?;
    env.call_object(intent, set_flags, &[JValue { i: 0x1000_0000 }])?;

    let call_start = |intent: JObject| env.call_int(task, start, &[
        JValue { l: caller }, JValue { l: env.new_string(PACKAGE) }, JValue { l: std::ptr::null_mut() },
        JValue { l: intent }, JValue { l: std::ptr::null_mut() }, JValue { l: std::ptr::null_mut() },
        JValue { l: std::ptr::null_mut() }, JValue { i: -1 }, JValue { i: 0 },
        JValue { l: std::ptr::null_mut() }, JValue { l: std::ptr::null_mut() },
    ]);
    let mut scratch = vec![0u8; ACTIVITY_REQUEST_SCRATCH];
    if call_start(intent)? != 0 { return None; }
    let request = take_request(&mut scratch)?;
    if request.op != 0 || request.package != PACKAGE || request.class != ACTIVITY { return None; }
    let restored = unmarshall_activity_intent(env, &request.parcel, request.intent_offset)?;
    let get_component = env.method_id(intent_class, "getComponent", "()Landroid/content/ComponentName;")?;
    let component = env.call_object(restored, get_component, &[])?;
    if component.is_null() { return None; }
    let component_class = env.find_class("android/content/ComponentName")?;
    for (method, expected) in [("getPackageName", PACKAGE), ("getClassName", ACTIVITY)] {
        let method = env.method_id(component_class, method, "()Ljava/lang/String;")?;
        let actual = env.call_object(component, method, &[])?;
        if env.get_string(actual)?.as_str() != expected { return None; }
    }
    for (method, expected) in [
        ("getAction", "org.pengudroid.OPEN"), ("getDataString", "https://example.invalid/video?id=37"),
    ] {
        let method = env.method_id(intent_class, method, "()Ljava/lang/String;")?;
        let actual = env.call_object(restored, method, &[])?;
        if env.get_string(actual)?.as_str() != expected { return None; }
    }
    let extra = env.method_id(intent_class, "getStringExtra", "(Ljava/lang/String;)Ljava/lang/String;")?;
    let actual = env.call_object(restored, extra, &[JValue { l: env.new_string("marker") }])?;
    let flags = env.method_id(intent_class, "getFlags", "()I")?;
    if env.get_string(actual)? != "intent-extra-preserved" || env.call_int(restored, flags, &[])? != 0x1000_0000 {
        return None;
    }
    println!("activity: generated API-37 startActivity proxy preserved component, action, URI, flags and extras");

    env.call_object(intent, set_component, &[JValue { l: env.new_string(PACKAGE) }, JValue { l: env.new_string("org.pengudroid.activityprobe.Missing") }])?;
    for candidate in [intent, std::ptr::null_mut()] {
        let result = call_start(candidate)?;
        let manager = env.find_class("android/app/ActivityManager")?;
        let fatal = env.static_method_id(manager, "isStartResultFatalError", "(I)Z")?;
        if env.call_static_boolean(manager, fatal, &[JValue { i: result }])? != true
            || take_request(&mut scratch).is_some() { return None; }
    }
    println!("activity: rejected starts return fatal framework results and enqueue nothing");

    let (_, token_a) = new_instance(env, "android/os/Binder")?;
    let (_, token_b) = new_instance(env, "android/os/Binder")?;
    let a = register_activity_token(env, token_a)?;
    let b = register_activity_token(env, token_b)?;
    let get_controller = env.method_id(task_class, "getActivityClientController", "()Landroid/app/IActivityClientController;")?;
    let controller = env.call_object(task, get_controller, &[])?;
    if controller.is_null() { return None; }
    let controller_class = env.find_class("android/app/IActivityClientController")?;
    let finish = env.method_id(controller_class, "finishActivity", "(Landroid/os/IBinder;ILandroid/content/Intent;I)Z")?;
    for (token, expected) in [(token_a, a.id), (token_b, b.id)] {
        if env.call_boolean(controller, finish, &[
            JValue { l: token }, JValue { i: 0 }, JValue { l: std::ptr::null_mut() }, JValue { i: 0 },
        ])? != true { return None; }
        let request = take_request(&mut scratch)?;
        if request.op != 1 || request.token != expected { return None; }
    }
    let (_, unregistered) = new_instance(env, "android/os/Binder")?;
    if env.call_boolean(controller, finish, &[
        JValue { l: unregistered }, JValue { i: 0 }, JValue { l: std::ptr::null_mut() }, JValue { i: 0 },
    ])? != false || take_request(&mut scratch).is_some() { return None; }
    println!("activity: finishActivity identifies two distinct registered tokens; unknown token rejected");

    let take_wakeup: unsafe extern "C" fn() -> bool = unsafe {
        std::mem::transmute(shim_symbol("__pengudroid_binder_take_activity_wakeup")?)
    };
    // Queue consumption and wakeup are independent. Clear earlier requests and
    // make a fresh start whose wake message must reach the idle main queue.
    unsafe { take_wakeup() };
    let _waker = ActivityHostWaker::start(env)?;
    env.call_object(intent, set_component, &[JValue { l: env.new_string(PACKAGE) }, JValue { l: env.new_string(ACTIVITY) }])?;
    let looper_class = env.find_class("android/os/Looper")?;
    let main_get = env.static_method_id(looper_class, "getMainLooper", "()Landroid/os/Looper;")?;
    let main = env.call_static_object(looper_class, main_get, &[])?;
    let handler_class = env.find_class("android/os/Handler")?;
    let create = env.static_method_id(handler_class, "createAsync", "(Landroid/os/Looper;)Landroid/os/Handler;")?;
    let watchdog = env.call_static_object(handler_class, create, &[JValue { l: main }])?;
    let delayed = env.method_id(handler_class, "sendEmptyMessageDelayed", "(IJ)Z")?;
    let remove = env.method_id(handler_class, "removeMessages", "(I)V")?;
    let once = env.static_method_id(looper_class, "loopOnce", "(Landroid/os/Looper;JI)Z")?;
    let binder_class = env.find_class("android/os/Binder")?;
    let clear = env.static_method_id(binder_class, "clearCallingIdentity", "()J")?;
    let identity = env.call_static_long(binder_class, clear, &[])?;
    if env.call_boolean(watchdog, delayed, &[JValue { i: 7 }, JValue { j: 3000 }])? != true { return None; }
    if call_start(intent)? != 0 { return None; }
    let now = std::time::Instant::now();
    if env.call_static_boolean(looper_class, once, &[JValue { l: main }, JValue { j: identity }, JValue { i: -1 }])? != true {
        return None;
    }
    if !env.call_void(watchdog, remove, &[JValue { i: 7 }]) { return None; }
    if now.elapsed() >= std::time::Duration::from_secs(2) || take_request(&mut scratch).is_none() { return None; }
    println!("activity: asynchronous main Handler wakeup dispatched before the idle-queue watchdog");
    Some(())
}
