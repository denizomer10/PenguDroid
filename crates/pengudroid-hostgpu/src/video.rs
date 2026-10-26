//! Host video-decode capability discovery. This does not enable guest decoding.
//! VA-API reference: https://intel.github.io/libva/group__api__core.html
use std::ffi::{c_char, c_int, c_void, CStr};
use std::os::fd::AsRawFd;
use std::path::Path;

#[derive(Debug)]
pub struct VideoDecodeInfo {
    pub device: String,
    pub vendor: String,
    pub version: (i32, i32),
    pub profiles: Vec<String>,
}

struct Library(*mut c_void);
impl Library {
    fn open(name: &CStr) -> Result<Self, String> {
        let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() { return Err(format!("cannot load {}", name.to_string_lossy())); }
        Ok(Self(handle))
    }
    // The signatures below are the public libva ABI; handles outlive calls.
    unsafe fn symbol<T: Copy>(&self, name: &CStr) -> Result<T, String> {
        let pointer = unsafe { libc::dlsym(self.0, name.as_ptr()) };
        if pointer.is_null() { return Err(format!("missing {}", name.to_string_lossy())); }
        assert_eq!(std::mem::size_of::<T>(), std::mem::size_of_val(&pointer));
        Ok(unsafe { std::mem::transmute_copy(&pointer) })
    }
}
impl Drop for Library {
    fn drop(&mut self) { unsafe { libc::dlclose(self.0); } }
}
type Display = *mut c_void;
struct Session { display: Display, terminate: unsafe extern "C" fn(Display) -> c_int }
impl Drop for Session {
    fn drop(&mut self) { unsafe { (self.terminate)(self.display); } }
}
fn checked_count(value: i32) -> Result<usize, String> {
    if !(1..=4096).contains(&value) { return Err(format!("invalid VA-API capacity {value}")); }
    Ok(value as usize)
}

/// Query actual VLD entrypoints for the selected render node. No codec support
/// is inferred from a PCI vendor name. Other backends (including NVDEC) need
/// their own discovery; a missing VA-API driver does not mean the GPU lacks them.
pub fn decode_info(device: &Path) -> Result<VideoDecodeInfo, String> {
    let file = std::fs::OpenOptions::new().read(true).write(true).open(device)
        .map_err(|e| format!("{}: {e}", device.display()))?;
    let va = Library::open(c"libva.so.2")?;
    let drm = Library::open(c"libva-drm.so.2")?;
    unsafe {
        let get_display: unsafe extern "C" fn(c_int) -> Display = drm.symbol(c"vaGetDisplayDRM")?;
        let initialize: unsafe extern "C" fn(Display, *mut c_int, *mut c_int) -> c_int = va.symbol(c"vaInitialize")?;
        let terminate = va.symbol(c"vaTerminate")?;
        let vendor: unsafe extern "C" fn(Display) -> *const c_char = va.symbol(c"vaQueryVendorString")?;
        let max_profiles: unsafe extern "C" fn(Display) -> c_int = va.symbol(c"vaMaxNumProfiles")?;
        let max_entries: unsafe extern "C" fn(Display) -> c_int = va.symbol(c"vaMaxNumEntrypoints")?;
        let query_profiles: unsafe extern "C" fn(Display, *mut c_int, *mut c_int) -> c_int = va.symbol(c"vaQueryConfigProfiles")?;
        let query_entries: unsafe extern "C" fn(Display, c_int, *mut c_int, *mut c_int) -> c_int = va.symbol(c"vaQueryConfigEntrypoints")?;
        let profile_name: unsafe extern "C" fn(c_int) -> *const c_char = va.symbol(c"vaProfileStr")?;
        let display = get_display(file.as_raw_fd());
        if display.is_null() { return Err("VA-API returned no DRM display".into()); }
        let (mut major, mut minor) = (0, 0);
        let status = initialize(display, &mut major, &mut minor);
        if status != 0 { return Err(format!("vaInitialize failed: {status}")); }
        let _session = Session { display, terminate };
        let mut profiles = vec![0; checked_count(max_profiles(display))?];
        let mut count = 0;
        let status = query_profiles(display, profiles.as_mut_ptr(), &mut count);
        if status != 0 || count < 0 || count as usize > profiles.len() {
            return Err(format!("vaQueryConfigProfiles failed: {status}, count {count}"));
        }
        let mut entries = vec![0; checked_count(max_entries(display))?];
        let mut supported = Vec::new();
        for profile in &profiles[..count as usize] {
            let mut count = 0;
            let status = query_entries(display, *profile, entries.as_mut_ptr(), &mut count);
            if status != 0 || count < 0 || count as usize > entries.len() {
                return Err(format!("vaQueryConfigEntrypoints failed: {status}, count {count}"));
            }
            // VAEntrypointVLD = 1; encode-only profiles are not decode support.
            if entries[..count as usize].contains(&1) {
                let name = profile_name(*profile);
                supported.push(if name.is_null() { format!("VAProfile({profile})") }
                    else { CStr::from_ptr(name).to_string_lossy().into_owned() });
            }
        }
        let vendor = vendor(display);
        Ok(VideoDecodeInfo {
            device: device.display().to_string(),
            vendor: if vendor.is_null() { String::new() } else { CStr::from_ptr(vendor).to_string_lossy().into_owned() },
            version: (major, minor), profiles: supported,
        })
    }
}

mod decode;
pub use decode::{Decoder, ExportedFrame, ExportedPlane, HardwareFrame, Plane, ReceiveStatus, SendStatus};
