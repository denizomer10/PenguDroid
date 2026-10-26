//! A minimal `AHardwareBuffer` implementation.
//!
//! hwui allocates its back buffers through `AHardwareBuffer_allocate`, which
//! normally goes to the gralloc mapper. The mappers are HIDL
//! (`android.hardware.graphics.mapper@2.0/3.0/4.0`) looked up through
//! `hwservicemanager`, which this project does not emulate, so
//! `GraphicBufferMapper::getInstance()` aborts with "gralloc-mapper is missing".
//!
//! hwui only uses five entry points (`allocate`, `describe`, `acquire`,
//! `release`, `to_ANativeWindowBuffer`), so the shim provides them itself and
//! backs each buffer with a `memfd`. That keeps the whole pipeline inside the
//! host process, which is the same idea as the rest of the shim: no HAL, no
//! binder round-trip, just a file descriptor.

use std::ffi::c_void;
use std::sync::atomic::{AtomicI32, Ordering};

// Track mappings by native-handle identity; file descriptors can be reused.
#[derive(Default)]
struct CpuMapping { address: usize, length: usize, locks: usize }
impl Drop for CpuMapping {
    fn drop(&mut self) { unsafe { libc::munmap(self.address as *mut c_void, self.length); } }
}
fn mappings() -> &'static std::sync::Mutex<std::collections::HashMap<usize, CpuMapping>> {
    static MAP: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<usize, CpuMapping>>> = std::sync::OnceLock::new();
    MAP.get_or_init(Default::default)
}
unsafe fn map_cpu(key: usize, fd: i32, length: usize) -> Result<*mut c_void, i32> {
    let mut entries = mappings().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(mapping) = entries.get_mut(&key) {
        mapping.locks += 1;
        return Ok(mapping.address as *mut c_void);
    }
    let address = libc::mmap(std::ptr::null_mut(), length, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0);
    if address == libc::MAP_FAILED { return Err(-12); }
    entries.insert(key, CpuMapping { address: address as usize, length, locks: 1 });
    Ok(address)
}
fn unmap_cpu(key: usize) {
    let mut entries = mappings().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(mapping) = entries.get_mut(&key) {
        mapping.locks -= 1;
        if mapping.locks == 0 { entries.remove(&key); }
    }
}
fn release_cpu(key: usize) {
    mappings().lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
}

/// `AHardwareBuffer_Desc` (android/hardware_buffer.h).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct HardwareBufferDesc {
    pub width: u32,
    pub height: u32,
    pub layers: u32,
    pub format: u32,
    pub usage: u64,
    pub stride: u32,
    pub rfu0: u32,
    pub rfu1: u64,
}

/// `ANativeWindowBuffer` as hwui and EGL see it.
///
/// The first five fields are `ANativeWindowBufferCommon`; `handle` and
/// `fenceFd` follow. Only the fields the render thread reads are meaningful.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NativeWindowBuffer {
    pub width: i32,
    pub height: i32,
    pub stride: i32,
    pub format: i32,
    pub usage: u64,
    pub handle: *const c_void,
    pub fence_fd: i32,
    pub _pad: i32,
}

impl Default for NativeWindowBuffer {
    fn default() -> Self {
        Self {
            width: 0,
            height: 0,
            stride: 0,
            format: 0,
            usage: 0,
            handle: std::ptr::null(),
            fence_fd: -1,
            _pad: 0,
        }
    }
}

const AHB_MAGIC: u64 = 0x4148_4250_454e_4755; // "AHBPENGU"

/// One buffer handed to the guest.
#[repr(C)]
pub struct FakeHardwareBuffer {
    magic: u64,
    desc: HardwareBufferDesc,
    fd: i32,
    refs: AtomicI32,
    window_buffer: NativeWindowBuffer,
    /// The gralloc0-style handle `AHardwareBuffer_getNativeHandle` returns.
    /// `window_buffer.handle` points at it.
    handle: NativeHandle,
}

fn bytes_for(desc: &HardwareBufferDesc) -> usize {
    let stride = desc.stride.max(desc.width);
    let bpp = match desc.format {
        // AHardwareBuffer_Format: RGBA_8888 = 1, RGBX_8888 = 2, RGB_565 = 4,
        // RGBA_F16 = 0x16.
        0x16 => 8,
        4 => 2,
        _ => 4,
    };
    (stride as usize) * (desc.height.max(1) as usize) * (desc.layers.max(1) as usize) * bpp
}

/// `android::GraphicBufferMapper::GraphicBufferMapper()`.
///
/// The real constructor looks up a gralloc mapper (HIDL, via
/// `hwservicemanager`) and calls `LOG_ALWAYS_FATAL("gralloc-mapper is
/// missing")` when none is found, which is what kills the render thread inside
/// `BufferQueueProducer::allocateBuffers`. This only covers constructor calls
/// through the PLT, not API-37 LTO-inlined constructors/backend operations.
/// Only the singleton pointer slot is zeroed `.bss`; the heap object is not.
/// This hook is not a fully initialized mapper and must not be used to seed
/// the singleton for code that accesses its backend directly.
#[no_mangle]
pub unsafe extern "C" fn _ZN7android19GraphicBufferMapperC1Ev(_this: *mut c_void) {
    if std::env::var_os("PENGUDROID_TRACE_AHB").is_some() {
        eprintln!("[ahb] GraphicBufferMapper constructed without a gralloc HAL");
    }
}

/// `android::GraphicBufferMapper::GraphicBufferMapper()` (complete-object alias).
#[no_mangle]
pub unsafe extern "C" fn _ZN7android19GraphicBufferMapperC2Ev(this: *mut c_void) {
    _ZN7android19GraphicBufferMapperC1Ev(this)
}

/// `android::GraphicBufferAllocator::GraphicBufferAllocator()`.
///
/// Same story as the mapper: the real constructor looks up a gralloc allocator
/// and aborts when it is missing.
#[no_mangle]
pub unsafe extern "C" fn _ZN7android22GraphicBufferAllocatorC1Ev(_this: *mut c_void) {
    if std::env::var_os("PENGUDROID_TRACE_AHB").is_some() {
        eprintln!("[ahb] GraphicBufferAllocator constructed without a gralloc HAL");
    }
}

/// `android::GraphicBufferAllocator::GraphicBufferAllocator()` (alias).
#[no_mangle]
pub unsafe extern "C" fn _ZN7android22GraphicBufferAllocatorC2Ev(this: *mut c_void) {
    _ZN7android22GraphicBufferAllocatorC1Ev(this)
}

/// `native_handle_t` (system/core/libcutils/include/cutils/native_handle.h).
#[repr(C)]
pub struct NativeHandle {
    pub version: i32,
    pub num_fds: i32,
    pub num_ints: i32,
    pub data: [i32; 8],
}

/// The `version` every `native_handle_t` must carry: `sizeof(native_handle_t)`
/// in Bionic, i.e. the three header ints only, *not* our whole fixed-size
/// struct (which is 44 bytes because the inline `data` array is sized for the
/// largest handle we ever produce).
///
/// This is load-bearing, not cosmetic: codec2's
/// `_WrapNativeCodec2GrallocHandle` validates the handle with
/// `handle->version == 0xc` before wrapping it in a `C2HandleGralloc`
/// (`libcodec2_vndk.so` `0x50cd2`, returns NULL on mismatch). A handle with the
/// wrong version still maps fine through `libui` (decode succeeds), but the
/// resulting `C2GraphicBlock::handle()` is NULL, so `objcpy` cannot serialise the
/// output buffer into a `WorkBundle` and the decoded frame is dropped as
/// "corrupted work items".
pub const NATIVE_HANDLE_VERSION: i32 = 12;

/// `android::GraphicBufferAllocator::allocate(w, h, format, layerCount, usage,
/// outHandle, outStride, outBufferId, requestorName)`.
///
/// The real implementation delegates to a gralloc allocator. This one backs the
/// buffer with a `memfd` and describes it with the classic gralloc0 layout that
/// `GraphicBuffer` reads back: `data[0]=stride`, `[1]=format`, `[2]=width`,
/// `[3]=height`, `[4..5]=usage`.
#[no_mangle]
pub unsafe extern "C" fn _ZN7android22GraphicBufferAllocator8allocateEjjijmPPK13native_handlePjmNSt3__112basic_stringIcNS6_11char_traitsIcEENS6_9allocatorIcEEEE(
    _this: *mut c_void,
    width: u32,
    height: u32,
    format: i32,
    layer_count: u32,
    usage: u64,
    out_handle: *mut *const NativeHandle,
    out_stride: *mut u32,
    _out_buffer_id: u64,
    _requestor: *mut c_void,
) -> i32 {
    // Thin wrapper: keeping the body in one place avoids the stack juggling that
    // clobbered the saved `out_handle` when this function had its own frame.
    allocate_buffer(
        width,
        height,
        format,
        layer_count,
        usage,
        out_handle,
        out_stride,
    )
}

/// `android::GraphicBufferAllocator::allocateHelper(w, h, format, layerCount,
/// usage, outHandle, outStride, outBufferId, requestorName, flag)`.
///
/// `allocate` and `allocateRawHandle` both reach this through the PLT, so
/// interposing it here is what actually replaces the gralloc allocator. The
/// `std::string` is passed by invisible reference (a pointer) and the trailing
/// `bool` by value.
#[no_mangle]
pub unsafe extern "C" fn _ZN7android22GraphicBufferAllocator14allocateHelperEjjijmPPK13native_handlePjNSt3__112basic_stringIcNS6_11char_traitsIcEENS6_9allocatorIcEEEEb(
    _this: *mut c_void,
    width: u32,
    height: u32,
    format: i32,
    layer_count: u32,
    usage: u64,
    out_handle: *mut *const NativeHandle,
    out_stride: *mut u32,
    _out_buffer_id: u64,
    _requestor: *mut c_void,
    _flag: u8,
) -> i32 {
    allocate_buffer(width, height, format, layer_count, usage, out_handle, out_stride)
}

/// Shared body of the two allocation entry points.
unsafe fn allocate_buffer(
    width: u32,
    height: u32,
    format: i32,
    layer_count: u32,
    usage: u64,
    out_handle: *mut *const NativeHandle,
    out_stride: *mut u32,
) -> i32 {
    if out_handle.is_null() {
        return -22;
    }
    let stride = width.max(1);
    let bpp: usize = match format {
        0x16 => 8, // RGBA_F16
        4 => 2,    // RGB_565
        _ => 4,
    };
    let size = (stride as usize) * (height.max(1) as usize) * (layer_count.max(1) as usize) * bpp;
    let fd = libc::memfd_create(c"pengudroid-gbuf".as_ptr(), libc::MFD_CLOEXEC);
    if fd < 0 {
        return -12;
    }
    if libc::ftruncate(fd, size as libc::off_t) != 0 {
        libc::close(fd);
        return -12;
    }
    let handle = Box::new(NativeHandle {
        version: NATIVE_HANDLE_VERSION,
        num_fds: 1,
        num_ints: 6,
        data: [
            fd,
            stride as i32,
            format,
            width as i32,
            height as i32,
            usage as u32 as i32,
            (usage >> 32) as u32 as i32,
            0,
        ],
    });
    *out_handle = Box::into_raw(handle);
    if !out_stride.is_null() {
        *out_stride = stride;
    }
    if std::env::var_os("PENGUDROID_TRACE_AHB").is_some() {
        eprintln!("[ahb] allocated {width}x{height} fmt={format} stride={stride} ({size} bytes) fd={fd}");
    }
    if std::env::var_os("PENGUDROID_TRACE_AHB_BT").is_some() {
        eprintln!("[ahb] allocate caller:\n{}", std::backtrace::Backtrace::force_capture());
    }
    0
}

/// `android::GraphicBufferMapper::importBuffer(handle, width, height, format,
/// usage, bufferId, generationNumber, out)`.
///
/// The real implementation goes to the gralloc mapper HAL. Our handles are
/// memfd-backed and we are their only producer, so `lock` maps the returned
/// handle directly.
///
/// The returned handle is a **copy**: `GraphicBuffer` frees whichever handle
/// `importBuffer` handed it (`~GraphicBuffer` → `freeBuffer` →
/// `native_handle_delete`), so two buffers wrapping the same block each need
/// their own. Returning the input pointer made the second free a double free —
/// glibc aborted with `double free or corruption (!prev)` from
/// `hidl_handle::~hidl_handle` while codec2 released a decoded frame's output
/// buffer. The descriptors are duplicated with it, so closing one copy cannot
/// pull the fd out from under the other.
#[no_mangle]
pub unsafe extern "C" fn _ZN7android19GraphicBufferMapper12importBufferEPK13native_handlejjjimjPS3_(
    _this: *mut c_void,
    handle: *const NativeHandle,
    _width: u32,
    _height: u32,
    _format: u32,
    _usage: i32,
    _buffer_id: u64,
    _generation: u32,
    out: *mut *const NativeHandle,
) -> i32 {
    if handle.is_null() || out.is_null() {
        return -22;
    }
    let src = &*handle;
    let mut copy = NativeHandle {
        version: src.version,
        num_fds: src.num_fds,
        num_ints: src.num_ints,
        data: src.data,
    };
    let fds = src.num_fds.clamp(0, copy.data.len() as i32);
    for i in 0..fds as usize {
        let dup = libc::dup(src.data[i]);
        if dup < 0 {
            for j in 0..i {
                libc::close(copy.data[j]);
            }
            return -12;
        }
        copy.data[i] = dup;
    }
    *out = Box::into_raw(Box::new(copy));
    0
}

/// `android::GraphicBufferMapper::lock(handle, usage, bounds, addr, bpp, stride)`.
///
/// Returns an `mmap` of the handle's memfd, which is all `C2AllocationGralloc`
/// needs to write decoded pixels.
#[no_mangle]
pub unsafe extern "C" fn _ZN7android19GraphicBufferMapper4lockEPK13native_handlejRKNS_4RectEPPvPiS9_(
    _this: *mut c_void,
    handle: *const NativeHandle,
    _usage: u32,
    _bounds: *const c_void,
    addr: *mut *mut c_void,
    bytes_per_pixel: *mut i32,
    stride: *mut i32,
) -> i32 {
    if handle.is_null() || addr.is_null() {
        return -22;
    }
    let h = &*handle;
    if h.num_fds < 1 || h.data.is_empty() {
        if std::env::var_os("PENGUDROID_TRACE_AHB").is_some() {
            eprintln!("[ahb] lock rejected: numFds={}", h.num_fds);
        }
        return -22;
    }
    let fd = h.data[0];
    let size = libc::lseek(fd, 0, libc::SEEK_END);
    if size <= 0 {
        if std::env::var_os("PENGUDROID_TRACE_AHB").is_some() {
            eprintln!("[ahb] lock rejected: fd={fd} size={size}");
        }
        return -22;
    }
    let p = match map_cpu(handle as usize, fd, size as usize) {
        Ok(p) => p, Err(error) => return error,
    };
    *addr = p;
    if !bytes_per_pixel.is_null() {
        *bytes_per_pixel = 4;
    }
    if !stride.is_null() {
        // `data[1]` is the stride we recorded at allocation time.
        *stride = if h.num_ints >= 1 { h.data[1] } else { 0 };
    }
    if std::env::var_os("PENGUDROID_TRACE_AHB").is_some() {
        eprintln!(
            "[ahb] lock mapped {size} bytes fd={fd} numInts={} stride={}",
            h.num_ints,
            if h.num_ints >= 1 { h.data[1] } else { 0 }
        );
    }
    0
}

/// `android::GraphicBufferMapper::unlock(handle)`.
///
/// The final matching unlock releases the CPU mapping. When
/// `PENGUDROID_DUMP_GBUF` names a directory the block's *current contents* are
/// written out here — the only point where "did anyone actually write pixels
/// into this block?" can be answered. A decoded frame that never reaches the
/// client shows up as one file holding the expected gradient and another still
/// holding zeros.
#[no_mangle]
pub unsafe extern "C" fn _ZN7android19GraphicBufferMapper6unlockEPK13native_handle(
    _this: *mut c_void,
    handle: *const NativeHandle,
) -> i32 {
    if handle.is_null() {
        return -22;
    }
    if let Some(dir) = std::env::var_os("PENGUDROID_DUMP_GBUF") {
        let h = &*handle;
        if h.num_fds >= 1 && !h.data.is_empty() {
            let fd = h.data[0];
            let size = libc::lseek(fd, 0, libc::SEEK_END);
            if size > 0 {
                let p = libc::mmap(
                    std::ptr::null_mut(),
                    size as usize,
                    libc::PROT_READ,
                    libc::MAP_SHARED,
                    fd,
                    0,
                );
                if p != libc::MAP_FAILED {
                    let bytes = std::slice::from_raw_parts(p as *const u8, size as usize);
                    let sum: u64 = bytes.iter().map(|&b| b as u64).sum();
                    let path = std::path::Path::new(&dir).join(format!("gbuf-{fd}.bin"));
                    let _ = std::fs::write(&path, bytes);
                    eprintln!(
                        "[ahb] dumped {} ({} bytes, sum={sum})",
                        path.display(),
                        bytes.len()
                    );
                    libc::munmap(p, size as usize);
                }
            }
        }
    }
    unmap_cpu(handle as usize);
    0
}

/// `android::GraphicBufferMapper::getTransportSize(handle, &numFds, &numInts)`.
///
/// `GraphicBuffer::initWithSize` asks the mapper how many fds and ints a handle
/// carries so it can copy it. Our handles always carry one fd and six ints.
#[no_mangle]
pub unsafe extern "C" fn _ZN7android19GraphicBufferMapper16getTransportSizeEPK13native_handlePjS4_(
    _this: *mut c_void,
    handle: *const NativeHandle,
    num_fds: *mut u32,
    num_ints: *mut u32,
) -> i32 {
    if handle.is_null() {
        return -22;
    }
    if !num_fds.is_null() {
        *num_fds = (*handle).num_fds as u32;
    }
    if !num_ints.is_null() {
        *num_ints = (*handle).num_ints as u32;
    }
    0
}

/// `android::GraphicBufferMapper::freeBuffer(handle)`.
#[no_mangle]
pub unsafe extern "C" fn _ZN7android19GraphicBufferMapper10freeBufferEPK13native_handle(
    _this: *mut c_void,
    handle: *const NativeHandle,
) -> i32 {
    if handle.is_null() {
        return -22;
    }
    release_cpu(handle as usize);
    let h = &*handle;
    if h.num_fds > 0 && !h.data.is_empty() {
        libc::close(h.data[0]);
    }
    drop(Box::from_raw(handle as *mut NativeHandle));
    0
}

/// Resolve a symbol from a guest library, bypassing our own interpositions.
///
/// `dlsym` on a specific handle searches that library first, so forwarding from
/// an interposed symbol reaches the implementation we replaced instead of
/// recursing into ourselves.
unsafe fn guest_sym(soname: &[u8], name: &[u8]) -> *mut c_void {
    let handle = libc::dlopen(soname.as_ptr() as *const libc::c_char, libc::RTLD_NOW | libc::RTLD_GLOBAL);
    if handle.is_null() {
        return std::ptr::null_mut();
    }
    libc::dlsym(handle, name.as_ptr() as *const libc::c_char)
}

/// `android::ConsumerBase::setDefaultBufferFormat(int)`.
///
/// Interposed only to trace: something in the player path sets the surface's
/// buffer format to `HAL_PIXEL_FORMAT_RGB_565` (0x4) while `ImageReader` expects
/// `HAL_PIXEL_FORMAT_YCbCr_420_888` (0x23), and the buffer is then rejected by
/// the consumer.
#[no_mangle]
pub unsafe extern "C" fn _ZN7android12ConsumerBase22setDefaultBufferFormatEi(
    this: *mut c_void,
    format: i32,
) -> i32 {
    if std::env::var_os("PENGUDROID_TRACE_AHB").is_some() {
        eprintln!("[ahb] setDefaultBufferFormat({format:#x}) this={this:p}");
    }
    type F = unsafe extern "C" fn(*mut c_void, i32) -> i32;
    static REAL: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let f = *REAL.get_or_init(|| {
        guest_sym(b"libgui.so\0", b"_ZN7android12ConsumerBase22setDefaultBufferFormatEi\0") as usize
    });
    if f == 0 {
        return 0;
    }
    let real: F = std::mem::transmute(f);
    real(this, format)
}

/// `android::Surface::setBuffersFormat(int)`.
///
/// Traced because `MediaCodec` overrides the surface's request format before
/// queueing: the buffer comes out as `HAL_PIXEL_FORMAT_RGB_565` (0x4) even
/// though the consumer asked for `YCbCr_420_888` (0x23), and the consumer then
/// rejects the buffer.
#[no_mangle]
pub unsafe extern "C" fn _ZN7android7Surface16setBuffersFormatEi(
    this: *mut c_void,
    format: i32,
) -> i32 {
    if std::env::var_os("PENGUDROID_TRACE_AHB").is_some() {
        eprintln!("[ahb] Surface::setBuffersFormat({format:#x}) this={this:p}");
    }
    type F = unsafe extern "C" fn(*mut c_void, i32) -> i32;
    static REAL: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let f = *REAL.get_or_init(|| {
        guest_sym(b"libgui.so\0", b"_ZN7android7Surface16setBuffersFormatEi\0") as usize
    });
    if f == 0 {
        return 0;
    }
    let real: F = std::mem::transmute(f);
    real(this, format)
}

// ---------------------------------------------------------------------------
// GraphicBufferMapper metadata setters and the YCbCr lock
// ---------------------------------------------------------------------------
// `Surface::queueBuffer` calls `Surface::applyGrallocMetadataLocked`, which
// pushes the buffer's colour metadata into the gralloc mapper through these
// setters (`libgui.so` `0xf8040`..`0xf814e`, all PLT calls). If they fall through
// to the real `libui`, the mapper tries to `preloadHal()` and dereferences the
// NULL gralloc HAL — measured as SIGSEGV in
// `GraphicBufferMapper::setDataspace`. Metadata is a gralloc4 concept we do not
// implement (our buffers are plain memfds), so accepting and dropping it is both
// safe and sufficient.
//
// The `dataspace` argument is a `Dataspace` enum (an `int`) and the SMPTE/Cta
// arguments are `std::optional` of small structs; none of them is inspected.

/// `GraphicBufferMapper::setDataspace(handle, Dataspace)`.
#[no_mangle]
pub unsafe extern "C" fn _ZN7android19GraphicBufferMapper12setDataspaceEPK13native_handleNS_8hardware8graphics6common4V1_29DataspaceE(
    _this: *mut c_void,
    _handle: *const NativeHandle,
    _dataspace: i32,
) -> i32 {
    0
}

/// `GraphicBufferMapper::setSmpte2086(handle, optional<Smpte2086>)`.
#[no_mangle]
pub unsafe extern "C" fn _ZN7android19GraphicBufferMapper12setSmpte2086EPK13native_handleNSt3__18optionalIN4aidl7android8hardware8graphics6common9Smpte2086EEE(
    _this: *mut c_void,
    _handle: *const NativeHandle,
    _smpte: *const c_void,
) -> i32 {
    0
}

/// `GraphicBufferMapper::setCta861_3(handle, optional<Cta861_3>)`.
#[no_mangle]
pub unsafe extern "C" fn _ZN7android19GraphicBufferMapper11setCta861_3EPK13native_handleNSt3__18optionalIN4aidl7android8hardware8graphics6common8Cta861_3EEE(
    _this: *mut c_void,
    _handle: *const NativeHandle,
    _cta: *const c_void,
) -> i32 {
    0
}

/// `GraphicBufferMapper::setSmpte2094_10(handle, optional<vector<uint8_t>>)`.
#[no_mangle]
pub unsafe extern "C" fn _ZN7android19GraphicBufferMapper15setSmpte2094_10EPK13native_handleNSt3__18optionalINS4_6vectorIhNS4_9allocatorIhEEEEEE(
    _this: *mut c_void,
    _handle: *const NativeHandle,
    _data: *const c_void,
) -> i32 {
    0
}

/// `GraphicBufferMapper::setSmpte2094_40(handle, optional<vector<uint8_t>>)`.
#[no_mangle]
pub unsafe extern "C" fn _ZN7android19GraphicBufferMapper15setSmpte2094_40EPK13native_handleNSt3__18optionalINS4_6vectorIhNS4_9allocatorIhEEEEEE(
    _this: *mut c_void,
    _handle: *const NativeHandle,
    _data: *const c_void,
) -> i32 {
    0
}

/// `android_ycbcr`: three plane pointers plus their strides.
#[repr(C)]
pub struct AndroidYcbcr {
    pub y: *mut c_void,
    pub cb: *mut c_void,
    pub cr: *mut c_void,
    pub ystride: usize,
    pub cstride: usize,
    pub chroma_step: usize,
}

/// `GraphicsBufferMapper::lockYCbCr(handle, usage, bounds, android_ycbcr*)`.
///
/// The YUV consumer side of a surface (`ImageReader`, `MediaCodec`'s
/// `SoftwareRenderer`) locks planes rather than a single pointer. Our buffers are
/// one memfd, so the planes are just offsets into it; the layout depends on the
/// pixel format the allocator recorded in `data[2]`.
#[no_mangle]
pub unsafe extern "C" fn _ZN7android19GraphicBufferMapper9lockYCbCrEPK13native_handlejRKNS_4RectEP13android_ycbcr(
    _this: *mut c_void,
    handle: *const NativeHandle,
    _usage: u32,
    _bounds: *const c_void,
    out: *mut AndroidYcbcr,
) -> i32 {
    if handle.is_null() || out.is_null() {
        return -22;
    }
    let h = &*handle;
    if h.num_fds < 1 || h.data.is_empty() {
        return -22;
    }
    let fd = h.data[0];
    let size = libc::lseek(fd, 0, libc::SEEK_END);
    if size <= 0 {
        return -22;
    }
    let p = match map_cpu(handle as usize, fd, size as usize) {
        Ok(p) => p, Err(error) => return error,
    };
    let base = p as *mut u8;
    let stride = h.data[1].max(0) as usize;
    let height = h.data[4].max(0) as usize;
    let format = h.data[2] as u32;
    let y = base;
    // Half the chroma planes: `stride/2` per row, `height/2` rows each.
    let cstride = stride / 2;
    let chroma_base = y.add(stride * height);
    // HAL_PIXEL_FORMAT_YCbCr_420_888 (0x23) and _YCrCb_420_SP (0x21) interleave
    // the chroma samples; YV12 (0x32315659) keeps them planar in V,U order.
    let (cb, cr, chroma_step) = match format {
        0x23 => (chroma_base, chroma_base.add(1), 2), // NV12: Cb first
        0x21 => (chroma_base.add(1), chroma_base, 2), // NV21: Cr first
        _ => (chroma_base.add(cstride * (height / 2)), chroma_base, 1), // YV12: V then U
    };
    *out = AndroidYcbcr {
        y: y as *mut c_void,
        cb: cb as *mut c_void,
        cr: cr as *mut c_void,
        ystride: stride,
        cstride,
        chroma_step,
    };
    if std::env::var_os("PENGUDROID_TRACE_AHB").is_some() {
        eprintln!("[ahb] lockYCbCr fmt={format:#x} stride={stride} cstride={cstride} step={chroma_step}");
    }
    0
}

/// `GraphicBufferMapper::lockAsync(handle, usage, bounds, addr, fenceFd, bpp, stride)`.
///
/// Note the `int fenceFd` between `addr` and `bytesPerPixel`: the mangled name
/// is `...PPviPiS9_` (`void**`, `int`, `int*`, `int*`). Omitting it shifts every
/// later argument by one slot, which is not a compile error but writes through
/// the fence descriptor and then dereferences a stride as a pointer (measured as
/// SIGSEGV in our own `lock`).
///
/// Asynchronous locking only matters for fencing; there is no GPU writer to wait
/// for here, so this is `lock`.
#[no_mangle]
pub unsafe extern "C" fn _ZN7android19GraphicBufferMapper9lockAsyncEPK13native_handlejRKNS_4RectEPPviPiS9_(
    this: *mut c_void,
    handle: *const NativeHandle,
    usage: u32,
    bounds: *const c_void,
    addr: *mut *mut c_void,
    _fence_fd: i32,
    bytes_per_pixel: *mut i32,
    stride: *mut i32,
) -> i32 {
    _ZN7android19GraphicBufferMapper4lockEPK13native_handlejRKNS_4RectEPPvPiS9_(
        this,
        handle,
        usage,
        bounds,
        addr,
        bytes_per_pixel,
        stride,
    )
}

/// `GraphicBufferMapper::lockAsync(handle, producerUsage, consumerUsage, bounds,
/// addr, fenceFd, bpp, stride)`.
#[no_mangle]
pub unsafe extern "C" fn _ZN7android19GraphicBufferMapper9lockAsyncEPK13native_handlemmRKNS_4RectEPPviPiS9_(
    this: *mut c_void,
    handle: *const NativeHandle,
    _producer_usage: u64,
    _consumer_usage: u64,
    bounds: *const c_void,
    addr: *mut *mut c_void,
    _fence_fd: i32,
    bytes_per_pixel: *mut i32,
    stride: *mut i32,
) -> i32 {
    _ZN7android19GraphicBufferMapper4lockEPK13native_handlejRKNS_4RectEPPvPiS9_(
        this,
        handle,
        0,
        bounds,
        addr,
        bytes_per_pixel,
        stride,
    )
}

/// `GraphicBufferMapper::unlockAsync(handle, int* fence)`.
#[no_mangle]
pub unsafe extern "C" fn _ZN7android19GraphicBufferMapper11unlockAsyncEPK13native_handlePi(
    _this: *mut c_void,
    handle: *const NativeHandle,
    fence: *mut i32,
) -> i32 {
    if !fence.is_null() {
        *fence = -1;
    }
    _ZN7android19GraphicBufferMapper6unlockEPK13native_handle(_this, handle)
}

/// `GraphicBufferMapper::lockAsyncYCbCr(handle, usage, bounds, ycbcr*, fenceFd)`.
#[no_mangle]
pub unsafe extern "C" fn _ZN7android19GraphicBufferMapper14lockAsyncYCbCrEPK13native_handlejRKNS_4RectEP13android_ycbcri(
    this: *mut c_void,
    handle: *const NativeHandle,
    usage: u32,
    bounds: *const c_void,
    out: *mut AndroidYcbcr,
    _fence_fd: i32,
) -> i32 {
    _ZN7android19GraphicBufferMapper9lockYCbCrEPK13native_handlejRKNS_4RectEP13android_ycbcr(
        this, handle, usage, bounds, out,
    )
}

/// `AHardwareBuffer_allocate(desc, out)`.
#[no_mangle]
pub unsafe extern "C" fn AHardwareBuffer_allocate(
    desc: *const HardwareBufferDesc,
    out: *mut *mut FakeHardwareBuffer,
) -> i32 {
    if desc.is_null() || out.is_null() {
        return -22; // EINVAL
    }
    if std::env::var_os("PENGUDROID_TRACE_AHB").is_some() {
        eprintln!("[ahb] allocate {}x{} fmt={} usage={:#x}", (*desc).width, (*desc).height, (*desc).format, (*desc).usage);
    }
    let mut d = *desc;
    if d.stride == 0 {
        d.stride = d.width;
    }
    let size = bytes_for(&d);
    let name = c"pengudroid-ahb";
    let fd = libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC);
    if fd < 0 {
        return -12; // ENOMEM
    }
    if libc::ftruncate(fd, size as libc::off_t) != 0 {
        libc::close(fd);
        return -12;
    }
    let buf = Box::new(FakeHardwareBuffer {
        magic: AHB_MAGIC,
        desc: d,
        fd,
        refs: AtomicI32::new(1),
        window_buffer: NativeWindowBuffer {
            width: d.width as i32,
            height: d.height as i32,
            stride: d.stride as i32,
            format: d.format as i32,
            usage: d.usage,
            handle: std::ptr::null(),
            fence_fd: -1,
            _pad: 0,
        },
        handle: NativeHandle {
            version: NATIVE_HANDLE_VERSION,
            num_fds: 1,
            num_ints: 6,
            data: [
                fd,
                d.stride as i32,
                d.format as i32,
                d.width as i32,
                d.height as i32,
                d.usage as u32 as i32,
                (d.usage >> 32) as u32 as i32,
                0,
            ],
        },
    });
    let mut buf = buf;
    buf.window_buffer.handle = &buf.handle as *const NativeHandle as *const c_void;
    *out = Box::into_raw(buf);
    0
}

unsafe fn as_buf<'a>(p: *mut FakeHardwareBuffer) -> Option<&'a FakeHardwareBuffer> {
    if p.is_null() {
        return None;
    }
    let b = &*p;
    (b.magic == AHB_MAGIC).then_some(b)
}

/// `AHardwareBuffer_describe(buffer, desc)`.
#[no_mangle]
pub unsafe extern "C" fn AHardwareBuffer_describe(
    buffer: *mut FakeHardwareBuffer,
    desc: *mut HardwareBufferDesc,
) {
    if desc.is_null() {
        return;
    }
    match as_buf(buffer) {
        Some(b) => *desc = b.desc,
        None => *desc = HardwareBufferDesc::default(),
    }
}

/// `AHardwareBuffer_acquire(buffer)`.
#[no_mangle]
pub unsafe extern "C" fn AHardwareBuffer_acquire(buffer: *mut FakeHardwareBuffer) {
    if let Some(b) = as_buf(buffer) {
        b.refs.fetch_add(1, Ordering::Relaxed);
    }
}

/// `AHardwareBuffer_release(buffer)`.
#[no_mangle]
pub unsafe extern "C" fn AHardwareBuffer_release(buffer: *mut FakeHardwareBuffer) {
    if buffer.is_null() {
        return;
    }
    let b = &*buffer;
    if b.magic != AHB_MAGIC {
        return;
    }
    if b.refs.fetch_sub(1, Ordering::AcqRel) == 1 {
        release_cpu(&b.handle as *const NativeHandle as usize);
        libc::close(b.fd);
        drop(Box::from_raw(buffer));
    }
}

/// `AHardwareBuffer_createFromHandle(desc, handle, handleType, out)`.
///
/// The handle's first int is the dma-buf/memfd; it is duplicated so the new
/// buffer owns its own descriptor.
#[no_mangle]
pub unsafe extern "C" fn AHardwareBuffer_createFromHandle(
    desc: *const HardwareBufferDesc,
    handle: *const NativeHandle,
    _handle_type: i32,
    out: *mut *mut FakeHardwareBuffer,
) -> i32 {
    if desc.is_null() || handle.is_null() || out.is_null() {
        return -22;
    }
    if (*handle).num_fds < 1 {
        return -22;
    }
    let fd = libc::dup((*handle).data[0]);
    if fd < 0 {
        return -9;
    }
    let mut d = *desc;
    if d.stride == 0 {
        d.stride = d.width;
    }
    let mut buf = Box::new(FakeHardwareBuffer {
        magic: AHB_MAGIC,
        desc: d,
        fd,
        refs: AtomicI32::new(1),
        window_buffer: NativeWindowBuffer {
            width: d.width as i32,
            height: d.height as i32,
            stride: d.stride as i32,
            format: d.format as i32,
            usage: d.usage,
            handle: std::ptr::null(),
            fence_fd: -1,
            _pad: 0,
        },
        handle: NativeHandle {
            version: NATIVE_HANDLE_VERSION,
            num_fds: 1,
            num_ints: 6,
            data: [
                fd,
                d.stride as i32,
                d.format as i32,
                d.width as i32,
                d.height as i32,
                d.usage as u32 as i32,
                (d.usage >> 32) as u32 as i32,
                0,
            ],
        },
    });
    buf.window_buffer.handle = &buf.handle as *const NativeHandle as *const c_void;
    *out = Box::into_raw(buf);
    0
}

/// `AHardwareBuffer_lockPlanes(buffer, usage, fence, rect, outPlanes)`.
///
/// `AHardwareBuffer_Planes` is `{ uint32_t planeCount; AHardwareBuffer_Plane
/// planes[4]; }` and each plane is `{ void* data; uint32_t stride; uint32_t
/// rowStride; uint32_t pixelStride; }`.
#[no_mangle]
pub unsafe extern "C" fn AHardwareBuffer_lockPlanes(
    buffer: *mut FakeHardwareBuffer,
    usage: u64,
    fence: i32,
    rect: *const c_void,
    out_planes: *mut c_void,
) -> i32 {
    if out_planes.is_null() {
        return -22;
    }
    let mut addr: *mut c_void = std::ptr::null_mut();
    let rc = AHardwareBuffer_lock(buffer, usage, fence, rect, &mut addr);
    if rc != 0 {
        return rc;
    }
    let Some(b) = as_buf(buffer) else {
        return -22;
    };
    // `[u32 planeCount][pad][plane0..3]`, each plane 24 bytes on LP64
    // (`void*` + three `u32` + padding).
    let out = out_planes as *mut u8;
    std::ptr::write_bytes(out, 0, 8 + 4 * 24);
    *(out as *mut u32) = 1;
    let plane = out.add(8);
    *(plane as *mut *mut c_void) = addr;
    *(plane.add(8) as *mut u32) = b.desc.stride * 4;
    *(plane.add(12) as *mut u32) = b.desc.stride * 4;
    *(plane.add(16) as *mut u32) = 4;
    0
}

/// `AHardwareBuffer_isSupported(desc)` — every format we back with a memfd is
/// supported, so this always answers 1.
#[no_mangle]
pub unsafe extern "C" fn AHardwareBuffer_isSupported(_desc: *const HardwareBufferDesc) -> i32 {
    1
}

/// `AHardwareBuffer_getNativeHandle(buffer)`.
///
/// codec2's buffer pool imports buffers through this handle, so it has to be a
/// real gralloc0 `native_handle_t` (one fd, then stride/format/width/height/
/// usage) rather than a null pointer.
#[no_mangle]
pub unsafe extern "C" fn AHardwareBuffer_getNativeHandle(
    buffer: *mut FakeHardwareBuffer,
) -> *const NativeHandle {
    match as_buf(buffer) {
        Some(b) => &b.handle,
        None => std::ptr::null(),
    }
}

/// Offset of `ANativeWindowBuffer::handle` inside an `android::GraphicBuffer`.
///
/// `GraphicBuffer` derives from `ANativeWindowBuffer`, and in this release
/// `toAHardwareBuffer()` is the identity, so a `GraphicBuffer*` *is* an
/// `AHardwareBuffer*` — but not one of ours, so [`as_buf`] rejects it. The
/// field offset was measured on the Android 13 x86_64 sysroot:
/// `GraphicBuffer::unlock` loads the handle from `this+0x70`, and
/// `needsReallocation` reads width/height/format/usage from
/// `0x48`/`0x4c`/`0x54`/`0x78`.
pub const GRAPHIC_BUFFER_HANDLE_OFFSET: usize = 0x70;

/// The `native_handle_t` behind a real `android::GraphicBuffer`.
///
/// This is what the video path needs: the decoder hands us a `GraphicBuffer`
/// from the `BufferQueue` and the daemon imports its dma-buf.
pub unsafe fn graphicbuffer_handle(gb: *const c_void) -> *const NativeHandle {
    if gb.is_null() {
        return std::ptr::null();
    }
    let slot = (gb as *const u8).add(GRAPHIC_BUFFER_HANDLE_OFFSET) as *const *const NativeHandle;
    let handle = *slot;
    if handle.is_null() {
        return std::ptr::null();
    }
    // A handle we did not allocate would be a layout mismatch; refuse rather
    // than hand the daemon a garbage fd.
    if (*handle).num_fds < 1 || (*handle).version != NATIVE_HANDLE_VERSION {
        return std::ptr::null();
    }
    handle
}

/// `AHardwareBuffer_lock(buffer, usage, fence, rect, outVirtualAddress)`.
#[no_mangle]
pub unsafe extern "C" fn AHardwareBuffer_lock(
    buffer: *mut FakeHardwareBuffer,
    _usage: u64,
    _fence: i32,
    _rect: *const c_void,
    out_addr: *mut *mut c_void,
) -> i32 {
    let Some(b) = as_buf(buffer) else {
        return -22;
    };
    if out_addr.is_null() {
        return -22;
    }
    let size = bytes_for(&b.desc);
    let p = match map_cpu(&b.handle as *const NativeHandle as usize, b.fd, size) {
        Ok(p) => p, Err(error) => return error,
    };
    *out_addr = p;
    0
}

/// `AHardwareBuffer_unlock(buffer, fence)`.
#[no_mangle]
pub unsafe extern "C" fn AHardwareBuffer_unlock(
    buffer: *mut FakeHardwareBuffer,
    _fence: *mut i32,
) -> i32 {
    let Some(b) = as_buf(buffer) else {
        return -22;
    };
    unmap_cpu(&b.handle as *const NativeHandle as usize);
    if !_fence.is_null() { *_fence = -1; }
    0
}

/// `AHardwareBuffer_to_ANativeWindowBuffer(buffer)`.
///
/// hwui hands the result to `eglCreateImageKHR`; the gleshim recognises the
/// pointer range and imports the memfd itself.
#[no_mangle]
pub unsafe extern "C" fn AHardwareBuffer_to_ANativeWindowBuffer(
    buffer: *mut FakeHardwareBuffer,
) -> *mut NativeWindowBuffer {
    if buffer.is_null() {
        return std::ptr::null_mut();
    }
    let b = &mut *buffer;
    if b.magic != AHB_MAGIC {
        return std::ptr::null_mut();
    }
    &mut b.window_buffer
}

/// The memfd behind a buffer, for the gleshim's `eglCreateImageKHR`.
#[no_mangle]
pub unsafe extern "C" fn __pengudroid_ahb_fd(buffer: *mut FakeHardwareBuffer) -> i32 {
    as_buf(buffer).map(|b| b.fd).unwrap_or(-1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// codec2 rejects a handle whose `version` is not `sizeof(native_handle_t)`,
    /// and the rejection is silent apart from the dropped frame: the allocation
    /// still maps, so the decoder succeeds and only `C2GraphicBlock::handle()`
    /// comes back NULL. Pin the value so it cannot drift back to
    /// `size_of::<NativeHandle>()` (44).
    #[test]
    fn cpu_lock_cycles_do_not_accumulate_memfd_mappings() {
        unsafe {
            let fd = libc::memfd_create(c"pd-lock-regression".as_ptr(), libc::MFD_CLOEXEC);
            assert!(fd >= 0);
            assert_eq!(libc::ftruncate(fd, 1024 * 1024), 0);
            let key = &fd as *const i32 as usize;
            let count = || std::fs::read_to_string("/proc/self/maps").unwrap().lines()
                .filter(|line| line.contains("pd-lock-regression")).count();
            for _ in 0..2000 {
                let address = map_cpu(key, fd, 1024 * 1024).unwrap();
                std::ptr::write_bytes(address, 0x5a, 1024 * 1024);
                assert_eq!(map_cpu(key, fd, 1024 * 1024).unwrap(), address);
                unmap_cpu(key);
                assert_eq!(count(), 1, "nested lock keeps a single mapping alive");
                unmap_cpu(key);
                assert_eq!(count(), 0, "final unlock must actually munmap");
            }
            map_cpu(key, fd, 1024 * 1024).unwrap();
            release_cpu(key);
            assert_eq!(count(), 0, "destruction releases outstanding locks");
            libc::close(fd);
        }
    }

    #[test]
    fn native_handle_version_is_bionic_sizeof() {
        assert_eq!(NATIVE_HANDLE_VERSION, 12);
        assert_eq!(
            std::mem::offset_of!(NativeHandle, data),
            NATIVE_HANDLE_VERSION as usize
        );
    }

    /// A handle we allocate must describe itself the way `C2AllocatorGralloc`
    /// and `GraphicBufferMapper` read it: one fd first, then the gralloc0
    /// metadata the module documents.
    #[test]
    fn allocated_handle_layout() {
        let mut out: *const NativeHandle = std::ptr::null();
        let mut stride = 0u32;
        let status = unsafe {
            allocate_buffer(320, 240, 0x3231_5659, 1, 0x100, &mut out, &mut stride)
        };
        assert_eq!(status, 0);
        assert!(!out.is_null());
        let h = unsafe { &*out };
        assert_eq!(h.version, NATIVE_HANDLE_VERSION);
        assert_eq!(h.num_fds, 1);
        assert_eq!(h.num_ints, 6);
        assert_eq!(h.data[1], 320, "data[0] is the fd, data[1] the stride");
        assert_eq!(h.data[2], 0x3231_5659);
        assert_eq!(h.data[3], 320);
        assert_eq!(h.data[4], 240);
        unsafe { libc::close(h.data[0]) };
        unsafe { drop(Box::from_raw(out as *mut NativeHandle)) };
    }

    /// `importBuffer` must hand back a handle its caller owns outright.
    ///
    /// `GraphicBuffer` frees whatever it got from `importBuffer`
    /// (`~GraphicBuffer` → `freeBuffer`), so returning the input pointer means
    /// the block's original owner and the importer both free the same handle —
    /// the double free that aborted playback inside
    /// `hidl_handle::~hidl_handle`. The copy also gets its own descriptor, so
    /// the two owners can be closed independently.
    #[test]
    fn import_buffer_copies_the_handle() {
        let mut out: *const NativeHandle = std::ptr::null();
        let mut stride = 0u32;
        let status = unsafe { allocate_buffer(64, 64, 1, 1, 0, &mut out, &mut stride) };
        assert_eq!(status, 0);
        let mut imported: *const NativeHandle = std::ptr::null();
        let status = unsafe {
            _ZN7android19GraphicBufferMapper12importBufferEPK13native_handlejjjimjPS3_(
                std::ptr::null_mut(),
                out,
                64,
                64,
                1,
                0,
                0,
                0,
                &mut imported,
            )
        };
        assert_eq!(status, 0);
        assert!(!imported.is_null());
        assert_ne!(imported, out, "the importer must own its own handle");
        let original = unsafe { &*out };
        let copy = unsafe { &*imported };
        assert_eq!(copy.version, original.version);
        assert_eq!(copy.num_fds, original.num_fds);
        assert_eq!(copy.num_ints, original.num_ints);
        assert_ne!(copy.data[0], original.data[0], "the fd is duplicated");
        assert_eq!(copy.data[1..7], original.data[1..7], "metadata is copied");
        // Both descriptors must be live: closing one leaves the other usable.
        let dup = copy.data[0];
        unsafe { libc::close(dup) };
        let size = unsafe { libc::lseek(original.data[0], 0, libc::SEEK_END) };
        assert!(size > 0, "closing the copy must not close the original");
        unsafe { libc::close(original.data[0]) };
        unsafe {
            drop(Box::from_raw(out as *mut NativeHandle));
            drop(Box::from_raw(imported as *mut NativeHandle));
        }
    }
}
