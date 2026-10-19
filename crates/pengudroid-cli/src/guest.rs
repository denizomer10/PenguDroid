//! The guest runtime: a real Linux process that owns a Wayland window.
//!
//! This is the MVP stand-in for an ART-hosted app. It connects to the daemon as
//! a guest, asks for a surface, receives the backing memory as an fd, maps it,
//! registers itself for `pengudroid ps`, and animates the surface by writing
//! pixels and asking the daemon to present. Crucially it is an ordinary process
//! — `ps aux` shows it, and there is no container or VM around it.
//!
//! When the ART integration lands, this loop is replaced by the Android
//! `SurfaceFlinger`-equivalent: the guest writes into the same fd via a
//! `gralloc`-compatible `GraphicBuffer`, and the calls below become the EGL
//! swap path.

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use pengudroid_proto::{Message, Role};

use crate::client::Client;

static STOP: AtomicBool = AtomicBool::new(false);

struct GuestArgs {
    socket: std::path::PathBuf,
    package: String,
    component: String,
    title: String,
    width: u32,
    height: u32,
    /// APK to run (the daemon's `RunApk` passes `--apk`; the CLI passes the
    /// parsed fields directly).
    apk: Option<String>,
}

fn parse_args(args: &[String]) -> Result<GuestArgs, String> {
    use pengudroid_proto::default_socket_path;
    let mut g = GuestArgs {
        socket: default_socket_path(),
        package: "unknown".into(),
        component: String::new(),
        title: "PenguDroid app".into(),
        width: 720,
        height: 1280,
        apk: None,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned();
        match a.as_str() {
            "--socket" => g.socket = val().ok_or("--socket needs a value")?.into(),
            "--package" => g.package = val().ok_or("--package needs a value")?,
            "--component" => g.component = val().ok_or("--component needs a value")?,
            "--title" => g.title = val().ok_or("--title needs a value")?,
            "--apk" => g.apk = Some(val().ok_or("--apk needs a value")?),
            "--width" => {
                g.width = val().ok_or("--width needs a value")?.parse().map_err(|_| "bad width")?
            }
            "--height" => {
                g.height = val().ok_or("--height needs a value")?.parse().map_err(|_| "bad height")?
            }
            _ => {}
        }
    }
    Ok(g)
}

extern "C" fn on_sigint(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

/// Entry point for the hidden `__guest` subcommand. Returns a process exit code.
pub fn guest_main(args: &[String]) -> i32 {
    let g = match parse_args(args) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("guest: {e}");
            return 2;
        }
    };
    // Started by the daemon's `RunApk`: derive identity from the APK itself so
    // the daemon only has to pass a path.
    let mut g = g;
    if let Some(apk) = g.apk.clone() {
        match crate::apk::read_apk(&apk) {
            Ok(info) => {
                if g.package == "unknown" {
                    g.package = info.package.clone();
                }
                if g.component.is_empty() {
                    g.component = info.main_activity_name().map(str::to_string).unwrap_or_default();
                }
                if g.title == "PenguDroid app" {
                    g.title = info.label.clone().unwrap_or_else(|| info.package.clone());
                }
            }
            Err(e) => eprintln!("guest: warning: {e}"),
        }
    }
    // SAFETY: correct handler signature.
    unsafe {
        libc::signal(libc::SIGINT, on_sigint as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_sigint as *const () as libc::sighandler_t);
    }

    let client = match Client::connect_to(&g.socket).and_then(|c| {
        c.handshake(Role::Guest)?;
        Ok(c)
    }) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("guest: {e}");
            return 1;
        }
    };

    // Ask for a surface; the daemon creates the Wayland toplevel and hands back
    // the memfd backing it.
    let (reply, fds) = match client.request(&Message::CreateSurface {
        width: g.width,
        height: g.height,
        title: g.title.clone(),
    }) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("guest: CreateSurface failed: {e}");
            return 1;
        }
    };
    let (surface_id, stride, size, shm_name) = match reply {
        Message::SurfaceCreated { surface_id, stride, size, shm_name, .. } => {
            (surface_id, stride, size, shm_name)
        }
        Message::Error { code, message } => {
            eprintln!("guest: daemon error {code}: {message}");
            return 1;
        }
        other => {
            eprintln!("guest: unexpected reply {other:?}");
            return 1;
        }
    };
    let Some(fd) = fds.into_iter().next() else {
        eprintln!("guest: daemon did not send the surface fd");
        return 1;
    };

    // Register with the daemon so `pengudroid ps` lists us.
    let _ = client.request(&Message::RegisterProc {
        pid: std::process::id() as i32,
        package: g.package.clone(),
        component: g.component.clone(),
        surface_id,
    });

    println!(
        "guest pid {}: surface {surface_id} '{shm_name}' {}×{} stride {stride} size {size}",
        std::process::id(),
        g.width,
        g.height
    );

    let mut map = match Mmap::new(&fd, size as usize) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("guest: cannot map surface memory: {e}");
            return 1;
        }
    };

    // Prefer the GPU path: render on the host GPU and hand the daemon a dma-buf
    // (zero-copy) instead of writing pixels through wl_shm.
    let mut gpu = pengudroid_hostgpu::Offscreen::new(g.width, g.height).ok();
    if gpu.is_some() {
        println!(
            "guest pid {}: rendering on the host GPU via dma-buf",
            std::process::id()
        );
    } else {
        println!(
            "guest pid {}: GPU unavailable; using CPU (wl_shm) path",
            std::process::id()
        );
    }

    let mut phase: u32 = 0;
    let mut announced = false;
    let mut present_errors: u32 = 0;
    while !STOP.load(Ordering::Relaxed) {
        let mut presented = false;
        if let Some(off) = gpu.as_mut() {
            let t = (phase % 256) as f32 / 255.0;
            if off.render(0.1 + 0.2 * t, 0.3, 0.7 - 0.2 * t, 1.0).is_ok() {
                if let Ok(d) = off.export_dmabuf() {
                    let msg = Message::PresentDmabuf {
                        surface_id,
                        width: d.width,
                        height: d.height,
                        stride: d.stride,
                        fourcc: d.fourcc,
                        modifier: d.modifier,
                        offset: d.offset,
                    };
                    if matches!(
                        client.request_with_fds(&msg, &[d.fd.as_raw_fd()]),
                        Ok(Message::Ack)
                    ) {
                        presented = true;
                        present_errors = 0;
                        if !announced {
                            println!(
                                "guest: first dma-buf presented ({}x{}, fourcc {:#x}) — zero-copy",
                                d.width, d.height, d.fourcc
                            );
                            announced = true;
                        }
                    }
                }
            }
        }
        if !presented {
            paint(&mut map, g.width, g.height, stride, phase);
            match client.request(&Message::PresentFrame { surface_id }) {
                Ok((Message::Ack, _)) => present_errors = 0,
                Ok((Message::Error { message, .. }, _)) => {
                    eprintln!("guest: present failed: {message}");
                    break;
                }
                Ok(_) => {}
                Err(e) if present_errors == 0 => {
                    // One retry: the daemon may be briefly busy serving another
                    // connection. A second consecutive failure means it is gone.
                    present_errors += 1;
                    eprintln!("guest: present failed ({e}); retrying once");
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
                Err(_) => break, // daemon went away
            }
        }
        phase = phase.wrapping_add(4);
        std::thread::sleep(Duration::from_millis(120));
    }

    // Close our part of the loop first: the daemon then drops this guest's
    // entry from `ps` and destroys the toplevel, so an exited guest never
    // leaves an orphaned window or a phantom process behind.
    let _ = client.request(&Message::UnregisterProc { pid: std::process::id() as i32 });
    let _ = client.send(&Message::Goodbye);
    println!("guest pid {}: exiting", std::process::id());
    0
}

/// Draw a moving diagonal gradient in RGBA, byte order B,G,R,A.
fn paint(map: &mut Mmap, width: u32, height: u32, stride: u32, phase: u32) {
    let bytes = map.as_mut_slice();
    for y in 0..height {
        for x in 0..width {
            let o = (y * stride + x * 4) as usize;
            if o + 3 >= bytes.len() {
                continue;
            }
            let t = ((x + y + phase) % 256) as u8;
            bytes[o] = 160u8.wrapping_add(t / 3); // B
            bytes[o + 1] = 60u8.wrapping_add(t / 4); // G
            bytes[o + 2] = 200u8.wrapping_sub(t / 3); // R
            bytes[o + 3] = 255; // A
        }
    }
}

/// Minimal shared mapping so the guest does not depend on the daemon crate.
struct Mmap {
    ptr: *mut u8,
    len: usize,
}

impl Mmap {
    fn new(fd: &OwnedFd, len: usize) -> std::io::Result<Mmap> {
        // SAFETY: mmap of a valid fd, PROT_READ|PROT_WRITE, MAP_SHARED.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Mmap { ptr: ptr as *mut u8, len })
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: mapping is valid for `len` bytes and we hold `&mut self`.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for Mmap {
    fn drop(&mut self) {
        // SAFETY: ptr/len came from a successful mmap we own.
        unsafe {
            libc::munmap(self.ptr as *mut libc::c_void, self.len);
        }
    }
}
