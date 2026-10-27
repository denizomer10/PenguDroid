//! Host-only ABI regression: no Android runtime or target library is loaded.
//! The C fields and accessor declaration are from frameworks/native
//! android17-release ae266dcb706d083868578cfedce381ef44488a07:
//! libs/nativebase/include/nativebase/nativebase.h and libs/ui/include/ui/GraphicBuffer.h.
//! The fake C++ object's layout is deliberately NOT an AOSP GraphicBuffer layout.
#[allow(dead_code)]
#[path = "../src/ahardwarebuffer.rs"]
mod ahardwarebuffer;

use ahardwarebuffer::*;
use std::ffi::{c_void, CStr, CString};
use std::io::Write;
use std::process::{Command, Stdio};

const FIXTURE: &str = r#"
#include <stddef.h>
#include <stdint.h>
#include <string.h>
struct native_handle;
typedef struct native_handle native_handle_t;
struct android_native_base_t {
    int magic;
    int version;
    void* reserved[4];
    void (*incRef)(struct android_native_base_t*);
    void (*decRef)(struct android_native_base_t*);
};
struct ANativeWindowBuffer {
    struct android_native_base_t common;
    int width;
    int height;
    int stride;
    int format;
    int usage_deprecated;
    uintptr_t layerCount;
    void* reserved[1];
    const native_handle_t* handle;
    uint64_t usage;
    void* reserved_proc[8 - (sizeof(uint64_t) / sizeof(void*))];
};
static size_t calls;
namespace android {
class GraphicBuffer {
    uintptr_t opaque[41];
    ANativeWindowBuffer* native;
public:
    explicit GraphicBuffer(ANativeWindowBuffer* b) : native(b) {
        memset(opaque, 0xa5, sizeof(opaque));
    }
    ANativeWindowBuffer* getNativeBuffer() const;
};
ANativeWindowBuffer* GraphicBuffer::getNativeBuffer() const { ++calls; return native; }
}
extern "C" void* fixture_new(ANativeWindowBuffer* b) { return new android::GraphicBuffer(b); }
extern "C" void fixture_delete(void* gb) { delete static_cast<android::GraphicBuffer*>(gb); }
extern "C" size_t fixture_calls() { return calls; }
extern "C" void fixture_retain(ANativeWindowBuffer* b) { b->common.incRef(&b->common); }
extern "C" void fixture_release(ANativeWindowBuffer* b) { b->common.decRef(&b->common); }
extern "C" void fixture_layout(size_t* out) {
    const size_t layout[] = {
        sizeof(android_native_base_t), alignof(android_native_base_t),
        sizeof(ANativeWindowBuffer), alignof(ANativeWindowBuffer),
        offsetof(android_native_base_t, magic), offsetof(android_native_base_t, version),
        offsetof(android_native_base_t, reserved), offsetof(android_native_base_t, incRef),
        offsetof(android_native_base_t, decRef), offsetof(ANativeWindowBuffer, common),
        offsetof(ANativeWindowBuffer, width), offsetof(ANativeWindowBuffer, height),
        offsetof(ANativeWindowBuffer, stride), offsetof(ANativeWindowBuffer, format),
        offsetof(ANativeWindowBuffer, usage_deprecated), offsetof(ANativeWindowBuffer, layerCount),
        offsetof(ANativeWindowBuffer, reserved), offsetof(ANativeWindowBuffer, handle),
        offsetof(ANativeWindowBuffer, usage), offsetof(ANativeWindowBuffer, reserved_proc)
    };
    memcpy(out, layout, sizeof(layout));
}
extern "C" int fixture_check(const ANativeWindowBuffer* b) {
    return b->common.magic == 0x5f626672 && b->common.version == (int)sizeof(*b)
        && b->common.incRef && b->common.decRef && b->handle
        && b->width == 17 && b->height == 9 && b->stride == 32 && b->format == 4
        && b->usage_deprecated == 0x123 && b->usage == 0x100000123ULL && b->layerCount == 2;
}
"#;

struct Fixture {
    library: *mut c_void,
    directory: std::path::PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!("pd-graphicbuffer-native-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("fixture.so");
        let mut compiler = Command::new("c++")
            .args(["-std=c++17", "-Wall", "-Wextra", "-Werror", "-shared", "-fPIC", "-x", "c++", "-", "-o"])
            .arg(&path).stdin(Stdio::piped()).stderr(Stdio::piped()).spawn()
            .expect("a host C++ compiler is required for the native-buffer ABI fixture");
        compiler.stdin.take().unwrap().write_all(FIXTURE.as_bytes()).unwrap();
        let result = compiler.wait_with_output().unwrap();
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        let path = CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        let library = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        assert!(!library.is_null(), "loading the host-only fixture failed");
        Self { library, directory }
    }

    unsafe fn symbol(&self, name: &CStr) -> *mut c_void {
        let symbol = libc::dlsym(self.library, name.as_ptr());
        assert!(!symbol.is_null(), "fixture symbol missing: {name:?}");
        symbol
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        unsafe { libc::dlclose(self.library); }
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

#[test]
fn android17_native_accessor_c_layout_provenance_and_lifetime() {
    use std::mem::{align_of, offset_of, size_of, transmute};
    use std::ptr::{null, null_mut};

    let fixture = Fixture::new();
    unsafe {
        let accessor: GetNativeBuffer = transmute(fixture.symbol(
            CStr::from_bytes_with_nul(GRAPHIC_BUFFER_GET_NATIVE_BUFFER).unwrap()));
        let make: unsafe extern "C" fn(*mut NativeWindowBuffer) -> *mut c_void =
            transmute(fixture.symbol(c"fixture_new"));
        let delete: unsafe extern "C" fn(*mut c_void) = transmute(fixture.symbol(c"fixture_delete"));
        let calls: unsafe extern "C" fn() -> usize = transmute(fixture.symbol(c"fixture_calls"));
        let layout: unsafe extern "C" fn(*mut usize) = transmute(fixture.symbol(c"fixture_layout"));
        let check: unsafe extern "C" fn(*const NativeWindowBuffer) -> i32 =
            transmute(fixture.symbol(c"fixture_check"));
        let retain: unsafe extern "C" fn(*mut NativeWindowBuffer) = transmute(fixture.symbol(c"fixture_retain"));
        let release: unsafe extern "C" fn(*mut NativeWindowBuffer) = transmute(fixture.symbol(c"fixture_release"));

        let rust_layout = [
            size_of::<NativeBase>(), align_of::<NativeBase>(),
            size_of::<NativeWindowBuffer>(), align_of::<NativeWindowBuffer>(),
            offset_of!(NativeBase, magic), offset_of!(NativeBase, version),
            offset_of!(NativeBase, reserved), offset_of!(NativeBase, inc_ref),
            offset_of!(NativeBase, dec_ref), offset_of!(NativeWindowBuffer, common),
            offset_of!(NativeWindowBuffer, width), offset_of!(NativeWindowBuffer, height),
            offset_of!(NativeWindowBuffer, stride), offset_of!(NativeWindowBuffer, format),
            offset_of!(NativeWindowBuffer, usage_deprecated), offset_of!(NativeWindowBuffer, layer_count),
            offset_of!(NativeWindowBuffer, reserved), offset_of!(NativeWindowBuffer, handle),
            offset_of!(NativeWindowBuffer, usage), offset_of!(NativeWindowBuffer, reserved_proc),
        ];
        let mut c_layout = [0; 20];
        layout(c_layout.as_mut_ptr());
        assert_eq!(rust_layout, c_layout, "all native C fields, sizes and alignments must agree");

        let desc = HardwareBufferDesc {
            width: 17, height: 9, stride: 32, format: 4, layers: 2,
            usage: 0x1_0000_0123, ..HardwareBufferDesc::default()
        };
        let mut buffer = null_mut();
        assert_eq!(AHardwareBuffer_allocate(&desc, &mut buffer), 0);
        let actual_native = AHardwareBuffer_to_ANativeWindowBuffer(buffer);
        assert_eq!(check(actual_native), 1, "C must see the allocated buffer's header and metadata");
        let handle = AHardwareBuffer_getNativeHandle(buffer);
        let fd = (*handle).data[0];
        let mut stat: libc::stat = std::mem::zeroed();
        assert_eq!(libc::fstat(fd, &mut stat), 0);
        let inode = stat.st_ino.to_string();
        let mapping_count = || std::fs::read_to_string("/proc/self/maps").unwrap().lines()
            .filter(|line| line.contains("memfd:pengudroid-ahb")
                && line.split_whitespace().nth(4) == Some(inode.as_str())).count();
        // The accessor returns this separate object, not any prefix/base of gb.
        let mut native = *actual_native;
        let gb = make(&mut native);
        assert!(!gb.is_null());
        assert_eq!(graphicbuffer_handle(gb, accessor), handle);
        let before = calls();
        assert!(graphicbuffer_handle(null(), accessor).is_null());
        assert_eq!(calls(), before, "a null gb must not invoke the C++ accessor");
        let empty = make(null_mut());
        assert!(graphicbuffer_handle(empty, accessor).is_null());
        delete(empty);

        native.common.magic = 0;
        assert!(graphicbuffer_handle(gb, accessor).is_null());
        native.common.magic = NATIVE_BUFFER_MAGIC;
        native.common.version -= 1;
        assert!(graphicbuffer_handle(gb, accessor).is_null());
        native.common.version += 1;
        native.handle = null();
        assert!(graphicbuffer_handle(gb, accessor).is_null());
        native.handle = 1usize as *const NativeHandle;
        assert!(graphicbuffer_handle(gb, accessor).is_null(), "unknown pointers must not be dereferenced");
        let lookalike = NativeHandle {
            version: NATIVE_HANDLE_VERSION, num_fds: 1, num_ints: 6, data: (*handle).data,
        };
        native.handle = &lookalike;
        assert!(graphicbuffer_handle(gb, accessor).is_null(), "a matching header is not shim provenance");
        native.handle = handle;
        for (version, fds, ints) in [(0, 1, 6), (12, 2, 6), (12, 1, 0)] {
            let h = handle as *mut NativeHandle;
            (*h).version = version;
            (*h).num_fds = fds;
            (*h).num_ints = ints;
            assert!(graphicbuffer_handle(gb, accessor).is_null());
        }
        (*(handle as *mut NativeHandle)).version = NATIVE_HANDLE_VERSION;
        (*(handle as *mut NativeHandle)).num_fds = 1;
        (*(handle as *mut NativeHandle)).num_ints = 6;

        let mut imported = null();
        assert_eq!(_ZN7android19GraphicBufferMapper12importBufferEPK13native_handlejjjimjPS3_(
            null_mut(), handle, 17, 9, 2, 4, desc.usage, 32, &mut imported), 0);
        assert_ne!((*imported).data[0], fd, "import still owns a duplicated descriptor");
        native.handle = imported;
        assert_eq!(graphicbuffer_handle(gb, accessor), imported);
        let mut clone = null_mut();
        assert_eq!(AHardwareBuffer_createFromHandle(&desc, handle, 2, &mut clone), 0);
        assert_eq!(check(AHardwareBuffer_to_ANativeWindowBuffer(clone)), 1);
        native.handle = AHardwareBuffer_getNativeHandle(clone);
        assert_eq!(graphicbuffer_handle(gb, accessor), native.handle);
        AHardwareBuffer_release(clone);
        assert!(graphicbuffer_handle(gb, accessor).is_null(), "released AHB imports are unregistered");

        native.handle = handle;
        let mut address = null_mut();
        assert_eq!(AHardwareBuffer_lock(buffer, 0, -1, null(), &mut address), 0);
        *(address as *mut u8) = 0x5a;
        for _ in 0..50 {
            assert_eq!(graphicbuffer_handle(gb, accessor), native.handle);
        }
        assert!(libc::fcntl(fd, libc::F_GETFD) >= 0, "accessors must only borrow the fd");
        retain(actual_native);
        AHardwareBuffer_release(buffer);
        assert_eq!(graphicbuffer_handle(gb, accessor), handle, "native incRef must retain the AHB owner");
        assert_eq!(*(address as *const u8), 0x5a, "retaining native storage also keeps CPU mapping alive");
        assert_eq!(mapping_count(), 1);
        release(actual_native);
        assert!(graphicbuffer_handle(gb, accessor).is_null(), "final decRef removes provenance before freeing");
        assert_eq!(mapping_count(), 0, "final release unmaps outstanding CPU locks");

        native.handle = imported;
        assert_eq!(graphicbuffer_handle(gb, accessor), native.handle);
        let mut byte = 0u8;
        assert_eq!(libc::pread((*imported).data[0], &mut byte as *mut u8 as *mut c_void, 1, 0), 1);
        assert_eq!(byte, 0x5a, "freeing the original must not close the import's memfd");
        assert_eq!(_ZN7android19GraphicBufferMapper10freeBufferEPK13native_handle(null_mut(), imported), 0);
        assert!(graphicbuffer_handle(gb, accessor).is_null(), "freed mapper imports are unregistered");
        delete(gb);
    }
}
