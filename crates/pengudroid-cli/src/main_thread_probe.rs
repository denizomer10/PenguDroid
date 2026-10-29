//! Real API-37 main-handler dispatch, including SharedPreferences' background
//! notification path. Called only after `apphost::prepare_main_thread`.

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::jni::{JMethodId, JObject, JValue, JniEnv, JNI_OK};

type ProbeResult<T> = Result<T, &'static str>;

const DISPATCH_TIMEOUT: Duration = Duration::from_secs(5);

pub fn run(env: &JniEnv) -> bool {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| probe(env))) {
        Ok(Ok(())) => true,
        Ok(Err(stage)) => {
            env.describe_exception();
            eprintln!("main-dispatch: {stage}");
            false
        }
        Err(_) => {
            env.describe_exception();
            eprintln!("main-dispatch: probe panicked");
            false
        }
    }
}

fn require(condition: bool, message: &'static str) -> ProbeResult<()> {
    if condition { Ok(()) } else { Err(message) }
}

fn object(value: Option<JObject>, message: &'static str) -> ProbeResult<JObject> {
    value.filter(|value| !value.is_null()).ok_or(message)
}

fn probe(env: &JniEnv) -> ProbeResult<()> {
    let _frame = env.local_frame(64).ok_or("cannot allocate main local frame")?;
    let version = env.find_class("android/os/Build$VERSION").ok_or("Build.VERSION missing")?;
    let sdk = env.static_field_id(version, "SDK_INT", "I").ok_or("SDK_INT missing")?;
    require(env.static_int_field(version, sdk) == 37 && env.exception_check(), "requires API 37")?;

    let looper_class = env.find_class("android/os/Looper").ok_or("Looper missing")?;
    let get_main = env.static_method_id(looper_class, "getMainLooper", "()Landroid/os/Looper;")
        .ok_or("getMainLooper missing")?;
    let my_looper = env.static_method_id(looper_class, "myLooper", "()Landroid/os/Looper;")
        .ok_or("myLooper missing")?;
    let main = object(env.call_static_object(looper_class, get_main, &[]), "main Looper is null")?;
    let current = object(env.call_static_object(looper_class, my_looper, &[]), "current Looper is null")?;
    require(same_object(env, main, current), "current Looper is not the main Looper")?;
    let is_current = env.method_id(looper_class, "isCurrentThread", "()Z")
        .ok_or("Looper.isCurrentThread missing")?;
    require(env.call_boolean(main, is_current, &[]) == Some(true), "probe is not on the main thread")?;

    let thread_class = env.find_class("android/app/ActivityThread").ok_or("ActivityThread missing")?;
    let thread = crate::apphost::current_activity_thread(env).ok_or("currentActivityThread is null")?;
    let get_handler = env.method_id(thread_class, "getHandler", "()Landroid/os/Handler;")
        .ok_or("ActivityThread.getHandler missing")?;
    let expected = object(env.call_object(thread, get_handler, &[]), "ActivityThread.getHandler is null")?;
    let field = env.static_field_id(thread_class, "sMainThreadHandler", "Landroid/os/Handler;")
        .ok_or("sMainThreadHandler missing")?;
    let handler = env.static_object_field(thread_class, field);
    require(env.exception_check() && !handler.is_null(), "sMainThreadHandler is null")?;
    require(same_object(env, handler, expected), "sMainThreadHandler differs from ActivityThread.getHandler")?;
    let handler_class = env.find_class("android/os/Handler").ok_or("Handler missing")?;
    let get_looper = env.method_id(handler_class, "getLooper", "()Landroid/os/Looper;")
        .ok_or("Handler.getLooper missing")?;
    let owner = object(env.call_object(handler, get_looper, &[]), "Handler Looper is null")?;
    require(same_object(env, main, owner), "sMainThreadHandler does not belong to the main Looper")?;
    println!("main-dispatch: sMainThreadHandler identity and main Looper passed");

    let object_class = env.find_class("java/lang/Object").ok_or("Object missing")?;
    let ctor = env.method_id(object_class, "<init>", "()V").ok_or("Object constructor missing")?;
    let marker = object(env.new_object(object_class, ctor, &[]), "cannot create result marker")?;
    let task = future_task(env, marker)?;
    let scratch = ScratchDir::new()?;
    let file = scratch.0.join("preferences.xml").to_str()
        .ok_or("preferences path is not UTF-8")?.to_owned();
    let vm = env.get_java_vm().ok_or("GetJavaVM failed")? as usize;

    // Only global-reference addresses cross threads. Their owners stay on this
    // thread until the scoped worker has joined, including during unwinding.
    let handler_ref = GlobalRef::new(env, handler)?;
    let task_ref = GlobalRef::new(env, task)?;
    let main_ref = GlobalRef::new(env, main)?;
    let refs = WorkerRefs {
        handler: handler_ref.object as usize,
        task: task_ref.object as usize,
        main: main_ref.object as usize,
    };
    let detached = AtomicBool::new(false);
    std::thread::scope(|scope| -> ProbeResult<()> {
        let detached = &detached;
        let worker = std::thread::Builder::new().name("pd-main-dispatch".into())
            .spawn_scoped(scope, move || worker(vm, refs, &file, detached))
            .map_err(|_| "cannot spawn dispatch worker")?;
        worker.join().map_err(|_| "dispatch worker panicked")?
    })?;
    require(detached.load(Ordering::Acquire), "dispatch worker did not detach cleanly")?;

    let future_class = env.find_class("java/util/concurrent/FutureTask").ok_or("FutureTask missing")?;
    let is_done = env.method_id(future_class, "isDone", "()Z").ok_or("FutureTask.isDone missing")?;
    require(env.call_boolean(task, is_done, &[]) == Some(false), "Runnable executed before main dispatch")?;
    dispatch_until_done(env, main, task, is_done)?;
    let get = env.method_id(future_class, "get", "()Ljava/lang/Object;").ok_or("FutureTask.get missing")?;
    let result = object(env.call_object(task, get, &[]), "FutureTask failed or returned null")?;
    require(same_object(env, marker, result), "Runnable returned the wrong result marker")?;
    // notifyListeners posts first, then the marker to the same Handler. Reaching
    // the marker without a dispatch exception also drains that notification.
    println!("main-dispatch: background SharedPreferences.notifyListeners dispatched on main");
    println!("main-dispatch: background Handler.post Runnable executed by API-37 Looper.loopOnce");
    Ok(())
}

#[derive(Clone, Copy)]
struct WorkerRefs {
    handler: usize,
    task: usize,
    main: usize,
}

fn worker(vm: usize, refs: WorkerRefs, file: &str, detached: &AtomicBool) -> ProbeResult<()> {
    let vm = vm as *mut c_void;
    let env = crate::apphost::attach_current_thread(vm).ok_or("cannot attach dispatch worker")?;
    let _attachment = Attachment { vm, env, detached };
    let _frame = env.local_frame(64).ok_or("cannot allocate worker local frame")?;
    let looper_class = env.find_class("android/os/Looper").ok_or("worker Looper missing")?;
    let my_looper = env.static_method_id(looper_class, "myLooper", "()Landroid/os/Looper;")
        .ok_or("worker myLooper missing")?;
    let current = env.call_static_object(looper_class, my_looper, &[]).ok_or("worker myLooper threw")?;
    require(current.is_null(), "attached worker unexpectedly has a Looper")?;
    let is_current = env.method_id(looper_class, "isCurrentThread", "()Z")
        .ok_or("worker isCurrentThread missing")?;
    require(env.call_boolean(refs.main as JObject, is_current, &[]) == Some(false), "worker owns main Looper")?;

    notify_preferences(&env, file)?;
    let handler_class = env.find_class("android/os/Handler").ok_or("worker Handler missing")?;
    let post = env.method_id(handler_class, "post", "(Ljava/lang/Runnable;)Z")
        .ok_or("Handler.post missing")?;
    require(env.call_boolean(refs.handler as JObject, post, &[JValue { l: refs.task as JObject }]) == Some(true),
        "background Handler.post failed")
}

fn notify_preferences(env: &JniEnv, path: &str) -> ProbeResult<()> {
    let file_class = env.find_class("java/io/File").ok_or("File missing")?;
    let file_ctor = env.method_id(file_class, "<init>", "(Ljava/lang/String;)V")
        .ok_or("File constructor missing")?;
    let path = object(Some(env.new_string(path)), "cannot create preferences path")?;
    require(env.exception_check(), "preferences path allocation threw")?;
    let file = object(env.new_object(file_class, file_ctor, &[JValue { l: path }]), "cannot create File")?;
    let preferences_class = env.find_class("android/app/SharedPreferencesImpl").ok_or("SharedPreferencesImpl missing")?;
    let preferences_ctor = env.method_id(preferences_class, "<init>", "(Ljava/io/File;I)V")
        .ok_or("API-37 SharedPreferencesImpl constructor missing")?;
    let preferences = object(env.new_object(preferences_class, preferences_ctor, &[JValue { l: file }, JValue { i: 0 }]),
        "cannot construct SharedPreferencesImpl")?;
    let edit = env.method_id(preferences_class, "edit", "()Landroid/content/SharedPreferences$Editor;")
        .ok_or("SharedPreferencesImpl.edit missing")?;
    let editor = object(env.call_object(preferences, edit, &[]), "cannot create framework EditorImpl")?;

    let collections = env.find_class("java/util/Collections").ok_or("Collections missing")?;
    let empty_list = env.static_method_id(collections, "emptyList", "()Ljava/util/List;")
        .ok_or("Collections.emptyList missing")?;
    let empty_set = env.static_method_id(collections, "emptySet", "()Ljava/util/Set;")
        .ok_or("Collections.emptySet missing")?;
    let empty_map = env.static_method_id(collections, "emptyMap", "()Ljava/util/Map;")
        .ok_or("Collections.emptyMap missing")?;
    let keys = object(env.call_static_object(collections, empty_list, &[]), "empty keys list is null")?;
    let listeners = object(env.call_static_object(collections, empty_set, &[]), "empty listener set is null")?;
    let map = object(env.call_static_object(collections, empty_map, &[]), "empty preferences map is null")?;
    let result_class = env.find_class("android/app/SharedPreferencesImpl$MemoryCommitResult")
        .ok_or("MemoryCommitResult missing")?;
    let result_ctor = env.method_id(result_class, "<init>", "(JZLjava/util/List;Ljava/util/Set;Ljava/util/Map;)V")
        .ok_or("API-37 MemoryCommitResult constructor missing")?;
    let result = object(env.new_object(result_class, result_ctor, &[
        JValue { j: 0 }, JValue { z: 0 }, JValue { l: keys }, JValue { l: listeners }, JValue { l: map },
    ]), "cannot construct MemoryCommitResult")?;
    let editor_class = env.find_class("android/app/SharedPreferencesImpl$EditorImpl").ok_or("EditorImpl missing")?;
    let notify = env.method_id(editor_class, "notifyListeners", "(Landroid/app/SharedPreferencesImpl$MemoryCommitResult;)V")
        .ok_or("API-37 notifyListeners missing")?;
    // API 37 only checks these collections for null before posting. Empty real
    // List/Set instances reach the exact Handler.post path without a listener
    // implementation, a disk commit, or an application/provider dependency.
    require(env.call_void(editor, notify, &[JValue { l: result }]), "background SharedPreferences.notifyListeners threw")
}

fn future_task(env: &JniEnv, result: JObject) -> ProbeResult<JObject> {
    let thread_class = env.find_class("java/lang/Thread").ok_or("Thread missing")?;
    let thread_ctor = env.method_id(thread_class, "<init>", "()V").ok_or("Thread constructor missing")?;
    // An unstarted Thread is a standard-library Runnable whose run() is a no-op.
    let runnable = object(env.new_object(thread_class, thread_ctor, &[]), "cannot create no-op Runnable")?;
    let class = env.find_class("java/util/concurrent/FutureTask").ok_or("FutureTask missing")?;
    let ctor = env.method_id(class, "<init>", "(Ljava/lang/Runnable;Ljava/lang/Object;)V")
        .ok_or("FutureTask Runnable constructor missing")?;
    object(env.new_object(class, ctor, &[JValue { l: runnable }, JValue { l: result }]), "cannot create FutureTask")
}

fn dispatch_until_done(env: &JniEnv, main: JObject, task: JObject, is_done: JMethodId) -> ProbeResult<()> {
    let handler_class = env.find_class("android/os/Handler").ok_or("watchdog Handler missing")?;
    let create_async = env.static_method_id(handler_class, "createAsync", "(Landroid/os/Looper;)Landroid/os/Handler;")
        .ok_or("API-37 Handler.createAsync missing")?;
    let handler = object(env.call_static_object(handler_class, create_async, &[JValue { l: main }]), "cannot create watchdog Handler")?;
    let timeout = future_task(env, std::ptr::null_mut())?;
    let post = env.method_id(handler_class, "postDelayed", "(Ljava/lang/Runnable;J)Z")
        .ok_or("Handler.postDelayed missing")?;
    let remove = env.method_id(handler_class, "removeCallbacks", "(Ljava/lang/Runnable;)V")
        .ok_or("Handler.removeCallbacks missing")?;
    let mut callback = Callback { env: *env, handler, runnable: timeout, remove, active: true };
    // An asynchronous timeout can wake next() even if a synchronization barrier
    // prevents the ordinary main Handler's marker from being dispatched.
    require(env.call_boolean(handler, post, &[
        JValue { l: timeout }, JValue { j: DISPATCH_TIMEOUT.as_millis() as i64 },
    ]) == Some(true), "cannot post dispatch watchdog")?;

    let binder = env.find_class("android/os/Binder").ok_or("Binder missing")?;
    let clear = env.static_method_id(binder, "clearCallingIdentity", "()J").ok_or("clearCallingIdentity missing")?;
    let restore = env.static_method_id(binder, "restoreCallingIdentity", "(J)V").ok_or("restoreCallingIdentity missing")?;
    let token = env.call_static_long(binder, clear, &[]).ok_or("clearCallingIdentity threw")?;
    let _identity = BinderIdentity { env: *env, class: binder, restore, token };
    let ident = env.call_static_long(binder, clear, &[]).ok_or("cannot establish loop identity")?;
    let looper = env.find_class("android/os/Looper").ok_or("dispatch Looper missing")?;
    let once = env.static_method_id(looper, "loopOnce", "(Landroid/os/Looper;JI)Z")
        .ok_or("API-37 Looper.loopOnce missing")?;
    let deadline = Instant::now() + DISPATCH_TIMEOUT;
    while !env.call_boolean(task, is_done, &[]).ok_or("FutureTask.isDone threw")? {
        require(Instant::now() < deadline && env.call_boolean(timeout, is_done, &[]) == Some(false),
            "main Looper did not dispatch the background Runnable before the deadline")?;
        let _iteration = env.local_frame(8).ok_or("cannot allocate dispatch local frame")?;
        require(env.call_static_boolean(looper, once, &[
            JValue { l: main }, JValue { j: ident }, JValue { i: 0 },
        ]) == Some(true), "API-37 Looper.loopOnce threw or the queue quit")?;
    }
    require(callback.cancel(), "cannot remove dispatch watchdog")
}

struct Callback {
    env: JniEnv,
    handler: JObject,
    runnable: JObject,
    remove: JMethodId,
    active: bool,
}

impl Callback {
    fn cancel(&mut self) -> bool {
        if !self.active { return true; }
        self.active = false;
        self.env.call_void(self.handler, self.remove, &[JValue { l: self.runnable }])
    }
}

impl Drop for Callback {
    fn drop(&mut self) {
        if !self.cancel() { eprintln!("main-dispatch: watchdog cleanup threw"); }
    }
}

struct BinderIdentity {
    env: JniEnv,
    class: JObject,
    restore: JMethodId,
    token: i64,
}

impl Drop for BinderIdentity {
    fn drop(&mut self) {
        if !self.env.call_static_void(self.class, self.restore, &[JValue { j: self.token }]) {
            eprintln!("main-dispatch: restoring Binder identity threw");
        }
    }
}

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> ProbeResult<Self> {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|_| "system clock predates Unix epoch")?.as_nanos();
        let path = std::env::temp_dir().join(format!("pd-main-dispatch-{}-{stamp}", std::process::id()));
        std::fs::create_dir(&path).map_err(|_| "cannot create isolated preferences directory")?;
        Ok(Self(path))
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            eprintln!("main-dispatch: temporary preferences cleanup failed: {error}");
        }
    }
}

struct GlobalRef {
    env: JniEnv,
    object: JObject,
}

impl GlobalRef {
    fn new(env: &JniEnv, object: JObject) -> ProbeResult<Self> {
        // SAFETY: JNI slot 21 is NewGlobalRef; the input is a live local reference.
        let new: unsafe extern "C" fn(*mut c_void, JObject) -> JObject = unsafe { jni_function(env, 21) };
        let value = unsafe { new(env.0, object) };
        let global = Self { env: *env, object: value };
        require(env.exception_check() && !value.is_null(), "cannot create worker global reference")?;
        Ok(global)
    }
}

impl Drop for GlobalRef {
    fn drop(&mut self) {
        if !self.object.is_null() {
            // SAFETY: JNI slot 22 is DeleteGlobalRef. Scoped joining guarantees
            // no worker still uses this reference, and this is its owning thread.
            let delete: unsafe extern "C" fn(*mut c_void, JObject) = unsafe { jni_function(&self.env, 22) };
            unsafe { delete(self.env.0, self.object) };
        }
    }
}

struct Attachment<'a> {
    vm: *mut c_void,
    env: JniEnv,
    detached: &'a AtomicBool,
}

impl Drop for Attachment<'_> {
    fn drop(&mut self) {
        let clean = self.env.exception_check();
        // SAFETY: the VM outlives the scoped worker. JavaVM slot 5 is
        // DetachCurrentThread, called after the worker's local frame has popped.
        let rc = unsafe {
            let table = *(self.vm as *const *const *const c_void);
            let detach: unsafe extern "C" fn(*mut c_void) -> i32 = std::mem::transmute(*table.add(5));
            detach(self.vm)
        };
        self.detached.store(rc == JNI_OK && clean, Ordering::Release);
        if rc != JNI_OK { eprintln!("main-dispatch: DetachCurrentThread failed: {rc}"); }
    }
}

fn same_object(env: &JniEnv, left: JObject, right: JObject) -> bool {
    // SAFETY: JNI slot 24 is IsSameObject. JNI handles must not be compared by
    // address: CheckJNI can supply different handles for the same Java object.
    let same: unsafe extern "C" fn(*mut c_void, JObject, JObject) -> u8 = unsafe { jni_function(env, 24) };
    unsafe { same(env.0, left, right) != 0 }
}

unsafe fn jni_function<F: Copy>(env: &JniEnv, slot: usize) -> F {
    // SAFETY: callers select the fixed JNI slot with its exact function type.
    let table = unsafe { *(env.0 as *const *const *const c_void) };
    let pointer = unsafe { *table.add(slot) };
    debug_assert_eq!(std::mem::size_of::<F>(), std::mem::size_of::<*const c_void>());
    unsafe { std::mem::transmute_copy(&pointer) }
}
