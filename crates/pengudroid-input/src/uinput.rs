//! Virtual input devices through `/dev/uinput`.
//!
//! This is the "real native Linux call" half of PenguDroid's input: instead of
//! synthesising an Android `MotionEvent` and hoping the app's view tree is what
//! receives it, the events are written into the kernel's input subsystem as a
//! genuine multitouch device. Anything that reads input the way a device does —
//! a game's own `AInputQueue`, an SDL2/Unity/Unreal loop, a compositor — sees
//! them exactly as it would see a finger on a touchscreen.
//!
//! Two devices are provided:
//!
//! * [`VirtualTouch`] — a multitouch panel (`ABS_MT_*` + `BTN_TOUCH`), which is
//!   what games expect. Coordinates are in the panel's own units, so the caller
//!   decides the resolution.
//! * [`VirtualKeyboard`] — a plain keyboard, used by the keymapper to turn
//!   physical keys into taps and holds.
//!
//! Both are created with the legacy `uinput_user_dev` layout rather than
//! `UI_DEV_SETUP`/`UI_ABS_SETUP`: it is what every kernel since 2.6 understands,
//! and it carries the axis ranges in one write.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Kernel ABI
// ---------------------------------------------------------------------------

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;

const SYN_REPORT: u16 = 0;

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_MT_SLOT: u16 = 0x2f;
const ABS_MT_TOUCH_MAJOR: u16 = 0x30;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TRACKING_ID: u16 = 0x39;

const BTN_TOUCH: u16 = 0x14a;

/// `ABS_CNT` — the size of the per-axis arrays in `uinput_user_dev`.
const ABS_CNT: usize = 64;
const UINPUT_MAX_NAME_SIZE: usize = 80;

/// `_IOW('U', nr, int)`.
const fn iow_u(nr: u32) -> libc::c_ulong {
    ((1u64 << 30) | (4u64 << 16) | ((b'U' as u64) << 8) | nr as u64) as libc::c_ulong
}

/// `_IO('U', nr)`.
const fn io_u(nr: u32) -> libc::c_ulong {
    (((b'U' as u64) << 8) | nr as u64) as libc::c_ulong
}

const UI_SET_EVBIT: libc::c_ulong = iow_u(100);
const UI_SET_KEYBIT: libc::c_ulong = iow_u(101);
const UI_SET_ABSBIT: libc::c_ulong = iow_u(103);
const UI_DEV_CREATE: libc::c_ulong = io_u(1);
const UI_DEV_DESTROY: libc::c_ulong = io_u(2);

/// `struct input_id`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

/// `struct uinput_user_dev` (the pre-`UI_DEV_SETUP` layout).
#[repr(C)]
struct UinputUserDev {
    name: [u8; UINPUT_MAX_NAME_SIZE],
    id: InputId,
    ff_effects_max: u32,
    absmax: [i32; ABS_CNT],
    absmin: [i32; ABS_CNT],
    absfuzz: [i32; ABS_CNT],
    absflat: [i32; ABS_CNT],
}

impl UinputUserDev {
    fn new(name: &str) -> UinputUserDev {
        let mut dev = UinputUserDev {
            name: [0; UINPUT_MAX_NAME_SIZE],
            // BUS_VIRTUAL: the device is not attached to real hardware.
            id: InputId {
                bustype: 0x06,
                vendor: 0x5044,  // "PD"
                product: 0x0001,
                version: 1,
            },
            ff_effects_max: 0,
            absmax: [0; ABS_CNT],
            absmin: [0; ABS_CNT],
            absfuzz: [0; ABS_CNT],
            absflat: [0; ABS_CNT],
        };
        let bytes = name.as_bytes();
        let n = bytes.len().min(UINPUT_MAX_NAME_SIZE - 1);
        dev.name[..n].copy_from_slice(&bytes[..n]);
        dev
    }
}

/// `struct input_event` on a 64-bit kernel: `timeval` (16) + type/code/value (8).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct InputEvent {
    pub sec: i64,
    pub usec: i64,
    pub kind: u16,
    pub code: u16,
    pub value: i32,
}

// ---------------------------------------------------------------------------
// Devices
// ---------------------------------------------------------------------------

/// A created uinput device.
pub struct VirtualDevice {
    file: File,
    name: String,
    /// `/dev/input/eventN`, resolved after creation.
    node: Option<PathBuf>,
}

impl VirtualDevice {
    fn open() -> io::Result<File> {
        OpenOptions::new()
            .write(true)
            .open(Path::new("/dev/uinput"))
            .or_else(|_| OpenOptions::new().write(true).open(Path::new("/dev/input/uinput")))
    }

    fn set_bit(&self, request: libc::c_ulong, bit: u16) -> io::Result<()> {
        let rc = unsafe { libc::ioctl(self.file.as_raw_fd(), request, bit as libc::c_int) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Write one `input_event`.
    pub fn emit(&mut self, kind: u16, code: u16, value: i32) -> io::Result<()> {
        let ev = InputEvent {
            kind,
            code,
            value,
            ..InputEvent::default()
        };
        self.write_events(&[ev])
    }

    fn write_events(&mut self, events: &[InputEvent]) -> io::Result<()> {
        use std::io::Write;
        let bytes = unsafe {
            std::slice::from_raw_parts(
                events.as_ptr() as *const u8,
                std::mem::size_of_val(events),
            )
        };
        self.file.write_all(bytes)
    }

    /// `SYN_REPORT`: ends one atomic event frame.
    pub fn sync(&mut self) -> io::Result<()> {
        self.emit(EV_SYN, SYN_REPORT, 0)
    }

    /// The device's name as the kernel knows it.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// `/dev/input/eventN`, resolved once the kernel has published the device.
    pub fn node(&self) -> Option<&Path> {
        self.node.as_deref()
    }

    /// Look the device up in `/sys/class/input` by name.
    ///
    /// `UI_DEV_CREATE` returns before udev has made the node, so this retries
    /// briefly; a caller that only needs to inject can ignore the result.
    fn resolve_node(&mut self) {
        for _ in 0..50 {
            if let Some(path) = find_event_node(&self.name) {
                self.node = Some(path);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

impl Drop for VirtualDevice {
    fn drop(&mut self) {
        unsafe {
            libc::ioctl(self.file.as_raw_fd(), UI_DEV_DESTROY);
        }
    }
}

/// Find `/dev/input/eventN` whose sysfs name matches `name`.
pub fn find_event_node(name: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir("/sys/class/input").ok()?;
    for entry in entries.flatten() {
        let file = entry.file_name();
        let file = file.to_string_lossy();
        if !file.starts_with("event") {
            continue;
        }
        let name_path = entry.path().join("device/name");
        let Ok(found) = std::fs::read_to_string(&name_path) else {
            continue;
        };
        if found.trim() == name {
            return Some(PathBuf::from("/dev/input").join(file.as_ref()));
        }
    }
    None
}

/// A virtual multitouch panel.
///
/// Coordinates are in the panel's units: `width`/`height` are the maximum values
/// reported for `ABS_MT_POSITION_X`/`_Y`, so a caller working in screen pixels
/// passes the screen size and needs no scaling.
pub struct VirtualTouch {
    dev: VirtualDevice,
    width: i32,
    height: i32,
    /// Current slot, so a second finger can be added later without reworking the
    /// API: `ABS_MT_SLOT` is emitted whenever it changes.
    slot: i32,
    tracking: i32,
}

impl VirtualTouch {
    /// Create the panel. Requires write access to `/dev/uinput`.
    pub fn create(name: &str, width: u32, height: u32) -> io::Result<VirtualTouch> {
        let file = VirtualDevice::open()?;
        let mut dev = VirtualDevice {
            file,
            name: name.to_string(),
            node: None,
        };
        dev.set_bit(UI_SET_EVBIT, EV_KEY)?;
        dev.set_bit(UI_SET_EVBIT, EV_ABS)?;
        dev.set_bit(UI_SET_EVBIT, EV_SYN)?;
        dev.set_bit(UI_SET_KEYBIT, BTN_TOUCH)?;
        for axis in [
            ABS_X,
            ABS_Y,
            ABS_MT_SLOT,
            ABS_MT_TOUCH_MAJOR,
            ABS_MT_POSITION_X,
            ABS_MT_POSITION_Y,
            ABS_MT_TRACKING_ID,
        ] {
            dev.set_bit(UI_SET_ABSBIT, axis)?;
        }

        let mut setup = UinputUserDev::new(name);
        setup.absmin[ABS_X as usize] = 0;
        setup.absmax[ABS_X as usize] = width as i32;
        setup.absmin[ABS_Y as usize] = 0;
        setup.absmax[ABS_Y as usize] = height as i32;
        setup.absmax[ABS_MT_POSITION_X as usize] = width as i32;
        setup.absmax[ABS_MT_POSITION_Y as usize] = height as i32;
        // A tracking id is a free-running tag; the kernel only needs a range.
        setup.absmin[ABS_MT_TRACKING_ID as usize] = -1;
        setup.absmax[ABS_MT_TRACKING_ID as usize] = 65535;
        setup.absmax[ABS_MT_SLOT as usize] = 9;
        setup.absmax[ABS_MT_TOUCH_MAJOR as usize] = 255;

        use std::io::Write;
        let bytes = unsafe {
            std::slice::from_raw_parts(
                &setup as *const UinputUserDev as *const u8,
                std::mem::size_of::<UinputUserDev>(),
            )
        };
        dev.file.write_all(bytes)?;
        let rc = unsafe { libc::ioctl(dev.file.as_raw_fd(), UI_DEV_CREATE) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        dev.resolve_node();
        Ok(VirtualTouch {
            dev,
            width: width as i32,
            height: height as i32,
            slot: 0,
            tracking: 1,
        })
    }

    pub fn node(&self) -> Option<&Path> {
        self.dev.node()
    }

    pub fn size(&self) -> (i32, i32) {
        (self.width, self.height)
    }

    /// Clamp a coordinate into the panel.
    fn clamp(&self, x: f32, y: f32) -> (i32, i32) {
        let x = x.round().clamp(0.0, self.width as f32) as i32;
        let y = y.round().clamp(0.0, self.height as f32) as i32;
        (x, y)
    }

    /// Finger down at `(x, y)`.
    pub fn down(&mut self, x: f32, y: f32) -> io::Result<()> {
        let (x, y) = self.clamp(x, y);
        self.tracking += 1;
        let tracking = self.tracking;
        self.dev.emit(EV_ABS, ABS_MT_SLOT, self.slot)?;
        self.dev.emit(EV_ABS, ABS_MT_TRACKING_ID, tracking)?;
        self.dev.emit(EV_ABS, ABS_MT_POSITION_X, x)?;
        self.dev.emit(EV_ABS, ABS_MT_POSITION_Y, y)?;
        self.dev.emit(EV_ABS, ABS_MT_TOUCH_MAJOR, 8)?;
        // The single-touch axes are what older consumers read.
        self.dev.emit(EV_ABS, ABS_X, x)?;
        self.dev.emit(EV_ABS, ABS_Y, y)?;
        self.dev.emit(EV_KEY, BTN_TOUCH, 1)?;
        self.dev.sync()
    }

    /// Move the finger.
    pub fn move_to(&mut self, x: f32, y: f32) -> io::Result<()> {
        let (x, y) = self.clamp(x, y);
        self.dev.emit(EV_ABS, ABS_MT_SLOT, self.slot)?;
        self.dev.emit(EV_ABS, ABS_MT_POSITION_X, x)?;
        self.dev.emit(EV_ABS, ABS_MT_POSITION_Y, y)?;
        self.dev.emit(EV_ABS, ABS_X, x)?;
        self.dev.emit(EV_ABS, ABS_Y, y)?;
        self.dev.sync()
    }

    /// Finger up.
    pub fn up(&mut self) -> io::Result<()> {
        self.dev.emit(EV_ABS, ABS_MT_SLOT, self.slot)?;
        self.dev.emit(EV_ABS, ABS_MT_TRACKING_ID, -1)?;
        self.dev.emit(EV_KEY, BTN_TOUCH, 0)?;
        self.dev.sync()
    }

    /// A complete tap: down, a short hold, up.
    pub fn tap(&mut self, x: f32, y: f32) -> io::Result<()> {
        self.down(x, y)?;
        std::thread::sleep(std::time::Duration::from_millis(40));
        self.up()
    }

    /// A drag from `(x0, y0)` to `(x1, y1)` in `steps` moves.
    pub fn swipe(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, steps: u32) -> io::Result<()> {
        let steps = steps.max(1);
        self.down(x0, y0)?;
        for i in 1..=steps {
            let t = i as f32 / steps as f32;
            self.move_to(x0 + (x1 - x0) * t, y0 + (y1 - y0) * t)?;
            std::thread::sleep(std::time::Duration::from_millis(8));
        }
        self.up()
    }
}

/// A virtual keyboard, used by the keymapper.
pub struct VirtualKeyboard {
    dev: VirtualDevice,
}

impl VirtualKeyboard {
    pub fn create(name: &str) -> io::Result<VirtualKeyboard> {
        let file = VirtualDevice::open()?;
        let mut dev = VirtualDevice {
            file,
            name: name.to_string(),
            node: None,
        };
        dev.set_bit(UI_SET_EVBIT, EV_KEY)?;
        dev.set_bit(UI_SET_EVBIT, EV_SYN)?;
        // Every key the kernel knows: the keymapper may bind any of them.
        for code in 1..=255u16 {
            dev.set_bit(UI_SET_KEYBIT, code)?;
        }
        let setup = UinputUserDev::new(name);
        use std::io::Write;
        let bytes = unsafe {
            std::slice::from_raw_parts(
                &setup as *const UinputUserDev as *const u8,
                std::mem::size_of::<UinputUserDev>(),
            )
        };
        dev.file.write_all(bytes)?;
        let rc = unsafe { libc::ioctl(dev.file.as_raw_fd(), UI_DEV_CREATE) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        dev.resolve_node();
        Ok(VirtualKeyboard { dev })
    }

    pub fn node(&self) -> Option<&Path> {
        self.dev.node()
    }

    /// Press or release a key by Linux keycode (`KEY_A` = 30, ...).
    pub fn key(&mut self, code: u16, pressed: bool) -> io::Result<()> {
        self.dev.emit(EV_KEY, code, i32::from(pressed))?;
        self.dev.sync()
    }
}

// ---------------------------------------------------------------------------
// Reading real devices (the keymapper's input side)
// ---------------------------------------------------------------------------

/// An opened `/dev/input/eventN` that yields key events.
pub struct EventReader {
    file: File,
    path: PathBuf,
}

impl EventReader {
    pub fn open(path: &Path) -> io::Result<EventReader> {
        let file = OpenOptions::new().read(true).open(path)?;
        Ok(EventReader {
            file,
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Block until one event arrives.
    pub fn read_event(&mut self) -> io::Result<InputEvent> {
        use std::io::Read;
        let mut buf = [0u8; std::mem::size_of::<InputEvent>()];
        self.file.read_exact(&mut buf)?;
        // SAFETY: `InputEvent` is `repr(C)` and the buffer is exactly its size.
        Ok(unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const InputEvent) })
    }

    /// Block until a key event, returning `(code, pressed)`.
    pub fn read_key(&mut self) -> io::Result<(u16, bool)> {
        loop {
            let ev = self.read_event()?;
            if ev.kind == EV_KEY {
                return Ok((ev.code, ev.value != 0));
            }
        }
    }
}

/// Every readable keyboard-like device under `/dev/input`.
///
/// The capability mask lives in sysfs, not in `/dev`: a keyboard sets bits well
/// past the pointer range, so a mask longer than 32 bits means "this device has
/// keys we can map". Devices whose mask cannot be read are skipped rather than
/// guessed at.
pub fn keyboards() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/sys/class/input") else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy().into_owned();
        if !name.starts_with("event") {
            continue;
        }
        let caps = entry.path().join("device/capabilities/key");
        let Ok(mask) = std::fs::read_to_string(&caps) else {
            continue;
        };
        // 32 bits of mask = 8 hex digits; a keyboard always sets more.
        if mask.trim().len() > 8 {
            out.push(PathBuf::from("/dev/input").join(&name));
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_the_kernel_headers() {
        // `_IOW('U', 100, int)` = 0x40045564, `_IO('U', 1)` = 0x5501.
        assert_eq!(UI_SET_EVBIT, 0x4004_5564);
        assert_eq!(UI_SET_KEYBIT, 0x4004_5565);
        assert_eq!(UI_SET_ABSBIT, 0x4004_5567);
        assert_eq!(UI_DEV_CREATE, 0x5501);
        assert_eq!(UI_DEV_DESTROY, 0x5502);
    }

    #[test]
    fn user_dev_layout_matches_the_kernel() {
        // 80 name + 8 input_id + 4 ff_effects_max + 4 * 64 * 4 axis arrays.
        assert_eq!(std::mem::size_of::<UinputUserDev>(), 1116);
        assert_eq!(std::mem::size_of::<InputEvent>(), 24);
    }

    /// End-to-end: create a panel, tap, and read the events back from the node
    /// the kernel published. Skips when `/dev/uinput` is not writable.
    #[test]
    fn tap_is_visible_on_the_created_device() {
        let Ok(mut touch) = VirtualTouch::create("pengudroid-test-touch", 720, 1280) else {
            eprintln!("skipping: /dev/uinput is not writable");
            return;
        };
        let node = touch.node().map(|p| p.to_path_buf());
        let Some(node) = node else {
            eprintln!("skipping: the kernel did not publish an event node");
            return;
        };
        // Reading `/dev/input/eventN` needs the `input` group (or root); writing
        // to `/dev/uinput` does not. Injection is what matters in practice, so a
        // permission error here is a skip, not a failure.
        let mut reader = match EventReader::open(&node) {
            Ok(r) => r,
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!("skipping read-back: {} is not readable ({e})", node.display());
                return;
            }
            Err(e) => panic!("open {}: {e}", node.display()),
        };
        touch.tap(100.0, 200.0).expect("tap");

        let mut saw_touch_down = false;
        let mut saw_touch_up = false;
        let mut x = None;
        for _ in 0..64 {
            let ev = reader.read_event().expect("read");
            if ev.kind == EV_KEY && ev.code == BTN_TOUCH {
                if ev.value == 1 {
                    saw_touch_down = true;
                } else {
                    saw_touch_up = true;
                }
            }
            if ev.kind == EV_ABS && ev.code == ABS_MT_POSITION_X {
                x = Some(ev.value);
            }
            if saw_touch_down && saw_touch_up {
                break;
            }
        }
        assert!(saw_touch_down, "no BTN_TOUCH press came back");
        assert!(saw_touch_up, "no BTN_TOUCH release came back");
        assert_eq!(x, Some(100), "the X coordinate did not survive the round trip");
    }
}
