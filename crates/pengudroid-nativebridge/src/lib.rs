//! The host side of Android's **native-bridge** contract.
//!
//! A native bridge is a shared library that exports `NativeBridgeItf`, a
//! versioned table of function pointers. Android's runtime (`libnativebridge`)
//! loads that table and uses it to run native code for an instruction set the
//! device cannot execute directly. PenguDroid uses the same contract to run
//! ARM64 Android libraries on x86_64 — the translation itself lives in the
//! bridge (`libberberis_arm64.so`, the Digitalis ARM64 tree), not here.
//!
//! This crate owns exactly one thing: the ABI. It exists so no other part of
//! PenguDroid hard-codes vtable indices, restates call signatures, or guesses
//! how a guest library path must look.
//!
//! # Source of truth
//!
//! Field order, call signatures and the guest path rule come from
//! `frameworks/libs/binary_translation/native_bridge/native_bridge.h` and
//! `native_bridge/arm64/native_bridge.cc`. The official `android17-release`
//! tree (revision `1290e88c60304844bd6e1c0778241ca249d64019`) and the ARM64
//! Digitalis tree agree on all three:
//!
//! * `NativeBridgeCallbacks` has 21 members in the order declared below and
//!   reports version 8.
//! * `kGuestIsa = "arm64"`.
//! * `kSupportedLibraryPathSubstring = "/lib/arm64"` — the bridge checks the
//!   path with `strstr`, not by path component. Both `lib/arm64/foo.so` and an
//!   APK's `lib/arm64-v8a/foo.so` satisfy it, because `arm64-v8a` begins with
//!   `arm64`; `lib/x86_64/foo.so` does not. PenguDroid still stages into
//!   `lib/arm64` as the canonical, unambiguous layout.
//!
//! # What this crate does not do
//!
//! * It does not ship or link a translator. The bridge is opened at runtime and
//!   its presence and version are checked explicitly.
//! * It does not de-Bionic the bridge's `.so`. A bridge built for Android
//!   carries Bionic symbol versions, so the caller prepares the file first
//!   (`pengudroid_elf::strip_android_versions`) and passes the prepared path.
//! * It does not start ART, a zygote, or a guest `app_process`. It is the
//!   low-level half that ART's `libnativebridge` would otherwise drive.
//!
//! # Threading
//!
//! [`Translator`] is deliberately not `Send`: bridges keep per-process guest
//! state and are driven from the thread that initialises them.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::{Path, PathBuf};

/// The interface version this crate is written against.
pub const CALLBACK_VERSION: u32 = 8;
/// Oldest bridge version that still provides every call used here.
///
/// `loadLibrary`/`getTrampoline` exist since version 1 and
/// `isCompatibleWith` since version 2. Anything older is rejected rather than
/// used unverified.
pub const MINIMUM_VERSION: u32 = 2;

/// Path fragment the ARM64 bridge requires (`kSupportedLibraryPathSubstring`).
///
/// The bridge applies `strstr`, so `lib/arm64-v8a` also passes. See
/// [`path_is_supported`].
pub const GUEST_LIBRARY_PATH_FRAGMENT: &str = "/lib/arm64";
/// Instruction-set name the ARM64 bridge expects (`kGuestIsa`).
pub const ARM64_ISA: &str = "arm64";

/// `NativeBridgeRuntimeCallbacks`: what the runtime offers to the bridge.
///
/// The bridge calls these to walk a class's native methods. PenguDroid answers
/// them from JNI, so the bridge never has to guess about the app.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RuntimeCallbacks {
    pub get_method_shorty: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *const c_char,
    pub get_native_method_count: unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32,
    pub get_native_methods: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, u32) -> u32,
}

/// `NativeBridgeCallbacks`, in declaration order.
///
/// Members this crate does not call stay opaque pointers. That keeps the size
/// and alignment exact (a function pointer is a pointer) while making misuse
/// impossible. Null slots are legal and are reported as [`Error::MissingSlot`]
/// only if a call needs one.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Callbacks {
    pub version: u32,
    pub initialize:
        Option<unsafe extern "C" fn(*const RuntimeCallbacks, *const c_char, *const c_char) -> bool>,
    pub load_library: Option<unsafe extern "C" fn(*const c_char, c_int) -> *mut c_void>,
    pub get_trampoline:
        Option<unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, u32) -> *mut c_void>,
    pub is_supported: Option<unsafe extern "C" fn(*const c_char) -> bool>,
    pub get_app_env: *const c_void,
    pub is_compatible_with: Option<unsafe extern "C" fn(u32) -> bool>,
    pub get_signal_handler: *const c_void,
    pub unload_library: Option<unsafe extern "C" fn(*mut c_void) -> c_int>,
    pub get_error: Option<unsafe extern "C" fn() -> *const c_char>,
    pub is_path_supported: Option<unsafe extern "C" fn(*const c_char) -> bool>,
    pub init_anonymous_namespace: *const c_void,
    pub create_namespace: *const c_void,
    pub link_namespaces: *const c_void,
    pub load_library_ext: *const c_void,
    pub get_vendor_namespace: *const c_void,
    pub get_exported_namespace: *const c_void,
    pub pre_zygote_fork: *const c_void,
    pub get_trampoline_with_jni_call_type: *const c_void,
    pub get_trampoline_for_function_pointer: *const c_void,
    pub is_native_bridge_function_pointer: *const c_void,
}

/// Why a bridge operation failed.
///
/// Every variant names the contract that was not met, so a launch fails where
/// it broke instead of misbehaving silently later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// `dlopen` failed.
    Load { path: PathBuf, detail: String },
    /// The library loaded but does not export `NativeBridgeItf`.
    NotABridge(PathBuf),
    /// The reported table version is outside the supported range.
    UnsupportedVersion { found: u32 },
    /// The bridge rejected the runtime's interface version.
    Incompatible { runtime: u32 },
    /// A required slot is null.
    MissingSlot(&'static str),
    /// `initialize` returned false.
    Initialize,
    /// The bridge will not accept the library path.
    UnsupportedPath(PathBuf),
    /// `loadLibrary` returned null.
    LibraryLoad { path: PathBuf, detail: String },
    /// `getTrampoline` returned null.
    NoTrampoline { symbol: String, shorty: String },
    /// A path or argument contained an interior NUL.
    BadString(&'static str),
    /// A staging file operation failed.
    Io { path: PathBuf, detail: String },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Load { path, detail } => {
                write!(f, "cannot open native bridge {}: {detail}", path.display())
            }
            Error::NotABridge(path) => {
                write!(f, "{} does not export NativeBridgeItf", path.display())
            }
            Error::UnsupportedVersion { found } => write!(
                f,
                "native bridge interface version {found} is outside {MINIMUM_VERSION}..={CALLBACK_VERSION}"
            ),
            Error::Incompatible { runtime } => {
                write!(f, "native bridge rejected interface version {runtime}")
            }
            Error::MissingSlot(slot) => write!(f, "native bridge has no {slot} callback"),
            Error::Initialize => write!(f, "native bridge initialize() failed"),
            Error::UnsupportedPath(path) => write!(
                f,
                "native bridge refuses {}: the ARM64 bridge requires a path containing \
                 {GUEST_LIBRARY_PATH_FRAGMENT}",
                path.display()
            ),
            Error::LibraryLoad { path, detail } => {
                write!(f, "native bridge cannot load {}: {detail}", path.display())
            }
            Error::NoTrampoline { symbol, shorty } => {
                write!(f, "native bridge has no trampoline for {symbol} ({shorty})")
            }
            Error::BadString(name) => write!(f, "{name} contains an interior NUL byte"),
            Error::Io { path, detail } => write!(f, "{}: {detail}", path.display()),
        }
    }
}

impl std::error::Error for Error {}

/// A JNI shorty descriptor, validated once.
///
/// The shorty is **not** the JNI descriptor: a native `static int f()` is `I`
/// here, not `()I`. Passing a descriptor makes the bridge build a trampoline
/// with the wrong marshalling, so the value is checked when it is created
/// instead of at call time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shorty(String);

impl Shorty {
    /// Validate a JNI shorty: a return type followed by argument types.
    ///
    /// `V` (void) is only accepted as the entire value, because a void return
    /// cannot have arguments and a void argument does not exist.
    pub fn parse(text: &str) -> Result<Shorty, String> {
        let mut chars = text.chars();
        let Some(ret) = chars.next() else {
            return Err("a shorty needs a return type".into());
        };
        let code = |c: char| matches!(c, 'Z' | 'B' | 'C' | 'S' | 'I' | 'J' | 'F' | 'D' | 'L');
        if !code(ret) && ret != 'V' {
            return Err(format!("`{ret}` is not a JNI return code"));
        }
        let args: Vec<char> = chars.collect();
        if ret == 'V' && !args.is_empty() {
            return Err("a void shorty cannot declare arguments".into());
        }
        for arg in &args {
            if !code(*arg) {
                return Err(format!("`{arg}` is not a JNI argument code"));
            }
        }
        Ok(Shorty(text.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn to_c(&self) -> Result<CString, Error> {
        CString::new(self.0.as_str()).map_err(|_| Error::BadString("shorty"))
    }
}

/// A guest library the bridge loaded.
///
/// The handle is owned by the bridge; [`Translator::unload_library`] releases
/// it. It is not released on drop, because the translator may still be running
/// guest code that uses it.
#[derive(Debug, Clone, Copy)]
pub struct GuestLibrary {
    handle: *mut c_void,
}

impl GuestLibrary {
    /// The bridge's opaque handle. Useful only as a token; do not dereference.
    pub fn handle(&self) -> *mut c_void {
        self.handle
    }
}

/// A callable guest entry point.
#[derive(Debug, Clone)]
pub struct GuestFunction {
    entry: *mut c_void,
    shorty: Shorty,
}

impl GuestFunction {
    pub fn shorty(&self) -> &Shorty {
        &self.shorty
    }

    /// The trampoline address. The caller must match the guest signature.
    pub fn address(&self) -> *mut c_void {
        self.entry
    }

    /// Call a guest `static int f(...)` with a `(`*args*`)I` signature of the
    /// given length, using the JNI calling convention.
    ///
    /// # Safety
    ///
    /// The caller must pass exactly the arguments the shorty describes, and the
    /// trampoline must remain valid (the translator stays loaded).
    pub unsafe fn call_int(&self, env: *mut c_void, args: &[*mut c_void]) -> i32 {
        let f: unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32 = unsafe {
            // SAFETY: the caller upholds the trampoline's signature contract.
            std::mem::transmute(self.entry)
        };
        match args {
            [] => f(env, std::ptr::null_mut()),
            [a] => {
                let f: unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32 =
                    unsafe { std::mem::transmute(self.entry) };
                f(env, *a)
            }
            _ => panic!("call_int supports at most one argument; cast the trampoline directly"),
        }
    }
}

/// A loaded native bridge.
pub struct Translator {
    handle: *mut c_void,
    callbacks: *const Callbacks,
    path: PathBuf,
    version: u32,
    initialized: bool,
}

impl Translator {
    /// Open `path` and validate its table.
    ///
    /// The file must already be prepared for the host: an Android-built bridge
    /// carries Bionic symbol versions, which glibc cannot resolve.
    pub fn open(path: &Path) -> Result<Translator, Error> {
        let cpath = CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|_| Error::BadString("bridge path"))?;
        // RTLD_LOCAL: the bridge's internal symbols must not shadow ours.
        let handle = unsafe { libc::dlopen(cpath.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            return Err(Error::Load {
                path: path.to_path_buf(),
                detail: dlerror(),
            });
        }
        let itf = unsafe { libc::dlsym(handle, c"NativeBridgeItf".as_ptr()) };
        if itf.is_null() {
            unsafe { libc::dlclose(handle) };
            return Err(Error::NotABridge(path.to_path_buf()));
        }
        let callbacks = itf as *const Callbacks;
        // SAFETY: a bridge exports a live table for its whole lifetime.
        let version = unsafe { (*callbacks).version };
        if !(MINIMUM_VERSION..=CALLBACK_VERSION).contains(&version) {
            unsafe { libc::dlclose(handle) };
            return Err(Error::UnsupportedVersion { found: version });
        }
        let translator = Translator {
            handle,
            callbacks,
            path: path.to_path_buf(),
            version,
            initialized: false,
        };
        // libnativebridge asks the bridge whether it accepts its own interface
        // version; a bridge that says no must not be driven at all.
        if let Some(check) = unsafe { (*callbacks).is_compatible_with } {
            if !unsafe { check(CALLBACK_VERSION) } {
                return Err(Error::Incompatible { runtime: CALLBACK_VERSION });
            }
        }
        Ok(translator)
    }

    /// The table version the bridge reports.
    pub fn version(&self) -> u32 {
        self.version
    }

    /// The file this translator was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether [`Translator::initialize`] has run.
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Call `initialize(runtime_cbs, private_dir, instruction_set)`.
    ///
    /// The argument order is the header's, not a guess: the bridge compares
    /// `instruction_set` against `kGuestIsa`. `private_dir` is where the bridge
    /// may keep translation caches.
    pub fn initialize(&mut self, runtime: &RuntimeCallbacks, private_dir: &Path, isa: &str) -> Result<(), Error> {
        let callbacks = unsafe { &*self.callbacks };
        let init = callbacks.initialize.ok_or(Error::MissingSlot("initialize"))?;
        let dir = CString::new(private_dir.as_os_str().as_encoded_bytes())
            .map_err(|_| Error::BadString("private directory"))?;
        let isa = CString::new(isa).map_err(|_| Error::BadString("instruction set"))?;
        if !unsafe { init(runtime, dir.as_ptr(), isa.as_ptr()) } {
            return Err(Error::Initialize);
        }
        self.initialized = true;
        Ok(())
    }

    /// Whether the bridge accepts `path` as a guest library location.
    ///
    /// Uses the bridge's own `isPathSupported` when it has one, and the
    /// documented ARM64 substring otherwise. A path that fails here is rejected
    /// before `loadLibrary`, so the error names the real cause.
    pub fn accepts_library_path(&self, path: &Path) -> bool {
        if let Some(check) = unsafe { (*self.callbacks).is_path_supported } {
            if let Ok(c) = CString::new(path.as_os_str().as_encoded_bytes()) {
                if unsafe { check(c.as_ptr()) } {
                    return true;
                }
            }
        }
        path_is_supported(path)
    }

    /// Load a guest library.
    pub fn load_library(&self, path: &Path) -> Result<GuestLibrary, Error> {
        if !self.initialized {
            return Err(Error::MissingSlot("initialize (call it before loadLibrary)"));
        }
        if !self.accepts_library_path(path) {
            return Err(Error::UnsupportedPath(path.to_path_buf()));
        }
        let callbacks = unsafe { &*self.callbacks };
        let load = callbacks.load_library.ok_or(Error::MissingSlot("loadLibrary"))?;
        let cpath = CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|_| Error::BadString("guest library path"))?;
        let handle = unsafe { load(cpath.as_ptr(), libc::RTLD_NOW) };
        if handle.is_null() {
            return Err(Error::LibraryLoad {
                path: path.to_path_buf(),
                detail: self.last_error(),
            });
        }
        Ok(GuestLibrary { handle })
    }

    /// Resolve a guest entry point.
    ///
    /// `shorty` must be a JNI shorty (`I` for `()I`), not a descriptor.
    pub fn guest_function(
        &self,
        library: &GuestLibrary,
        symbol: &str,
        shorty: &Shorty,
    ) -> Result<GuestFunction, Error> {
        let callbacks = unsafe { &*self.callbacks };
        let trampoline = callbacks.get_trampoline.ok_or(Error::MissingSlot("getTrampoline"))?;
        let name = CString::new(symbol).map_err(|_| Error::BadString("symbol name"))?;
        let shorty_c = shorty.to_c()?;
        let entry = unsafe {
            trampoline(
                library.handle,
                name.as_ptr(),
                shorty_c.as_ptr(),
                shorty.as_str().len() as u32,
            )
        };
        if entry.is_null() {
            return Err(Error::NoTrampoline {
                symbol: symbol.to_string(),
                shorty: shorty.as_str().to_string(),
            });
        }
        Ok(GuestFunction { entry, shorty: shorty.clone() })
    }

    /// Release a guest library through the bridge.
    pub fn unload_library(&self, library: GuestLibrary) -> Result<(), Error> {
        let callbacks = unsafe { &*self.callbacks };
        let Some(unload) = callbacks.unload_library else {
            return Ok(());
        };
        if unsafe { unload(library.handle) } == 0 {
            Ok(())
        } else {
            Err(Error::LibraryLoad {
                path: PathBuf::from("<unload>"),
                detail: self.last_error(),
            })
        }
    }

    /// The bridge's most recent error text, if it publishes one.
    pub fn last_error(&self) -> String {
        let callbacks = unsafe { &*self.callbacks };
        let Some(get) = callbacks.get_error else { return "no detail".into() };
        let text = unsafe { get() };
        if text.is_null() {
            return "no detail".into();
        }
        unsafe { CStr::from_ptr(text) }.to_string_lossy().into_owned()
    }
}

impl Drop for Translator {
    fn drop(&mut self) {
        // Guest libraries are not unloaded here: their code may still be
        // executing. Closing the bridge releases the library on process exit.
        unsafe { libc::dlclose(self.handle) };
    }
}

/// Whether `path` satisfies the ARM64 bridge's path rule.
///
/// This mirrors `kSupportedLibraryPathSubstring`, which the bridge tests with
/// `strstr`. It is therefore a substring check, not a directory-name check:
/// `.../lib/arm64/libx.so` and `.../lib/arm64-v8a/libx.so` both pass, while
/// `.../lib/x86_64/libx.so` does not. Keep the semantics identical to the
/// bridge so a pre-check never accepts something the bridge rejects.
pub fn path_is_supported(path: &Path) -> bool {
    path.to_string_lossy().contains(GUEST_LIBRARY_PATH_FRAGMENT)
}

/// Where guest libraries must live under `root`.
///
/// The directory is `root/lib/arm64`: the canonical layout a device uses. An
/// APK's `lib/arm64-v8a` would also satisfy the bridge's substring rule, but a
/// single canonical directory keeps staging and diagnostics unambiguous.
pub fn guest_library_dir(root: &Path) -> PathBuf {
    root.join("lib").join("arm64")
}

/// Copy one APK library into the layout the bridge accepts.
///
/// Returns the staged path. A copy (rather than a link) keeps the guest's
/// library path independent of where the APK was extracted.
pub fn stage_guest_library(root: &Path, source: &Path) -> Result<PathBuf, Error> {
    let Some(name) = source.file_name() else {
        return Err(Error::Io {
            path: source.to_path_buf(),
            detail: "not a file path".into(),
        });
    };
    let dir = guest_library_dir(root);
    std::fs::create_dir_all(&dir).map_err(|e| Error::Io { path: dir.clone(), detail: e.to_string() })?;
    let dest = dir.join(name);
    std::fs::copy(source, &dest).map_err(|e| Error::Io {
        path: dest.clone(),
        detail: e.to_string(),
    })?;
    if !path_is_supported(&dest) {
        return Err(Error::UnsupportedPath(dest));
    }
    Ok(dest)
}

/// Stage a whole APK's native library set.
///
/// Returns the guest library directory the bridge will accept.
pub fn stage_guest_libraries(root: &Path, sources: &[PathBuf]) -> Result<PathBuf, Error> {
    for source in sources {
        stage_guest_library(root, source)?;
    }
    Ok(guest_library_dir(root))
}

fn dlerror() -> String {
    unsafe {
        let text = libc::dlerror();
        if text.is_null() {
            "unknown dynamic-loader error".into()
        } else {
            CStr::from_ptr(text).to_string_lossy().into_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shorty_accepts_jni_shorties_only() {
        assert!(Shorty::parse("I").is_ok());
        assert!(Shorty::parse("V").is_ok());
        assert!(Shorty::parse("ILjava_lang_String_").is_err());
        assert!(Shorty::parse("LI").is_ok());
        assert!(Shorty::parse("ZZI").is_ok());
        // A JNI descriptor is the mistake this type exists to catch.
        assert!(Shorty::parse("()I").is_err());
        assert!(Shorty::parse("(I)V").is_err());
        assert!(Shorty::parse("VI").is_err());
        assert!(Shorty::parse("").is_err());
        assert!(Shorty::parse("X").is_err());
    }

    #[test]
    fn apk_abi_directory_satisfies_the_substring_rule() {
        // `kSupportedLibraryPathSubstring` is a `strstr` test, so the APK's own
        // directory also passes. Recording it here prevents a future change
        // from "tightening" this into a rule the bridge does not have.
        assert!(path_is_supported(Path::new("/tmp/app/lib/arm64/libSDL3.so")));
        assert!(path_is_supported(Path::new("/tmp/app/lib/arm64-v8a/libSDL3.so")));
        assert!(!path_is_supported(Path::new("/tmp/app/lib/x86_64/libSDL3.so")));
        assert_eq!(
            guest_library_dir(Path::new("/tmp/guest")),
            Path::new("/tmp/guest/lib/arm64")
        );
    }

    #[test]
    fn staging_rewrites_to_the_canonical_layout() {
        let root = std::env::temp_dir().join(format!("pd-nb-stage-{}", std::process::id()));
        let source_dir = root.join("apk/lib/arm64-v8a");
        std::fs::create_dir_all(&source_dir).unwrap();
        let source = source_dir.join("libfixture.so");
        std::fs::write(&source, b"\x7fELF").unwrap();

        let staged = stage_guest_library(&root, &source).unwrap();
        assert!(path_is_supported(&staged), "{}", staged.display());
        assert!(staged.ends_with("lib/arm64/libfixture.so"));
        assert_eq!(std::fs::read(&staged).unwrap(), b"\x7fELF");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn vtable_members_are_in_declaration_order() {
        use std::mem::offset_of;
        // The header's order; a swap here would silently call the wrong slot.
        let order = [
            offset_of!(Callbacks, version),
            offset_of!(Callbacks, initialize),
            offset_of!(Callbacks, load_library),
            offset_of!(Callbacks, get_trampoline),
            offset_of!(Callbacks, is_supported),
            offset_of!(Callbacks, get_app_env),
            offset_of!(Callbacks, is_compatible_with),
            offset_of!(Callbacks, get_signal_handler),
            offset_of!(Callbacks, unload_library),
            offset_of!(Callbacks, get_error),
            offset_of!(Callbacks, is_path_supported),
            offset_of!(Callbacks, init_anonymous_namespace),
            offset_of!(Callbacks, create_namespace),
            offset_of!(Callbacks, link_namespaces),
            offset_of!(Callbacks, load_library_ext),
            offset_of!(Callbacks, get_vendor_namespace),
            offset_of!(Callbacks, get_exported_namespace),
            offset_of!(Callbacks, pre_zygote_fork),
            offset_of!(Callbacks, get_trampoline_with_jni_call_type),
            offset_of!(Callbacks, get_trampoline_for_function_pointer),
            offset_of!(Callbacks, is_native_bridge_function_pointer),
        ];
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?}");
        // `version` is 4 bytes; the first pointer starts at the natural
        // alignment boundary, which is what libnativebridge indexes by.
        assert_eq!(order[0], 0);
        assert_eq!(order[1], std::mem::align_of::<*const c_void>());
        assert_eq!(std::mem::size_of::<Callbacks>(), order[1] + 20 * 8);
    }
}
