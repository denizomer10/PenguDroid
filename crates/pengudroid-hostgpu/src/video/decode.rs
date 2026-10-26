//! Host-only compressed video decoding through an optional native plugin.
//! FFmpeg structures stay behind a versioned C ABI. Never load this plugin in
//! the Bionic guest: the broker/host owns the native driver and frame lifetimes.
use super::Library;
use std::ffi::{c_char, c_int, c_void, CString};
use std::marker::PhantomData;
use std::os::fd::{BorrowedFd, RawFd};
use std::path::Path;
use std::ptr::NonNull;
use std::rc::Rc;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Object { fd: i32, size: u32, modifier: u64 }
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Plane { pub object_index: u32, pub offset: u32, pub stride: u32 }
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RawInfo {
    width: u32, height: u32, fourcc: u32, object_count: u32, plane_count: u32,
    pts_us: i64, objects: [Object; 4], planes: [Plane; 4],
}
type Open = unsafe extern "C" fn(*const c_char, *const c_char, *const u8, u32, *mut c_char, u32) -> *mut c_void;
type Close = unsafe extern "C" fn(*mut c_void);
type Send = unsafe extern "C" fn(*mut c_void, *const u8, u32, i64, *mut c_char, u32) -> c_int;
type Receive = unsafe extern "C" fn(*mut c_void, *mut *mut c_void, *mut RawInfo, *mut c_char, u32) -> c_int;
type Release = unsafe extern "C" fn(*mut c_void);
type Flush = unsafe extern "C" fn(*mut c_void);
type Luma = unsafe extern "C" fn(*mut c_void, *mut u32, *mut c_char, u32) -> c_int;
struct Api { _library: Library, close: Close, send: Send, receive: Receive, release: Release, flush: Flush, luma: Luma }

fn error_text(error: &[u8], status: i32) -> String {
    let end = error.iter().position(|b| *b == 0).unwrap_or(error.len());
    if end == 0 { format!("hardware video backend failed ({status})") }
    else { String::from_utf8_lossy(&error[..end]).into_owned() }
}

/// A decoder uses VA-API on the chosen DRM render node. Unsupported devices or
/// codecs return errors; software decoding is never silently substituted.
pub struct Decoder { api: Rc<Api>, pointer: NonNull<c_void>, _thread: PhantomData<Rc<()>> }
#[derive(Debug, PartialEq, Eq)]
pub enum SendStatus { Accepted, DrainFirst }
pub enum ReceiveStatus { Frame(HardwareFrame), NeedsInput, EndOfStream, FramesHeld }

/// A GPU frame export. Descriptor fds are duplicated, while the original
/// HardwareFrame must remain held until compositor release.
pub struct ExportedFrame {
    pub width: u32, pub height: u32, pub fourcc: u32,
    pub objects: Vec<std::os::fd::OwnedFd>,
    pub planes: Vec<ExportedPlane>,
}
#[derive(Clone, Copy, Debug)]
pub struct ExportedPlane {
    pub object_index: u32, pub offset: u32, pub stride: u32, pub modifier: u64,
}
impl ExportedFrame {
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=8192).contains(&self.width) || !(1..=8192).contains(&self.height)
            || !(1..=4).contains(&self.objects.len()) || !(1..=4).contains(&self.planes.len()) {
            return Err("invalid GPU frame dimensions/object/plane counts".into());
        }
        for plane in &self.planes {
            if plane.object_index as usize >= self.objects.len() || plane.stride == 0
                || plane.offset.checked_add(plane.stride).is_none() {
                return Err("invalid GPU plane descriptor".into());
            }
        }
        Ok(())
    }
}

/// Owns the GPU decode surface and exported object fds. Keep this alive until
/// the compositor has released the imported buffer, not merely acknowledged
/// the present request. Frames can outlive their decoder safely.
pub struct HardwareFrame { api: Rc<Api>, pointer: NonNull<c_void>, info: RawInfo }
impl Decoder {
    pub fn open(codec: &str, device: &Path, extra: &[u8]) -> Result<Self, String> {
        let plugin = match std::env::var_os("PENGUDROID_VIDEO_BRIDGE") {
            Some(path) => std::path::PathBuf::from(path),
            None => std::env::current_exe().map_err(|e| e.to_string())?.parent()
                .ok_or("executable has no directory")?.join("libpengudroid_video.so"),
        };
        Self::open_plugin(&plugin, codec, device, extra)
    }
    pub fn open_plugin(plugin: &Path, codec: &str, device: &Path, extra: &[u8]) -> Result<Self, String> {
        use std::os::unix::ffi::OsStrExt;
        if extra.len() > 1024 * 1024 { return Err("codec extra data exceeds 1 MiB".into()); }
        let plugin_name = CString::new(plugin.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
        let library = Library::open(&plugin_name)
            .map_err(|e| format!("{e}; build the host plugin with tools/build-video-bridge.sh"))?;
        let codec = CString::new(codec).map_err(|e| e.to_string())?;
        let device = CString::new(device.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
        unsafe {
            let version: unsafe extern "C" fn() -> u32 = library.symbol(c"pd_video_abi")?;
            if version() != 1 { return Err("unsupported host video bridge ABI".into()); }
            let open: Open = library.symbol(c"pd_video_open")?;
            let api = Rc::new(Api {
                close: library.symbol(c"pd_video_close")?, send: library.symbol(c"pd_video_send")?,
                receive: library.symbol(c"pd_video_receive")?, release: library.symbol(c"pd_video_frame_release")?,
                flush: library.symbol(c"pd_video_flush")?, luma: library.symbol(c"pd_video_test_luma")?,
                _library: library,
            });
            let mut error = [0u8; 512];
            let pointer = open(codec.as_ptr(), device.as_ptr(), extra.as_ptr(), extra.len() as u32,
                error.as_mut_ptr().cast(), error.len() as u32);
            let pointer = NonNull::new(pointer).ok_or_else(|| error_text(&error, -1))?;
            Ok(Self { api, pointer, _thread: PhantomData })
        }
    }
    /// Send one complete compressed access unit with a microsecond timestamp.
    pub fn send(&mut self, bytes: &[u8], pts_us: i64) -> Result<SendStatus, String> {
        if bytes.is_empty() || bytes.len() > 16 * 1024 * 1024 { return Err("video packet must contain 1 byte to 16 MiB".into()); }
        self.send_raw(bytes.as_ptr(), bytes.len() as u32, pts_us)
    }
    pub fn end_of_stream(&mut self) -> Result<SendStatus, String> {
        self.send_raw(std::ptr::null(), 0, 0)
    }
    fn send_raw(&mut self, bytes: *const u8, size: u32, pts: i64) -> Result<SendStatus, String> {
        let mut error = [0u8; 512];
        let status = unsafe { (self.api.send)(self.pointer.as_ptr(), bytes, size, pts, error.as_mut_ptr().cast(), error.len() as u32) };
        match status { 0 => Ok(SendStatus::Accepted), 1 => Ok(SendStatus::DrainFirst), _ => Err(error_text(&error, status)) }
    }
    pub fn receive(&mut self) -> Result<ReceiveStatus, String> {
        let mut error = [0u8; 512];
        let mut pointer = std::ptr::null_mut();
        let mut info = RawInfo::default();
        let status = unsafe { (self.api.receive)(self.pointer.as_ptr(), &mut pointer, &mut info, error.as_mut_ptr().cast(), error.len() as u32) };
        match status {
            0 => {
                let pointer = NonNull::new(pointer).ok_or("backend returned a null hardware frame")?;
                let frame = HardwareFrame { api: self.api.clone(), pointer, info };
                if !(1..=8192).contains(&info.width) || !(1..=8192).contains(&info.height)
                    || !(1..=4).contains(&info.object_count) || !(1..=4).contains(&info.plane_count)
                    || info.objects[..info.object_count.min(4) as usize].iter().any(|o| o.fd < 0)
                    || info.planes[..info.plane_count.min(4) as usize].iter().any(|p| p.object_index >= info.object_count || p.stride == 0) {
                    return Err("invalid hardware frame descriptor".into());
                }
                Ok(ReceiveStatus::Frame(frame))
            }
            1 => Ok(ReceiveStatus::NeedsInput), 2 => Ok(ReceiveStatus::EndOfStream),
            3 => Ok(ReceiveStatus::FramesHeld), _ => Err(error_text(&error, status)),
        }
    }
    /// Flush queued codec work; previously returned GPU frames remain owned.
    pub fn flush(&mut self) { unsafe { (self.api.flush)(self.pointer.as_ptr()); } }
}
impl Drop for Decoder { fn drop(&mut self) { unsafe { (self.api.close)(self.pointer.as_ptr()); } } }
impl HardwareFrame {
    pub fn width(&self) -> u32 { self.info.width }
    pub fn height(&self) -> u32 { self.info.height }
    pub fn fourcc(&self) -> u32 { self.info.fourcc }
    pub fn pts_us(&self) -> i64 { self.info.pts_us }
    pub fn planes(&self) -> &[Plane] { &self.info.planes[..self.info.plane_count as usize] }
    pub fn object_count(&self) -> usize { self.info.object_count as usize }
    pub fn object_fd(&self, index: usize) -> Option<BorrowedFd<'_>> {
        if index >= self.object_count() { return None; }
        Some(unsafe { BorrowedFd::borrow_raw(self.info.objects[index].fd as RawFd) })
    }
    pub fn object_modifier(&self, index: usize) -> Option<u64> {
        if index >= self.object_count() { return None; }
        Some(self.info.objects[index].modifier)
    }
    pub fn export(&self) -> Result<ExportedFrame, String> {
        let objects = (0..self.object_count()).map(|index| self.object_fd(index).unwrap().try_clone_to_owned()
            .map_err(|e| e.to_string())).collect::<Result<Vec<_>, _>>()?;
        let planes = self.planes().iter().map(|p| ExportedPlane {
            object_index: p.object_index, offset: p.offset, stride: p.stride,
            modifier: self.object_modifier(p.object_index as usize).unwrap(),
        }).collect();
        let frame = ExportedFrame { width: self.width(), height: self.height(), fourcc: self.fourcc(), objects, planes };
        frame.validate()?;
        Ok(frame)
    }
    /// Diagnostic CPU readback, explicitly separate from GPU presentation.
    pub fn test_luma(&self) -> Result<u32, String> {
        let mut error = [0u8; 512]; let mut sample = 0;
        let status = unsafe { (self.api.luma)(self.pointer.as_ptr(), &mut sample, error.as_mut_ptr().cast(), error.len() as u32) };
        if status == 0 { Ok(sample) } else { Err(error_text(&error, status)) }
    }
}
impl Drop for HardwareFrame { fn drop(&mut self) { unsafe { (self.api.release)(self.pointer.as_ptr()); } } }
