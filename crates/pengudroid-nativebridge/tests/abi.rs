//! Drives the crate against a C translator whose table is declared from the
//! AOSP header's member order.
//!
//! The stub is not a translator: it records which slot was called and returns
//! known values. If the Rust table's indices or signatures drifted from the
//! header, this test calls the wrong function and fails — which is the point,
//! because the real bridge cannot be loaded on a machine without it.
//!
//! The second test compiles the **real** `native_bridge.h` from the supplied
//! Android 17 / Digitalis sources and compares every field offset, so the Rust
//! declaration is checked against the header, not against a copy.

use std::ffi::{CStr, CString, c_char, c_void};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use pengudroid_nativebridge::{
    ARM64_ISA, Callbacks, RuntimeCallbacks, Shorty, Translator, guest_library_dir, path_is_supported,
    stage_guest_library,
};

/// Minimal stub translator. Declarations mirror the AOSP header's order.
const STUB: &str = r#"
#include <stdint.h>
#include <string.h>
#include <stddef.h>

typedef struct NativeBridgeRuntimeCallbacks NativeBridgeRuntimeCallbacks;
typedef bool (*NativeBridgeSignalHandlerFn)(int, void*, void*);

typedef struct NativeBridgeCallbacks {
  uint32_t version;
  bool (*initialize)(const NativeBridgeRuntimeCallbacks*, const char*, const char*);
  void* (*loadLibrary)(const char*, int);
  void* (*getTrampoline)(void*, const char*, const char*, uint32_t);
  bool (*isSupported)(const char*);
  const void* getAppEnv;
  bool (*isCompatibleWith)(uint32_t);
  NativeBridgeSignalHandlerFn getSignalHandler;
  int (*unloadLibrary)(void*);
  const char* (*getError)(void);
  bool (*isPathSupported)(const char*);
  const void* initAnonymousNamespace;
  const void* createNamespace;
  const void* linkNamespaces;
  const void* loadLibraryExt;
  const void* getVendorNamespace;
  const void* getExportedNamespace;
  const void* preZygoteFork;
  const void* getTrampolineWithJNICallType;
  const void* getTrampolineForFunctionPointer;
  const void* isNativeBridgeFunctionPointer;
} NativeBridgeCallbacks;

static unsigned calls;
static char error[24] = "stub error";

extern "C" int stub_guest(void* env, void* clazz) { (void)env; (void)clazz; return 42; }

static bool stub_initialize(const NativeBridgeRuntimeCallbacks* cbs, const char* dir, const char* isa) {
  (void)cbs; ++calls;
  return dir && isa && strcmp(dir, "init-dir") == 0 && strcmp(isa, "arm64") == 0;
}
static void* stub_load(const char* path, int flag) {
  (void)flag; ++calls;
  if (!path || !strstr(path, "/lib/arm64")) return NULL;
  return (void*)&calls;
}
static void* stub_trampoline(void* handle, const char* name, const char* shorty, uint32_t len) {
  ++calls;
  if (handle != (void*)&calls || !name || !shorty) return NULL;
  if (len != (uint32_t)strlen(shorty)) return NULL;
  if (strcmp(name, "stub_guest") != 0 || strcmp(shorty, "I") != 0) return NULL;
  return (void*)&stub_guest;
}
static bool stub_compatible(uint32_t version) { return version == 8; }
static bool stub_path(const char* path) { return path && strstr(path, "/lib/arm64") != NULL; }
static const char* stub_error(void) { return error; }
static int stub_unload(void* handle) { (void)handle; return 0; }

extern "C" {
NativeBridgeCallbacks NativeBridgeItf = {
  8, &stub_initialize, &stub_load, &stub_trampoline, NULL, NULL,
  &stub_compatible, NULL, &stub_unload, &stub_error, &stub_path,
  NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL
};
}

extern "C" unsigned stub_calls(void) { return calls; }
"#;

struct Fixture {
    path: PathBuf,
    directory: PathBuf,
}

/// Why a fixture could not be built.
///
/// A missing compiler is an environment skip; a compiler that rejects the
/// fixture is a broken test and must fail. Collapsing the two is how a test
/// suite ends up "passing" while running nothing.
enum FixtureError {
    NoCompiler,
    Build(String),
}

impl Fixture {
    fn new(source: &str, name: &str, extra: &[&str]) -> Result<Fixture, FixtureError> {
        let directory =
            std::env::temp_dir().join(format!("pd-nb-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&directory).map_err(|e| FixtureError::Build(e.to_string()))?;
        let path = directory.join(format!("{name}.so"));
        let mut command = Command::new("c++");
        command.args(["-std=c++17", "-Wall", "-Wextra", "-Werror", "-shared", "-fPIC"]);
        command.args(extra);
        command.args(["-x", "c++", "-", "-o"]).arg(&path);
        let mut child = match command.stdin(Stdio::piped()).stderr(Stdio::piped()).spawn() {
            Ok(child) => child,
            Err(_) => return Err(FixtureError::NoCompiler),
        };
        child
            .stdin
            .take()
            .ok_or_else(|| FixtureError::Build("no stdin".into()))?
            .write_all(source.as_bytes())
            .map_err(|e| FixtureError::Build(e.to_string()))?;
        let result = child.wait_with_output().map_err(|e| FixtureError::Build(e.to_string()))?;
        if !result.status.success() {
            let _ = std::fs::remove_dir_all(&directory);
            return Err(FixtureError::Build(
                String::from_utf8_lossy(&result.stderr).into_owned(),
            ));
        }
        Ok(Fixture { path, directory })
    }
}

/// Build a fixture or explain why the test cannot run.
///
/// Returns `None` only when the host has no C++ compiler at all.
fn fixture_or_skip(source: &str, name: &str) -> Option<Fixture> {
    match Fixture::new(source, name, &[]) {
        Ok(fixture) => Some(fixture),
        Err(FixtureError::NoCompiler) => {
            eprintln!("skipping: no host C++ compiler");
            None
        }
        Err(FixtureError::Build(detail)) => {
            panic!("the fixture source no longer compiles:\n{detail}")
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

#[test]
fn stub_translator_drives_every_contract_in_order() {
    let Some(fixture) = fixture_or_skip(STUB, "stub") else { return; };

    let mut translator = Translator::open(&fixture.path).expect("open stub bridge");
    assert_eq!(translator.version(), 8);

    // initialize gets (callbacks, private_dir, isa) in the header's order; the
    // stub accepts only that.
    let runtime = RuntimeCallbacks {
        get_method_shorty: no_shorty,
        get_native_method_count: no_count,
        get_native_methods: no_methods,
    };
    assert!(
        translator
            .initialize(&runtime, Path::new("wrong-dir"), ARM64_ISA)
            .is_err(),
        "the instruction-set argument must be the third one"
    );
    translator
        .initialize(&runtime, Path::new("init-dir"), ARM64_ISA)
        .expect("initialize");

    // A path outside the ARM64 layout is refused before the bridge is asked.
    // `kSupportedLibraryPathSubstring` is a substring test, so `arm64-v8a` is
    // accepted (see the unit test); `x86_64` is not.
    let apk_style = Path::new("/tmp/app/lib/x86_64/libfixture.so");
    assert!(!translator.accepts_library_path(apk_style));
    assert!(translator.load_library(apk_style).is_err());
    assert!(translator.accepts_library_path(Path::new("/tmp/app/lib/arm64-v8a/libfixture.so")));

    let root = std::env::temp_dir().join(format!("pd-nb-root-{}", std::process::id()));
    std::fs::create_dir_all(root.join("apk/lib/arm64-v8a")).unwrap();
    let source = root.join("apk/lib/arm64-v8a/libfixture.so");
    std::fs::write(&source, b"\x7fELF").unwrap();
    let staged = stage_guest_library(&root, &source).expect("stage");
    assert_eq!(staged, guest_library_dir(&root).join("libfixture.so"));
    assert!(path_is_supported(&staged));

    let library = translator.load_library(&staged).expect("load staged library");

    // A JNI descriptor is rejected by the type, before the bridge sees it.
    assert!(Shorty::parse("()I").is_err());
    let shorty = Shorty::parse("I").unwrap();
    assert!(translator.guest_function(&library, "missing_symbol", &shorty).is_err());
    let function = translator
        .guest_function(&library, "stub_guest", &shorty)
        .expect("trampoline");
    let value = unsafe { function.call_int(std::ptr::null_mut(), &[]) };
    assert_eq!(value, 42, "the translated entry point must run");

    assert_eq!(translator.last_error(), "stub error");
    translator.unload_library(library).expect("unload");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn rejects_a_bridge_with_an_unusable_interface_version() {
    let source = STUB.replace(
        "NativeBridgeCallbacks NativeBridgeItf = {\n  8,",
        "NativeBridgeCallbacks NativeBridgeItf = {\n  99,",
    );
    assert_ne!(source, STUB, "the fixture must actually change the version");
    let Some(fixture) = fixture_or_skip(&source, "future") else { return; };
    let error = Translator::open(&fixture.path).err().expect("must reject");
    assert!(error.to_string().contains("99"), "{error}");
}

#[test]
fn rejects_a_library_that_is_not_a_bridge() {
    let Some(fixture) = fixture_or_skip("extern \"C\" int unrelated(void) { return 0; }", "plain")
    else {
        return;
    };
    let error = Translator::open(&fixture.path).err().expect("must reject");
    assert!(error.to_string().contains("NativeBridgeItf"), "{error}");
}

unsafe extern "C" fn no_shorty(_: *mut c_void, _: *mut c_void) -> *const c_char {
    std::ptr::null()
}
unsafe extern "C" fn no_count(_: *mut c_void, _: *mut c_void) -> u32 {
    0
}
unsafe extern "C" fn no_methods(_: *mut c_void, _: *mut c_void, _: *mut c_void, _: u32) -> u32 {
    0
}

/// Compare the Rust table with the real header, when the sources are present.
#[test]
fn rust_table_matches_the_android17_header() {
    let sources = [
        ".toolchain/references/binary_translation-android17/native_bridge/native_bridge.h",
        ".toolchain/platform_frameworks_libs_binary_translation-android16/native_bridge/native_bridge.h",
    ];
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap();
    let Some(header) = sources.iter().map(|p| workspace.join(p)).find(|p| p.exists()) else {
        eprintln!("skipping: no native_bridge.h source tree supplied");
        return;
    };
    // The header includes "jni.h"; any declaration-compatible copy will do.
    // Nothing here starts a JVM. `jni.h` also needs its platform `jni_md.h`.
    let Some(jni_dirs) = find_jni_includes() else {
        eprintln!("skipping: no jni.h on this host to parse native_bridge.h");
        return;
    };

    let probe = format!(
        r#"
#include <cstddef>
#include <cstdio>
#include "{header}"
using android::NativeBridgeCallbacks;
#define P(field) std::printf("%s %zu\n", #field, offsetof(NativeBridgeCallbacks, field))
int main() {{
  std::printf("size %zu\n", sizeof(NativeBridgeCallbacks));
  P(version); P(initialize); P(loadLibrary); P(getTrampoline); P(isSupported);
  P(getAppEnv); P(isCompatibleWith); P(getSignalHandler); P(unloadLibrary);
  P(getError); P(isPathSupported); P(initAnonymousNamespace);
  P(createNamespace); P(linkNamespaces); P(loadLibraryExt);
  P(getVendorNamespace); P(getExportedNamespace); P(preZygoteFork);
  P(getTrampolineWithJNICallType); P(getTrampolineForFunctionPointer);
  P(isNativeBridgeFunctionPointer);
  return 0;
}}
"#,
        header = header.display()
    );

    let directory = std::env::temp_dir().join(format!("pd-nb-header-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let source = directory.join("probe.cc");
    let binary = directory.join("probe");
    std::fs::write(&source, probe).unwrap();
    let compile = Command::new("c++")
        .args(["-std=c++17", "-O0"])
        .args(jni_dirs.iter().flat_map(|dir| ["-I".as_ref(), dir.as_os_str()]))
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .output()
        .expect("host C++ compiler");
    assert!(
        compile.status.success(),
        "native_bridge.h probe failed to compile:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );
    let output = Command::new(&binary).output().expect("run layout probe");
    let text = String::from_utf8_lossy(&output.stdout);
    let mut c_offsets = std::collections::BTreeMap::new();
    for line in text.lines() {
        if let Some((name, value)) = line.split_once(' ') {
            c_offsets.insert(name.to_string(), value.parse::<usize>().unwrap());
        }
    }
    let _ = std::fs::remove_dir_all(&directory);

    use std::mem::{offset_of, size_of};
    let rust = [
        ("version", offset_of!(Callbacks, version)),
        ("initialize", offset_of!(Callbacks, initialize)),
        ("loadLibrary", offset_of!(Callbacks, load_library)),
        ("getTrampoline", offset_of!(Callbacks, get_trampoline)),
        ("isSupported", offset_of!(Callbacks, is_supported)),
        ("getAppEnv", offset_of!(Callbacks, get_app_env)),
        ("isCompatibleWith", offset_of!(Callbacks, is_compatible_with)),
        ("getSignalHandler", offset_of!(Callbacks, get_signal_handler)),
        ("unloadLibrary", offset_of!(Callbacks, unload_library)),
        ("getError", offset_of!(Callbacks, get_error)),
        ("isPathSupported", offset_of!(Callbacks, is_path_supported)),
        ("initAnonymousNamespace", offset_of!(Callbacks, init_anonymous_namespace)),
        ("createNamespace", offset_of!(Callbacks, create_namespace)),
        ("linkNamespaces", offset_of!(Callbacks, link_namespaces)),
        ("loadLibraryExt", offset_of!(Callbacks, load_library_ext)),
        ("getVendorNamespace", offset_of!(Callbacks, get_vendor_namespace)),
        ("getExportedNamespace", offset_of!(Callbacks, get_exported_namespace)),
        ("preZygoteFork", offset_of!(Callbacks, pre_zygote_fork)),
        (
            "getTrampolineWithJNICallType",
            offset_of!(Callbacks, get_trampoline_with_jni_call_type),
        ),
        (
            "getTrampolineForFunctionPointer",
            offset_of!(Callbacks, get_trampoline_for_function_pointer),
        ),
        (
            "isNativeBridgeFunctionPointer",
            offset_of!(Callbacks, is_native_bridge_function_pointer),
        ),
    ];
    for (name, rust_offset) in rust {
        let c = c_offsets
            .get(name)
            .unwrap_or_else(|| panic!("layout probe did not report {name}"));
        assert_eq!(rust_offset, *c, "offset of {name} differs from the header");
    }
    assert_eq!(
        size_of::<Callbacks>(),
        c_offsets["size"],
        "table size differs from the header"
    );
}

/// Locate the include directories for `jni.h` and its platform `jni_md.h`.
///
/// Header parsing only; no JVM is loaded. The host JDK is preferred, and the
/// supplied NDK sysroot is the fallback.
fn find_jni_includes() -> Option<Vec<PathBuf>> {
    let mut roots: Vec<PathBuf> = vec![PathBuf::from("/usr/lib/jvm")];
    if let Some(home) = std::env::var_os("JAVA_HOME") {
        roots.push(PathBuf::from(home));
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    for root in roots {
        if root.join("include/jni.h").exists() {
            candidates.push(root.join("include"));
        }
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                let include = entry.path().join("include");
                if include.join("jni.h").exists() {
                    candidates.push(include);
                }
            }
        }
    }
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap();
    let ndk = workspace.join(".toolchain/ndk-refs_tags_ndk-r30-platform-sysroot/usr/include");
    if ndk.join("jni.h").exists() {
        candidates.push(ndk);
    }

    for include in candidates {
        // `jni_md.h` lives in a platform subdirectory of the JDK layout, and
        // directly beside `jni.h` in the NDK layout.
        if include.join("jni_md.h").exists() {
            return Some(vec![include]);
        }
        for platform in ["linux", "darwin", "win32"] {
            let dir = include.join(platform);
            if dir.join("jni_md.h").exists() {
                return Some(vec![dir, include]);
            }
        }
    }
    None
}

#[test]
fn shorty_text_survives_the_c_conversion() {
    let shorty = Shorty::parse("IL").unwrap();
    assert_eq!(shorty.as_str(), "IL");
    assert_eq!(CStr::from_bytes_with_nul(b"IL\0").unwrap().to_str().unwrap(), "IL");
    let _ = CString::new("unused");
}
