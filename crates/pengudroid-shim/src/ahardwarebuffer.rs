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
    });
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
        libc::close(b.fd);
        drop(Box::from_raw(buffer));
    }
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
