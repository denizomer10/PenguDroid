//! Bionic-compatible `libEGL` / `libGLESv2` running on the *host* GPU.
//!
//! Android apps (and their bundled native libraries) call the Bionic EGL/GLES
//! entry points directly. This crate implements the small subset they need by
//! `dlopen`ing the host's `libEGL.so.1` / `libGLESv2.so.2` at runtime — there
//! are no `-dev` headers involved — and by rendering into an offscreen FBO.
//!
//! Every [`eglSwapBuffers`] exports the FBO's texture as a dma-buf with
//! `EGL_MESA_image_dma_buf_export` and hands the fd to the PenguDroid daemon as
//! a [`Message::PresentDmabuf`] over `SCM_RIGHTS`. The daemon imports the
//! buffer into its Wayland surface, so the frame reaches the screen with no CPU
//! copy and no `wl_shm`.
//!
//! # Surfaces
//!
//! A Bionic app passes an `ANativeWindow*` to `eglCreateWindowSurface`, but
//! PenguDroid has no real native window: the geometry is supplied out of band
//! via `PENGUDROID_SURFACE_W` / `PENGUDROID_SURFACE_H`. The returned
//! `EGLSurface` is an opaque pointer to a [`SurfaceRec`] owned by this crate;
//! it is *not* a host EGL surface because a surfaceless display has none.
//!
//! # Global state and locking
//!
//! The host entry-point table is immutable after the first load ([`OnceLock`]).
//! Everything mutable (cached display, current surface, daemon connection and
//! surface id) lives under one [`Mutex`]. That mutex is never held across
//! arbitrary host calls; in `eglSwapBuffers` it is held for one daemon
//! request/response only (the socket has a short read timeout), so a missing or
//! wedged daemon can never hang the guest.

#![allow(non_snake_case)]
#![allow(clippy::missing_safety_doc)]

use std::ffi::{c_char, c_int, c_void, CString};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use pengudroid_proto::{default_socket_path, Message, Role, PROTO_VERSION};
use pengudroid_socket::Connection;

// ---------------------------------------------------------------------------
// EGL/GLES types and constants
// ---------------------------------------------------------------------------

pub type EGLDisplay = *mut c_void;
pub type EGLContext = *mut c_void;
pub type EGLConfig = *mut c_void;
pub type EGLSurface = *mut c_void;
type EGLImageKHR = *mut c_void;
type EGLClientBuffer = *mut c_void;

const EGL_TRUE: u32 = 1;
const EGL_FALSE: u32 = 0;
const EGL_NOT_INITIALIZED: i32 = 0x3001;

const EGL_PLATFORM_SURFACELESS_MESA: u32 = 0x31DD;
const EGL_GL_TEXTURE_2D_KHR: u32 = 0x30B1;

const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_RGBA: i32 = 0x1908;
const GL_RGBA8: i32 = 0x8058;
const GL_UNSIGNED_BYTE: u32 = 0x1401;
const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
const GL_LINEAR: i32 = 0x2601;
const GL_FRAMEBUFFER: u32 = 0x8D40;
const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;

// ---------------------------------------------------------------------------
// Host entry-point signatures
// ---------------------------------------------------------------------------

type EglGetProcAddress = unsafe extern "C" fn(*const c_char) -> *mut c_void;
type EglGetDisplay = unsafe extern "C" fn(*mut c_void) -> EGLDisplay;
type EglGetPlatformDisplayEXT =
    unsafe extern "C" fn(u32, *mut c_void, *const c_int) -> EGLDisplay;
type EglInitialize = unsafe extern "C" fn(EGLDisplay, *mut c_int, *mut c_int) -> u32;
type EglTerminate = unsafe extern "C" fn(EGLDisplay) -> u32;
type EglQueryString = unsafe extern "C" fn(EGLDisplay, c_int) -> *const c_char;
type EglGetError = unsafe extern "C" fn() -> c_int;
type EglChooseConfig =
    unsafe extern "C" fn(EGLDisplay, *const c_int, *mut EGLConfig, c_int, *mut c_int) -> u32;
type EglGetConfigAttrib = unsafe extern "C" fn(EGLDisplay, EGLConfig, c_int, *mut c_int) -> u32;
type EglCreateContext =
    unsafe extern "C" fn(EGLDisplay, EGLConfig, EGLContext, *const c_int) -> EGLContext;
type EglDestroyContext = unsafe extern "C" fn(EGLDisplay, EGLContext) -> u32;
type EglMakeCurrent =
    unsafe extern "C" fn(EGLDisplay, EGLSurface, EGLSurface, EGLContext) -> u32;
type EglGetCurrentContext = unsafe extern "C" fn() -> EGLContext;
type EglGetCurrentDisplay = unsafe extern "C" fn() -> EGLDisplay;
type EglBindAPI = unsafe extern "C" fn(u32) -> u32;
type EglSwapInterval = unsafe extern "C" fn(EGLDisplay, c_int) -> u32;
type EglCreateImageKHR =
    unsafe extern "C" fn(EGLDisplay, EGLContext, u32, EGLClientBuffer, *const c_int) -> EGLImageKHR;
type EglDestroyImageKHR = unsafe extern "C" fn(EGLDisplay, EGLImageKHR) -> u32;
type EglExportDMABUFImageQueryMESA =
    unsafe extern "C" fn(EGLDisplay, EGLImageKHR, *mut c_int, *mut c_int, *mut u64) -> u32;
type EglExportDMABUFImageMESA =
    unsafe extern "C" fn(EGLDisplay, EGLImageKHR, *mut c_int, *mut c_int, *mut c_int) -> u32;

type GlGenTextures = unsafe extern "C" fn(c_int, *mut u32);
type GlBindTexture = unsafe extern "C" fn(u32, u32);
type GlTexImage2D = unsafe extern "C" fn(
    u32,
    c_int,
    c_int,
    c_int,
    c_int,
    c_int,
    u32,
    u32,
    *const c_void,
);
type GlTexParameteri = unsafe extern "C" fn(u32, u32, c_int);
type GlGenFramebuffers = unsafe extern "C" fn(c_int, *mut u32);
type GlBindFramebuffer = unsafe extern "C" fn(u32, u32);
type GlFramebufferTexture2D = unsafe extern "C" fn(u32, u32, u32, u32, c_int);
type GlViewport = unsafe extern "C" fn(c_int, c_int, c_int, c_int);
type GlGetError = unsafe extern "C" fn() -> u32;
type GlGetString = unsafe extern "C" fn(u32) -> *const u8;
type GlClear = unsafe extern "C" fn(u32);
type GlClearColor = unsafe extern "C" fn(f32, f32, f32, f32);
type GlFinish = unsafe extern "C" fn();
type GlFlush = unsafe extern "C" fn();
type GlReadPixels = unsafe extern "C" fn(i32, i32, i32, i32, u32, u32, *mut c_void);

// ---------------------------------------------------------------------------
// Host table
// ---------------------------------------------------------------------------

struct Host {
    /// `libGLESv2` handle, kept for the `eglGetProcAddress` dlsym fallback.
    gles_handle: usize,

    get_proc_address: Option<EglGetProcAddress>,
    get_display: EglGetDisplay,
    get_platform_display_ext: Option<EglGetPlatformDisplayEXT>,
    initialize: EglInitialize,
    terminate: EglTerminate,
    query_string: EglQueryString,
    get_error: EglGetError,
    choose_config: EglChooseConfig,
    get_config_attrib: EglGetConfigAttrib,
    create_context: EglCreateContext,
    destroy_context: EglDestroyContext,
    make_current: EglMakeCurrent,
    get_current_context: Option<EglGetCurrentContext>,
    get_current_display: Option<EglGetCurrentDisplay>,
    bind_api: Option<EglBindAPI>,
    swap_interval: Option<EglSwapInterval>,
    create_image_khr: Option<EglCreateImageKHR>,
    destroy_image_khr: Option<EglDestroyImageKHR>,
    export_dmabuf_query: Option<EglExportDMABUFImageQueryMESA>,
    export_dmabuf_export: Option<EglExportDMABUFImageMESA>,

    gl_gen_textures: Option<GlGenTextures>,
    gl_bind_texture: Option<GlBindTexture>,
    gl_tex_image_2d: Option<GlTexImage2D>,
    gl_tex_parameter_i: Option<GlTexParameteri>,
    gl_gen_framebuffers: Option<GlGenFramebuffers>,
    gl_bind_framebuffer: Option<GlBindFramebuffer>,
    gl_framebuffer_texture_2d: Option<GlFramebufferTexture2D>,
    gl_viewport: Option<GlViewport>,
    gl_get_error: Option<GlGetError>,
    gl_get_string: Option<GlGetString>,
    gl_clear: Option<GlClear>,
    gl_clear_color: Option<GlClearColor>,
    gl_finish: Option<GlFinish>,
    gl_flush: Option<GlFlush>,
    gl_read_pixels: Option<GlReadPixels>,
}

impl Host {
    fn load() -> Result<Host, String> {
        let egl_handle = open_first(&["libEGL.so.1", "libEGL.so", "libEGL_mesa.so.0"])
            .ok_or_else(|| "could not dlopen libEGL.so.1".to_string())?;
        let gles_handle = open_first(&["libGLESv2.so.2", "libGLESv2.so"])
            .ok_or_else(|| "could not dlopen libGLESv2.so.2".to_string())?;

        if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
            trace_mapped_gl();
        }

        // Core entry points are looked up in libEGL itself; the extension
        // entry points may only be published through `eglGetProcAddress`.
        let core = |name: &str| sym_ptr(egl_handle, name);

        let get_proc_address: Option<EglGetProcAddress> =
            core("eglGetProcAddress").and_then(cast_fn);

        let proc_or_sym = |name: &str| -> Option<*mut c_void> {
            if let Some(get_proc_address) = get_proc_address {
                if let Ok(c_name) = CString::new(name) {
                    // SAFETY: `get_proc_address` is the host EGL entry point and
                    // `c_name` is a valid NUL-terminated string.
                    let ptr = unsafe { get_proc_address(c_name.as_ptr()) };
                    if !ptr.is_null() {
                        return Some(ptr);
                    }
                }
            }
            sym_ptr(egl_handle, name)
        };

        let gl_proc = |name: &str| -> Option<*mut c_void> {
            if let Some(get_proc_address) = get_proc_address {
                if let Ok(c_name) = CString::new(name) {
                    // SAFETY: as above.
                    let ptr = unsafe { get_proc_address(c_name.as_ptr()) };
                    if !ptr.is_null() {
                        return Some(ptr);
                    }
                }
            }
            sym_ptr(gles_handle, name).or_else(|| sym_ptr(egl_handle, name))
        };

        Ok(Host {
            gles_handle: gles_handle as usize,
            get_proc_address,
            get_display: need(core("eglGetDisplay"), "eglGetDisplay")?,
            get_platform_display_ext: proc_or_sym("eglGetPlatformDisplayEXT")
                .and_then(cast_fn),
            initialize: need(core("eglInitialize"), "eglInitialize")?,
            terminate: need(core("eglTerminate"), "eglTerminate")?,
            query_string: need(core("eglQueryString"), "eglQueryString")?,
            get_error: need(core("eglGetError"), "eglGetError")?,
            choose_config: need(core("eglChooseConfig"), "eglChooseConfig")?,
            get_config_attrib: need(core("eglGetConfigAttrib"), "eglGetConfigAttrib")?,
            create_context: need(core("eglCreateContext"), "eglCreateContext")?,
            destroy_context: need(core("eglDestroyContext"), "eglDestroyContext")?,
            make_current: need(core("eglMakeCurrent"), "eglMakeCurrent")?,
            get_current_context: core("eglGetCurrentContext").and_then(cast_fn),
            get_current_display: core("eglGetCurrentDisplay").and_then(cast_fn),
            bind_api: core("eglBindAPI").and_then(cast_fn),
            swap_interval: core("eglSwapInterval").and_then(cast_fn),
            create_image_khr: proc_or_sym("eglCreateImageKHR").and_then(cast_fn),
            destroy_image_khr: proc_or_sym("eglDestroyImageKHR").and_then(cast_fn),
            export_dmabuf_query: proc_or_sym("eglExportDMABUFImageQueryMESA")
                .and_then(cast_fn),
            export_dmabuf_export: proc_or_sym("eglExportDMABUFImageMESA")
                .and_then(cast_fn),

            gl_gen_textures: gl_proc("glGenTextures").and_then(cast_fn),
            gl_bind_texture: gl_proc("glBindTexture").and_then(cast_fn),
            gl_tex_image_2d: gl_proc("glTexImage2D").and_then(cast_fn),
            gl_tex_parameter_i: gl_proc("glTexParameteri").and_then(cast_fn),
            gl_gen_framebuffers: gl_proc("glGenFramebuffers").and_then(cast_fn),
            gl_bind_framebuffer: gl_proc("glBindFramebuffer").and_then(cast_fn),
            gl_framebuffer_texture_2d: gl_proc("glFramebufferTexture2D")
                .and_then(cast_fn),
            gl_viewport: gl_proc("glViewport").and_then(cast_fn),
            gl_get_error: gl_proc("glGetError").and_then(cast_fn),
            gl_get_string: gl_proc("glGetString").and_then(cast_fn),
            gl_clear: gl_proc("glClear").and_then(cast_fn),
            gl_clear_color: gl_proc("glClearColor").and_then(cast_fn),
            gl_finish: gl_proc("glFinish").and_then(cast_fn),
            gl_flush: gl_proc("glFlush").and_then(cast_fn),
            gl_read_pixels: gl_proc("glReadPixels").and_then(cast_fn),
        })
    }
}

static HOST: OnceLock<Result<Host, String>> = OnceLock::new();

/// Load the host driver at process start.
///
/// An app library is `dlopen`ed with `RTLD_NOW`, so every GLES symbol it imports
/// must already be resolvable. Loading the host driver here (before any app
/// code runs) makes its symbols global in time; doing it lazily on the first EGL
/// call would be too late.
#[used]
#[link_section = ".init_array"]
static PENGUDROID_GLESHIM_CTOR: extern "C" fn() = gleshim_init;

extern "C" fn gleshim_init() {
    let _ = host();
}

fn host() -> Result<&'static Host, String> {
    HOST.get_or_init(Host::load).as_ref().map_err(Clone::clone)
}

fn open_first(names: &[&str]) -> Option<*mut c_void> {
    for name in names {
        if let Ok(c_name) = CString::new(*name) {
            // RTLD_GLOBAL: an app's native library imports the whole GLES2 API
            // (glGenBuffers, glDrawElements, ...), not just the handful this
            // shim wraps. Making the host driver's symbols global lets those
            // resolve directly; the shim's own exports still interpose first
            // because it is preloaded.
            // SAFETY: `c_name` is a valid NUL-terminated soname.
            let handle =
                unsafe { libc::dlopen(c_name.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
            if !handle.is_null() {
                return Some(handle);
            }
        }
    }
    None
}

/// Print every mapped `libEGL`/`libGLESv2` image, for diagnostics.
///
/// `PENGUDROID_TRACE_GLESHIM=1` runs call this once. It answers the question the
/// symbol-level debugging cannot: *which* `libEGL` the process actually mapped,
/// the host's or the Bionic tree's copy that `nativeloader` also pulls in.
fn trace_mapped_gl() {
    let Ok(maps) = std::fs::read_to_string("/proc/self/maps") else {
        return;
    };
    let mut seen = std::collections::BTreeSet::new();
    for line in maps.lines() {
        let Some(path) = line.split_whitespace().last() else { continue };
        if !(path.contains("libEGL") || path.contains("libGLES")) {
            continue;
        }
        if seen.insert(path.to_string()) {
            eprintln!("[gleshim] mapped {path}");
        }
    }
}

fn sym_ptr(handle: *mut c_void, name: &str) -> Option<*mut c_void> {
    let c_name = CString::new(name).ok()?;
    // SAFETY: `handle` came from `dlopen` and `c_name` is a valid C string.
    let ptr = unsafe { libc::dlsym(handle, c_name.as_ptr()) };
    if ptr.is_null() {
        None
    } else {
        Some(ptr)
    }
}

fn need<T: Copy>(ptr: Option<*mut c_void>, name: &str) -> Result<T, String> {
    ptr.and_then(cast_fn)
        .ok_or_else(|| format!("host symbol `{name}` is missing"))
}

/// Reinterpret a `dlsym`/`eglGetProcAddress` result as a function pointer.
///
/// The caller only instantiates `T` with an `unsafe extern "C" fn` type that
/// matches the symbol the pointer was looked up under. POSIX guarantees the
/// representation of `void *` matches a function pointer on Linux.
fn cast_fn<T: Copy>(ptr: *mut c_void) -> Option<T> {
    if ptr.is_null() {
        return None;
    }
    debug_assert_eq!(
        std::mem::size_of::<T>(),
        std::mem::size_of::<*mut c_void>()
    );
    // SAFETY: see the function comment.
    Some(unsafe { std::mem::transmute_copy(&ptr) })
}

// ---------------------------------------------------------------------------
// Mutable global state
// ---------------------------------------------------------------------------

#[derive(Default)]
struct State {
    /// Host `EGLDisplay`, stored as `usize` so `State` stays `Send`.
    display: usize,
    /// Host `EGLContext` that the app most recently made current.
    context: usize,
    /// Our `EGLSurface` (`*mut SurfaceRec`) currently made current.
    current_surface: usize,

    conn: Option<Connection>,
    surface_id: Option<u32>,
    /// Whether we already tried (and possibly failed) to reach the daemon.
    connect_attempted: bool,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State::default()))
}

fn lock_state() -> MutexGuard<'static, State> {
    // A panic in another FFI call poisons the mutex; recover so a guest never
    // sees a hard failure because of it.
    state().lock().unwrap_or_else(|e| e.into_inner())
}

static LOGGED: AtomicBool = AtomicBool::new(false);

/// Emit the first diagnostic only. Bytecode loaders call EGL from many threads
/// and a missing daemon would otherwise flood the guest's stderr.
fn log_once(args: std::fmt::Arguments<'_>) {
    if !LOGGED.swap(true, Ordering::Relaxed) {
        eprintln!("[pengudroid-gleshim] {args}");
    }
}

// ---------------------------------------------------------------------------
// Surfaces and FBOs
// ---------------------------------------------------------------------------

/// The object behind every `EGLSurface` this crate hands out.
struct SurfaceRec {
    w: u32,
    h: u32,
    tex: u32,
    fbo: u32,
    /// The daemon's surface id for this window, or 0 until the first present.
    ///
    /// It lives here rather than on the shared `State` because the framework
    /// gives each window its own EGL surface: an activity and a dialog on the
    /// same render thread. A single process-wide id made both windows present
    /// into one Wayland surface, so they overwrote each other and only whichever
    /// drew last was visible.
    surface_id: u32,
}

/// Substitute the cached host display for `EGL_NO_DISPLAY`.
///
/// hwui's `EglManager` keeps its own `mEglDisplay`, but a few call sites — the
/// `SkiaOpenGLPipeline` cleanup paths in particular — pass `EGL_NO_DISPLAY` or
/// what `eglGetCurrentDisplay` returned after a context teardown. Forwarding
/// that to the host EGL produces `EGL_BAD_DISPLAY` from Mesa's
/// `validate_display`, which makes every subsequent swap fail.
fn resolve_display(dpy: EGLDisplay) -> EGLDisplay {
    if dpy.is_null() {
        let cached = lock_state().display;
        cached as EGLDisplay
    } else {
        dpy
    }
}

fn env_dim(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(default)
}

/// Lazily create the colour texture + FBO for `surface`. The host context must
/// be current on this thread.
unsafe fn ensure_fbo(surface: EGLSurface, host: &Host) -> Result<(), String> {
    let rec = &mut *(surface as *mut SurfaceRec);
    if rec.tex != 0 && rec.fbo != 0 {
        return Ok(());
    }

    let gen_textures = host
        .gl_gen_textures
        .ok_or_else(|| "host glGenTextures is unavailable".to_string())?;
    let bind_texture = host
        .gl_bind_texture
        .ok_or_else(|| "host glBindTexture is unavailable".to_string())?;
    let tex_image_2d = host
        .gl_tex_image_2d
        .ok_or_else(|| "host glTexImage2D is unavailable".to_string())?;
    let tex_parameter_i = host
        .gl_tex_parameter_i
        .ok_or_else(|| "host glTexParameteri is unavailable".to_string())?;
    let gen_framebuffers = host
        .gl_gen_framebuffers
        .ok_or_else(|| "host glGenFramebuffers is unavailable".to_string())?;
    let bind_framebuffer = host
        .gl_bind_framebuffer
        .ok_or_else(|| "host glBindFramebuffer is unavailable".to_string())?;
    let framebuffer_texture_2d = host
        .gl_framebuffer_texture_2d
        .ok_or_else(|| "host glFramebufferTexture2D is unavailable".to_string())?;

    let mut tex = 0u32;
    gen_textures(1, &mut tex);
    if tex == 0 {
        return Err("glGenTextures returned 0".into());
    }
    bind_texture(GL_TEXTURE_2D, tex);
    tex_image_2d(
        GL_TEXTURE_2D,
        0,
        GL_RGBA8,
        rec.w as c_int,
        rec.h as c_int,
        0,
        GL_RGBA as u32,
        GL_UNSIGNED_BYTE,
        std::ptr::null(),
    );
    tex_parameter_i(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
    tex_parameter_i(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);

    let mut fbo = 0u32;
    gen_framebuffers(1, &mut fbo);
    if fbo == 0 {
        return Err("glGenFramebuffers returned 0".into());
    }
    bind_framebuffer(GL_FRAMEBUFFER, fbo);
    framebuffer_texture_2d(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);

    rec.tex = tex;
    rec.fbo = fbo;
    Ok(())
}

/// Bind the surface's FBO and set the viewport, creating it first if needed.
unsafe fn bind_surface_fbo(surface: EGLSurface, host: &Host) -> Result<(), String> {
    ensure_fbo(surface, host)?;
    let rec = &*(surface as *const SurfaceRec);
    if let Some(bind_framebuffer) = host.gl_bind_framebuffer {
        bind_framebuffer(GL_FRAMEBUFFER, rec.fbo);
    }
    if let Some(viewport) = host.gl_viewport {
        viewport(0, 0, rec.w as c_int, rec.h as c_int);
    }
    Ok(())
}

/// Make sure a host context is current on this thread before any GL/export.
unsafe fn ensure_host_context(host: &Host) -> Result<(), String> {
    if let Some(get_current_context) = host.get_current_context {
        if !get_current_context().is_null() {
            return Ok(());
        }
        let (display, context) = {
            let st = lock_state();
            (st.display, st.context)
        };
        if display != 0 && context != 0 {
            let ok = (host.make_current)(
                display as EGLDisplay,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                context as EGLContext,
            );
            if ok == EGL_TRUE {
                return Ok(());
            }
        }
        Err("no current EGL context".into())
    } else {
        // We cannot query the driver; assume the app made the context current.
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// dma-buf export
// ---------------------------------------------------------------------------

/// Plane 0 of an exported EGLImage. The caller owns `fd`.
#[derive(Debug, Clone, Copy)]
pub struct DmabufExport {
    pub fd: RawFd,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub offset: u32,
}

/// Export the texture of `surface` as a dma-buf.
///
/// The host context must be current. The internal EGL context value is passed
/// to `eglCreateImageKHR` — Mesa's Iris driver dereferences it and segfaults if
/// `EGL_NO_CONTEXT` is used.
pub fn export_surface_dmabuf(surface: EGLSurface) -> Result<DmabufExport, String> {
    if surface.is_null() {
        return Err("export: null EGLSurface".into());
    }
    let host = host()?;

    let create_image = host
        .create_image_khr
        .ok_or_else(|| "EGL_KHR_image_base (eglCreateImageKHR) is unavailable".to_string())?;
    let destroy_image = host
        .destroy_image_khr
        .ok_or_else(|| "eglDestroyImageKHR is unavailable".to_string())?;
    let query_image = host
        .export_dmabuf_query
        .ok_or_else(|| "EGL_MESA_image_dma_buf_export is unavailable".to_string())?;
    let export_image = host
        .export_dmabuf_export
        .ok_or_else(|| "EGL_MESA_image_dma_buf_export is unavailable".to_string())?;

    unsafe {
        // Direct callers of this helper may not have made the context current
        // even though they set one up earlier.
        ensure_host_context(host)?;
        ensure_fbo(surface, host)?;
    }
    let rec = unsafe { &*(surface as *const SurfaceRec) };
    if rec.tex == 0 {
        return Err("export: surface has no texture".into());
    }

    // The display and current context. Never pass `EGL_NO_CONTEXT`.
    let (cached_display, cached_context) = {
        let st = lock_state();
        (st.display, st.context)
    };
    let current_context = host
        .get_current_context
        .map(|f| unsafe { f() })
        .unwrap_or(std::ptr::null_mut());
    let context = if current_context.is_null() {
        if cached_context == 0 {
            return Err("export: no current EGLContext".into());
        }
        cached_context as EGLContext
    } else {
        current_context
    };
    // Prefer the display this process actually initialized: every image this
    // module creates is created against that one, and Mesa's dma-buf entry
    // points validate the display before anything else.
    let display = if cached_display != 0 {
        cached_display as EGLDisplay
    } else {
        host.get_current_display
            .map(|f| unsafe { f() })
            .unwrap_or(std::ptr::null_mut())
    };
    if display.is_null() {
        return Err("export: display is not initialized".into());
    }

    // SAFETY: the texture belongs to the current context and `image` is
    // destroyed below on every path.
    let image = unsafe {
        create_image(
            display,
            context,
            EGL_GL_TEXTURE_2D_KHR,
            rec.tex as usize as EGLClientBuffer,
            std::ptr::null(),
        )
    };
    if image.is_null() {
        return Err(format!("eglCreateImageKHR failed: {}", host_error(host)));
    }

    let result = export_plane0(
        host,
        display,
        image,
        query_image,
        export_image,
        rec.w,
        rec.h,
    );
    // SAFETY: `image` was created above and is not used afterwards.
    unsafe { destroy_image(display, image) };
    result
}

fn export_plane0(
    host: &Host,
    display: EGLDisplay,
    image: EGLImageKHR,
    query_image: EglExportDMABUFImageQueryMESA,
    export_image: EglExportDMABUFImageMESA,
    width: u32,
    height: u32,
) -> Result<DmabufExport, String> {
    let mut fourcc: c_int = 0;
    let mut num_planes: c_int = 0;
    let mut modifiers = [0u64; 4];

    // SAFETY: `image` is live, the out-pointers are valid, and `modifiers` has
    // room for the number of planes the extension reports.
    let ok = unsafe { query_image(display, image, &mut fourcc, &mut num_planes, modifiers.as_mut_ptr()) };
    if ok != EGL_TRUE {
        return Err(format!(
            "eglExportDMABUFImageQueryMESA failed: {}",
            host_error(host)
        ));
    }
    if num_planes < 1 {
        return Err("dmabuf export reported no planes".into());
    }
    let planes = (num_planes as usize).min(modifiers.len());

    let mut fds = [-1i32; 4];
    let mut strides = [0i32; 4];
    let mut offsets = [0i32; 4];
    // SAFETY: three arrays of at least `num_planes` `int`s, as required.
    let ok = unsafe {
        export_image(
            display,
            image,
            fds.as_mut_ptr(),
            strides.as_mut_ptr(),
            offsets.as_mut_ptr(),
        )
    };
    if ok != EGL_TRUE {
        close_exported_fds(&fds[..planes]);
        return Err(format!(
            "eglExportDMABUFImageMESA failed: {}",
            host_error(host)
        ));
    }
    if fds[0] < 0 {
        close_exported_fds(&fds[..planes]);
        return Err("dmabuf export returned an invalid fd".into());
    }

    // Only plane 0 is presented; close any additional planes (avoiding dupes).
    for i in 1..planes {
        if fds[i] >= 0 && !fds[..i].contains(&fds[i]) {
            // SAFETY: this fd was just created by the export extension.
            unsafe { libc::close(fds[i]) };
        }
    }

    Ok(DmabufExport {
        fd: fds[0],
        width,
        height,
        stride: strides[0] as u32,
        fourcc: fourcc as u32,
        modifier: modifiers[0],
        offset: offsets[0] as u32,
    })
}

fn close_exported_fds(fds: &[RawFd]) {
    for &fd in fds {
        if fd >= 0 {
            // SAFETY: these fds are owned by us and not closed elsewhere.
            unsafe { libc::close(fd) };
        }
    }
}

fn host_error(host: &Host) -> String {
    // SAFETY: plain entry point with no arguments.
    let code = unsafe { (host.get_error)() };
    format!("EGL error 0x{code:04X}")
}

// ---------------------------------------------------------------------------
// Daemon presentation
// ---------------------------------------------------------------------------

fn close_fds(fds: &[RawFd]) {
    for &fd in fds {
        if fd >= 0 {
            // SAFETY: the socket layer transferred ownership of these fds.
            unsafe { libc::close(fd) };
        }
    }
}

/// Connect + handshake + resolve/create the surface id, lazily and once.
fn ensure_surface_id(st: &mut State, rec: &mut SurfaceRec) -> Result<u32, String> {
    if rec.surface_id != 0 {
        return Ok(rec.surface_id);
    }

    if st.conn.is_none() {
        if st.connect_attempted {
            return Err("daemon is unavailable".into());
        }
        st.connect_attempted = true;

        let conn = Connection::connect(&default_socket_path())
            .map_err(|e| format!("cannot connect to the daemon: {e}"))?;
        // Never block a guest indefinitely behind a wedged daemon.
        let _ = conn.set_read_timeout(Some(Duration::from_millis(500)));

        let hello = Message::Hello {
            version: PROTO_VERSION,
            role: Role::Guest,
            pid: std::process::id() as i32,
        };
        conn.send_message(&hello, &[])
            .map_err(|e| format!("Hello failed: {e}"))?;
        let (reply, fds) = conn
            .recv_message()
            .map_err(|e| format!("Welcome failed: {e}"))?;
        close_fds(&fds);
        match reply {
            Message::Welcome { .. } => {}
            other => return Err(format!("unexpected handshake reply: {other:?}")),
        }
        st.conn = Some(conn);
    }

    if let Ok(value) = std::env::var("PENGUDROID_SURFACE_ID") {
        if let Ok(id) = value.trim().parse::<u32>() {
            rec.surface_id = id;
            return Ok(id);
        }
    }

    let (w, h) = (rec.w, rec.h);
    let request = Message::CreateSurface {
        width: w,
        height: h,
        title: "PenguDroid EGL".into(),
    };
    let send_result = match st.conn.as_ref() {
        Some(conn) => conn.send_message(&request, &[]),
        None => return Err("daemon is unavailable".into()),
    };
    if let Err(e) = send_result {
        st.conn = None;
        return Err(format!("CreateSurface failed: {e}"));
    }

    let recv_result = match st.conn.as_ref() {
        Some(conn) => conn.recv_message(),
        None => return Err("daemon is unavailable".into()),
    };
    let (reply, fds) = match recv_result {
        Ok(v) => v,
        Err(e) => {
            st.conn = None;
            return Err(format!("SurfaceCreated failed: {e}"));
        }
    };
    close_fds(&fds);
    match reply {
        Message::SurfaceCreated { surface_id, .. } => {
            rec.surface_id = surface_id;
            subscribe_input(surface_id);
            Ok(surface_id)
        }
        Message::Error { code, message } => Err(format!(
            "CreateSurface rejected ({code}): {message}"
        )),
        other => Err(format!("unexpected CreateSurface reply: {other:?}")),
    }
}

/// Input events forwarded by the daemon, waiting for the host to pick them up.
static INPUT_QUEUE: Mutex<Vec<(u32, pengudroid_proto::InputKind)>> = Mutex::new(Vec::new());

/// Subscribe to the compositor's input for `surface_id` on a second connection.
///
/// The present connection is strictly request/reply, so unsolicited input would
/// desynchronise it; a dedicated connection keeps the two streams apart. The
/// reader thread only ever appends to [`INPUT_QUEUE`].
fn subscribe_input(surface_id: u32) {
    std::thread::spawn(move || {
        let conn = match Connection::connect(&default_socket_path()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[pengudroid-gleshim] input: cannot connect: {e}");
                return;
            }
        };
        let hello = Message::Hello {
            version: PROTO_VERSION,
            role: Role::Guest,
            pid: std::process::id() as i32,
        };
        if conn.send_message(&hello, &[]).is_err() {
            return;
        }
        if conn.recv_message().is_err() {
            return;
        }
        if conn
            .send_message(&Message::SubscribeInput { surface_id }, &[])
            .is_err()
        {
            return;
        }
        if conn.recv_message().is_err() {
            return;
        }
        loop {
            match conn.recv_message() {
                Ok((Message::InputEvent { surface_id, event }, fds)) => {
                    close_fds(&fds);
                    let mut q = INPUT_QUEUE.lock().unwrap();
                    if q.len() < 256 {
                        q.push((surface_id, event));
                    }
                }
                Ok((_, fds)) => close_fds(&fds),
                Err(_) => return,
            }
        }
    });
}

/// Pop one queued input event into `out`.
///
/// Layout (all little-endian, 16 bytes): `[u32 surface_id][u32 kind]
/// [f32 a][f32 b]`, where `kind` is the protocol's `InputKind` tag and `a`/`b`
/// carry the coordinates (or the button/keycode and the pressed flag).
///
/// Returns 1 when an event was written, 0 when the queue is empty.
#[no_mangle]
pub unsafe extern "C" fn pengudroid_input_poll(out: *mut u8) -> i32 {
    if out.is_null() {
        return 0;
    }
    let event = {
        let mut q = INPUT_QUEUE.lock().unwrap();
        if q.is_empty() {
            return 0;
        }
        q.remove(0)
    };
    let (surface_id, kind) = event;
    let (tag, a, b) = match kind {
        pengudroid_proto::InputKind::PointerEnter { x, y } => (1u32, x, y),
        pengudroid_proto::InputKind::PointerLeave => (2, 0.0, 0.0),
        pengudroid_proto::InputKind::PointerMotion { x, y } => (3, x, y),
        pengudroid_proto::InputKind::PointerButton { button, pressed } => {
            (4, button as f32, if pressed { 1.0 } else { 0.0 })
        }
        pengudroid_proto::InputKind::PointerAxis { axis, value } => (5, axis as f32, value),
        pengudroid_proto::InputKind::Key { keycode, pressed } => {
            (6, keycode as f32, if pressed { 1.0 } else { 0.0 })
        }
    };
    let buf = std::slice::from_raw_parts_mut(out, 16);
    buf[0..4].copy_from_slice(&surface_id.to_le_bytes());
    buf[4..8].copy_from_slice(&tag.to_le_bytes());
    buf[8..12].copy_from_slice(&a.to_le_bytes());
    buf[12..16].copy_from_slice(&b.to_le_bytes());
    1
}

/// Send one `PresentDmabuf` and wait for exactly one reply.
///
/// The connection is taken out of the global state for the duration, so even a
/// blocked `recv` does not leave the mutex in a state that other EGL calls
/// would deadlock on; the mutex is held only for this single request/response.
unsafe fn present_dmabuf(
    surface: EGLSurface,
    width: u32,
    height: u32,
    stride: u32,
    fourcc: u32,
    modifier: u64,
    offset: u32,
    fd: RawFd,
) -> Result<(), String> {
    let mut st = lock_state();
    // SAFETY: every non-null surface is a `SurfaceRec` this module allocated.
    let rec = &mut *(surface as *mut SurfaceRec);
    let surface_id = ensure_surface_id(&mut st, rec)?;

    let conn = st
        .conn
        .take()
        .ok_or_else(|| "daemon is unavailable".to_string())?;

    let mut io_failure = false;
    let result = (|| -> Result<(), String> {
        let msg = Message::PresentDmabuf {
            surface_id,
            width,
            height,
            stride,
            fourcc,
            modifier,
            offset,
        };
        conn.send_message(&msg, &[fd]).map_err(|e| {
            io_failure = true;
            format!("PresentDmabuf send failed: {e}")
        })?;
        let (reply, fds) = conn.recv_message().map_err(|e| {
            io_failure = true;
            format!("PresentDmabuf reply failed: {e}")
        })?;
        close_fds(&fds);
        match reply {
            Message::Ack => Ok(()),
            Message::Error { code, message } => {
                Err(format!("present rejected ({code}): {message}"))
            }
            other => Err(format!("unexpected present reply: {other:?}")),
        }
    })();

    if io_failure {
        // Dead socket: allow a later swap to reconnect.
        st.conn = None;
        st.connect_attempted = false;
    } else {
        st.conn = Some(conn);
    }
    if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
        match &result {
            Ok(()) => eprintln!(
                "[gleshim] presented surface {surface_id} {width}x{height}"
            ),
            Err(e) => eprintln!("[gleshim] present failed: {e}"),
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Exported EGL API
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn eglGetDisplay(native_display: *mut c_void) -> EGLDisplay {
    let host = match host() {
        Ok(host) => host,
        Err(e) => {
            log_once(format_args!("{e}"));
            return std::ptr::null_mut();
        }
    };

    let mut display = std::ptr::null_mut();
    if native_display.is_null() {
        // One surfaceless display for the whole process. The framework creates an
        // `EglManager` per window (the activity's, then a dialog's), and each one
        // calls `eglGetDisplay`, so without this cache the second window opened a
        // second display and context — two render paths over one FBO-backed
        // surface set. The host returns the same object for the surfaceless
        // platform anyway; skipping the call keeps the shim's own state single.
        let cached = lock_state().display as EGLDisplay;
        if !cached.is_null() {
            return cached;
        }
        if let Some(get_platform_display) = host.get_platform_display_ext {
            // SAFETY: the surfaceless platform ignores both arguments.
            display = get_platform_display(
                EGL_PLATFORM_SURFACELESS_MESA,
                std::ptr::null_mut(),
                std::ptr::null(),
            );
        }
    }
    if display.is_null() {
        // SAFETY: `EGL_DEFAULT_DISPLAY` is the null native display.
        display = (host.get_display)(native_display);
    }

    if !display.is_null() {
        let mut st = lock_state();
        st.display = display as usize;
    }
    display
}

#[no_mangle]
pub unsafe extern "C" fn eglInitialize(
    dpy: EGLDisplay,
    major: *mut c_int,
    minor: *mut c_int,
) -> u32 {
    let host = match host() {
        Ok(host) => host,
        Err(e) => {
            log_once(format_args!("{e}"));
            return EGL_FALSE;
        }
    };
    let dpy = if dpy.is_null() {
        lock_state().display as EGLDisplay
    } else {
        dpy
    };
    if dpy.is_null() {
        return EGL_FALSE;
    }
    // SAFETY: `dpy` is a display handle from the host and the out-pointers are
    // the caller's (valid or null, which EGL accepts).
    let ok = (host.initialize)(dpy, major, minor);
    if ok == EGL_TRUE {
        lock_state().display = dpy as usize;
    }
    if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
        eprintln!(
            "[pengudroid-gleshim] eglInitialize dpy={dpy:p} -> {ok}"
        );
    }
    ok
}

#[no_mangle]
pub unsafe extern "C" fn eglTerminate(dpy: EGLDisplay) -> u32 {
    // Deliberately *not* forwarded.
    //
    // There is exactly one host display for the whole process — the surfaceless
    // platform `eglGetDisplay` hands out — and every `EglManager` in the app
    // shares it, because the framework creates one per window (the activity's,
    // then the dialog's). Forwarding `eglTerminate` let whichever teardown ran
    // first invalidate the display for everybody, and from then on every host
    // call logged
    //
    //     libEGL: validate_display:567 error 3008 (EGL_BAD_DISPLAY)
    //
    // about once per frame while the surviving renderer kept using it. The
    // display is a process-lifetime resource here, so terminating it is a no-op
    // that reports success.
    let _ = dpy;
    EGL_TRUE
}

#[no_mangle]
pub unsafe extern "C" fn eglGetError() -> c_int {
    match host() {
        // SAFETY: plain entry point.
        Ok(host) => (host.get_error)(),
        Err(_) => EGL_NOT_INITIALIZED,
    }
}

#[no_mangle]
pub unsafe extern "C" fn eglQueryString(dpy: EGLDisplay, name: c_int) -> *const c_char {
    const EGL_EXTENSIONS: c_int = 0x3055;
    match host() {
        // SAFETY: forwarded verbatim.
        Ok(host) => {
            let raw = (host.query_string)(dpy, name);
            if name != EGL_EXTENSIONS || raw.is_null() {
                return raw;
            }
            // hwui hard-requires `EGL_KHR_swap_buffers_with_damage`
            // (`EglManager::initialize` aborts without it), and a surfaceless
            // Mesa display does not advertise it. The entry point below is a
            // real implementation — it forwards to `eglSwapBuffers` — so
            // advertising it is honest.
            static EXTENSIONS: OnceLock<CString> = OnceLock::new();
            let s = EXTENSIONS.get_or_init(|| {
                let base = std::ffi::CStr::from_ptr(raw).to_string_lossy().into_owned();
                if base.contains("EGL_KHR_swap_buffers_with_damage") {
                    CString::new(base).unwrap_or_default()
                } else {
                    CString::new(format!("{base} EGL_KHR_swap_buffers_with_damage"))
                        .unwrap_or_default()
                }
            });
            s.as_ptr()
        }
        Err(_) => std::ptr::null(),
    }
}

/// `eglSwapBuffersWithDamageKHR(dpy, surface, rects, n_rects)`.
///
/// The damage rectangles are a hint for the compositor; PenguDroid presents the
/// whole surface, so this is `eglSwapBuffers` with the arguments dropped.
#[no_mangle]
pub unsafe extern "C" fn eglSwapBuffersWithDamageKHR(
    dpy: EGLDisplay,
    surface: EGLSurface,
    _rects: *const c_int,
    _n_rects: c_int,
) -> u32 {
    eglSwapBuffers(dpy, surface)
}

/// `eglSetDamageRegionKHR(dpy, surface, rects, n_rects)`.
///
/// hwui calls this before every swap when partial updates are on and aborts
/// (`LOG_ALWAYS_FATAL`) when it fails. Our surfaces are offscreen FBOs, so the
/// damage region is only a hint; accepting it is enough.
#[no_mangle]
pub unsafe extern "C" fn eglSetDamageRegionKHR(
    _dpy: EGLDisplay,
    surface: EGLSurface,
    _rects: *const c_int,
    _n_rects: c_int,
) -> u32 {
    if surface.is_null() {
        return EGL_FALSE;
    }
    EGL_TRUE
}

#[no_mangle]
pub unsafe extern "C" fn eglChooseConfig(
    dpy: EGLDisplay,
    attribs: *const c_int,
    configs: *mut EGLConfig,
    config_size: c_int,
    num_config: *mut c_int,
) -> u32 {
    // A Bionic app asks for `EGL_WINDOW_BIT` because it expects a real window.
    // PenguDroid's display is surfaceless (frames are FBOs exported as
    // dma-bufs), so the request is rewritten to `EGL_PBUFFER_BIT`; otherwise the
    // host reports "no matching config" and the app cannot create a context.
    const EGL_SURFACE_TYPE: c_int = 0x3033;
    const EGL_PBUFFER_BIT: c_int = 0x0001;
    const EGL_NONE: c_int = 0x3038;
    let mut rewritten: Vec<c_int> = Vec::new();
    if !attribs.is_null() {
        let mut i = 0isize;
        loop {
            let key = *attribs.offset(i);
            if key == EGL_NONE {
                rewritten.push(EGL_NONE);
                break;
            }
            let val = *attribs.offset(i + 1);
            if key == EGL_SURFACE_TYPE {
                rewritten.push(EGL_SURFACE_TYPE);
                rewritten.push(EGL_PBUFFER_BIT);
            } else {
                rewritten.push(key);
                rewritten.push(val);
            }
            i += 2;
        }
    }
    let attribs = if rewritten.is_empty() { attribs } else { rewritten.as_ptr() };
    match host() {
        // SAFETY: the attribute list is NUL-terminated and the out-pointers are
        // the caller's.
        Ok(host) => (host.choose_config)(dpy, attribs, configs, config_size, num_config),
        Err(_) => EGL_FALSE,
    }
}

#[no_mangle]
pub unsafe extern "C" fn eglGetConfigAttrib(
    dpy: EGLDisplay,
    config: EGLConfig,
    attribute: c_int,
    value: *mut c_int,
) -> u32 {
    match host() {
        // SAFETY: forwarded verbatim.
        Ok(host) => (host.get_config_attrib)(dpy, config, attribute, value),
        Err(_) => EGL_FALSE,
    }
}

#[no_mangle]
pub unsafe extern "C" fn eglCreateContext(
    dpy: EGLDisplay,
    config: EGLConfig,
    share_context: EGLContext,
    attribs: *const c_int,
) -> EGLContext {
    match host() {
        // SAFETY: forwarded verbatim.
        Ok(host) => (host.create_context)(dpy, config, share_context, attribs),
        Err(e) => {
            log_once(format_args!("{e}"));
            std::ptr::null_mut()
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn eglDestroyContext(dpy: EGLDisplay, ctx: EGLContext) -> u32 {
    let ok = match host() {
        // SAFETY: forwarded verbatim.
        Ok(host) => (host.destroy_context)(dpy, ctx),
        Err(_) => EGL_FALSE,
    };
    if ok == EGL_TRUE {
        let mut st = lock_state();
        if st.context == ctx as usize {
            st.context = 0;
        }
    }
    ok
}

#[no_mangle]
pub unsafe extern "C" fn eglMakeCurrent(
    dpy: EGLDisplay,
    draw: EGLSurface,
    read: EGLSurface,
    ctx: EGLContext,
) -> u32 {
    let host = match host() {
        Ok(host) => host,
        Err(e) => {
            log_once(format_args!("{e}"));
            return EGL_FALSE;
        }
    };

    // A surfaceless host display has no window surfaces; the app renders into
    // our FBO instead, so only the context is made current.
    if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
        eprintln!("[gleshim] eglMakeCurrent dpy={dpy:p} draw={draw:p} ctx={ctx:p}");
    }
    let dpy = resolve_display(dpy);
    let ok = (host.make_current)(
        dpy,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        ctx,
    );
    if ok != EGL_TRUE {
        let mut st = lock_state();
        st.current_surface = 0;
        return EGL_FALSE;
    }

    {
        let mut st = lock_state();
        if !dpy.is_null() {
            st.display = dpy as usize;
        }
        st.context = ctx as usize;
        st.current_surface = draw as usize;
    }

    // `read` is ignored: the FBO is both the draw and read target.
    let _ = read;

    if !draw.is_null() {
        // Binding has to happen after the context is current. `draw` is one of
        // our `SurfaceRec`s; `ensure_fbo`/`bind_surface_fbo` need no lock.
        if let Err(e) = bind_surface_fbo(draw, host) {
            log_once(format_args!("eglMakeCurrent: {e}"));
            return EGL_FALSE;
        }
    }
    EGL_TRUE
}

#[no_mangle]
pub unsafe extern "C" fn eglGetCurrentContext() -> EGLContext {
    if let Some(get_current_context) = host().ok().and_then(|h| h.get_current_context) {
        // SAFETY: plain entry point.
        return get_current_context();
    }
    lock_state().context as EGLContext
}

#[no_mangle]
pub unsafe extern "C" fn eglGetCurrentDisplay() -> EGLDisplay {
    if let Some(get_current_display) = host().ok().and_then(|h| h.get_current_display) {
        // SAFETY: plain entry point.
        let display = get_current_display();
        if !display.is_null() {
            return display;
        }
    }
    lock_state().display as EGLDisplay
}

#[no_mangle]
pub unsafe extern "C" fn eglGetCurrentSurface(_readdraw: i32) -> EGLSurface {
    lock_state().current_surface as EGLSurface
}

#[no_mangle]
pub unsafe extern "C" fn eglBindAPI(api: u32) -> u32 {
    match host().ok().and_then(|h| h.bind_api) {
        // SAFETY: forwarded verbatim.
        Some(bind_api) => bind_api(api),
        None => EGL_FALSE,
    }
}

#[no_mangle]
pub unsafe extern "C" fn eglSwapInterval(dpy: EGLDisplay, interval: c_int) -> u32 {
    match host().ok().and_then(|h| h.swap_interval) {
        // SAFETY: forwarded verbatim.
        Some(swap_interval) => swap_interval(resolve_display(dpy), interval),
        None => EGL_FALSE,
    }
}

#[no_mangle]
pub unsafe extern "C" fn eglGetProcAddress(name: *const c_char) -> *mut c_void {
    if name.is_null() {
        return std::ptr::null_mut();
    }
    // Answer for the entry points this shim implements itself. hwui resolves
    // `eglSetDamageRegionKHR`/`eglSwapBuffersWithDamageKHR` through here, and
    // the host's copies reject our offscreen surfaces with `EGL_BAD_DISPLAY`.
    let requested = std::ffi::CStr::from_ptr(name).to_bytes();
    let ours: &[(&[u8], *mut c_void)] = &[
        (b"eglSetDamageRegionKHR", eglSetDamageRegionKHR as *mut c_void),
        (b"eglSwapBuffersWithDamageKHR", eglSwapBuffersWithDamageKHR as *mut c_void),
        (b"eglSurfaceAttrib", eglSurfaceAttrib as *mut c_void),
        (b"eglQuerySurface", eglQuerySurface as *mut c_void),
        (b"eglCreateWindowSurface", eglCreateWindowSurface as *mut c_void),
        (b"eglDestroySurface", eglDestroySurface as *mut c_void),
        (b"eglSwapBuffers", eglSwapBuffers as *mut c_void),
        (b"glBindFramebuffer", glBindFramebuffer as *mut c_void),
    ];
    for (symbol, ptr) in ours {
        if requested == *symbol {
            return *ptr;
        }
    }
    let host = match host() {
        Ok(host) => host,
        Err(_) => return std::ptr::null_mut(),
    };
    if let Some(get_proc_address) = host.get_proc_address {
        // SAFETY: `name` is the caller's NUL-terminated symbol name.
        let ptr = get_proc_address(name);
        if !ptr.is_null() {
            return ptr;
        }
    }
    if host.gles_handle != 0 {
        // SAFETY: the handle came from dlopen and `name` is a valid C string.
        let ptr = libc::dlsym(host.gles_handle as *mut c_void, name);
        if !ptr.is_null() {
            return ptr;
        }
    }
    std::ptr::null_mut()
}

/// Ask an `ANativeWindow` for its buffer dimensions.
///
/// `ANativeWindow_getWidth`/`getHeight` live in Bionic's `libandroid.so`, which
/// the app has already loaded by the time hwui creates a surface. They are
/// resolved lazily so a process without `libandroid.so` still works.
unsafe fn window_size(window: *mut c_void) -> (u32, u32) {
    if window.is_null() {
        return (0, 0);
    }
    type GetDim = unsafe extern "C" fn(*mut c_void) -> i32;
    let mut resolve = |name: &[u8]| -> Option<GetDim> {
        let sym = libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr() as *const c_char);
        if sym.is_null() {
            None
        } else {
            Some(std::mem::transmute::<*mut c_void, GetDim>(sym))
        }
    };
    let w = resolve(b"ANativeWindow_getWidth\0").map(|f| f(window)).unwrap_or(0);
    let h = resolve(b"ANativeWindow_getHeight\0").map(|f| f(window)).unwrap_or(0);
    if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
        eprintln!("[gleshim] ANativeWindow reports {w}x{h}");
    }
    if w > 0 && h > 0 {
        (w as u32, h as u32)
    } else {
        (0, 0)
    }
}

#[no_mangle]
pub unsafe extern "C" fn eglCreateWindowSurface(
    _dpy: EGLDisplay,
    _config: EGLConfig,
    native_window: *mut c_void,
    _attribs: *const c_int,
) -> EGLSurface {
    if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
        eprintln!("[gleshim] eglCreateWindowSurface window={native_window:p}");
    }
    // Bionic hands us an `ANativeWindow*`. There is no host window, only the
    // FBO that `eglSwapBuffers` exports, but the window still knows the size the
    // framework laid the surface out at — and hwui renders at that size. Using
    // the window's dimensions keeps the FBO, the viewport and the presented
    // buffer in agreement; the env vars are only a fallback.
    let (mut w, mut h) = window_size(native_window);
    if w == 0 || h == 0 {
        w = env_dim("PENGUDROID_SURFACE_W", 720);
        h = env_dim("PENGUDROID_SURFACE_H", 1280);
    }
    if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
        eprintln!("[gleshim] eglCreateWindowSurface -> {w}x{h}");
    }
    let rec = Box::new(SurfaceRec {
        w,
        h,
        tex: 0,
        fbo: 0,
        surface_id: 0,
    });
    let surface = Box::into_raw(rec) as EGLSurface;
    lock_state().current_surface = surface as usize;
    surface
}

#[no_mangle]
pub unsafe extern "C" fn eglDestroySurface(_dpy: EGLDisplay, surface: EGLSurface) -> u32 {
    if surface.is_null() {
        return EGL_FALSE;
    }
    {
        let mut st = lock_state();
        if st.current_surface == surface as usize {
            st.current_surface = 0;
        }
    }
    // SAFETY: `surface` was produced by `Box::into_raw` in
    // `eglCreateWindowSurface` and is dropped exactly once here.
    drop(Box::from_raw(surface as *mut SurfaceRec));
    EGL_TRUE
}

#[no_mangle]
pub unsafe extern "C" fn eglCreatePbufferSurface(
    _dpy: EGLDisplay,
    _config: EGLConfig,
    attribs: *const c_int,
) -> EGLSurface {
    // A pbuffer is just another offscreen target for us: the same FBO-backed
    // surface, with geometry taken from EGL_WIDTH/EGL_HEIGHT if supplied.
    const EGL_WIDTH: c_int = 0x3057;
    const EGL_HEIGHT: c_int = 0x3056;
    let mut w = env_dim("PENGUDROID_SURFACE_W", 720);
    let mut h = env_dim("PENGUDROID_SURFACE_H", 1280);
    if !attribs.is_null() {
        let mut i = 0isize;
        loop {
            let key = *attribs.offset(i);
            if key == 0x3038 {
                break; // EGL_NONE
            }
            let val = *attribs.offset(i + 1);
            if key == EGL_WIDTH && val > 0 {
                w = val as u32;
            } else if key == EGL_HEIGHT && val > 0 {
                h = val as u32;
            }
            i += 2;
        }
    }
    let rec = Box::new(SurfaceRec { w, h, tex: 0, fbo: 0, surface_id: 0 });
    let surface = Box::into_raw(rec) as EGLSurface;
    lock_state().current_surface = surface as usize;
    surface
}

#[no_mangle]
pub unsafe extern "C" fn eglSurfaceAttrib(
    _dpy: EGLDisplay,
    surface: EGLSurface,
    attribute: c_int,
    value: c_int,
) -> u32 {
    // hwui sets `EGL_SWAP_BEHAVIOR` to `EGL_BUFFER_DESTROYED` on every surface it
    // creates and aborts when that fails. Our surfaces are offscreen FBOs, so the
    // attribute has no meaning; accepting it (and answering the query) is what
    // keeps the render thread alive.
    if surface.is_null() {
        return EGL_FALSE;
    }
    if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
        eprintln!("[gleshim] eglSurfaceAttrib surface={surface:p} attr=0x{attribute:x} value=0x{value:x}");
    }
    let _ = (attribute, value);
    EGL_TRUE
}

#[no_mangle]
pub unsafe extern "C" fn eglQuerySurface(
    dpy: EGLDisplay,
    surface: EGLSurface,
    attribute: c_int,
    value: *mut c_int,
) -> u32 {
    let _ = resolve_display(dpy);
    if surface.is_null() || value.is_null() {
        return EGL_FALSE;
    }
    // SAFETY: non-null surfaces are `SurfaceRec` pointers from the create calls.
    let rec = &*(surface as *const SurfaceRec);
    match attribute {
        0x3057 => {
            *value = rec.w as c_int; // EGL_WIDTH
            EGL_TRUE
        }
        0x3056 => {
            *value = rec.h as c_int; // EGL_HEIGHT
            EGL_TRUE
        }
        // EGL_SWAP_BEHAVIOR -> EGL_BUFFER_DESTROYED. hwui queries this right
        // after setting it; answering EGL_FALSE makes it log "Unable to match
        // the desired swap behavior" and fall back to allocating its own
        // GraphicBuffers, which needs a gralloc mapper we do not have.
        0x3093 => {
            *value = 0x3095; // EGL_BUFFER_DESTROYED
            EGL_TRUE
        }
        // EGL_BUFFER_AGE_EXT: a single-buffered offscreen FBO is always fresh.
        0x313D => {
            *value = 1;
            EGL_TRUE
        }
        _ => EGL_FALSE,
    }
}

// ---------------------------------------------------------------------------
// EGL sync objects and images
// ---------------------------------------------------------------------------
//
// `libhwui` imports these from `libEGL`, and none of them was exported by the
// gleshim — so Skia's fence handling called straight into Mesa with our synthetic
// surface and display handles. Mesa's first act in every entry point is
// `validate_display`, which is where the flood of
// "libEGL: validate_display:567 error 3008 (EGL_BAD_DISPLAY)" came from.
//
// A sync object here is a bare sentinel: the FBO is rendered synchronously and
// presentation copies the whole buffer, so there is never anything to wait for.

/// `EGL_CONDITION_SATISFIED_KHR`.
const EGL_CONDITION_SATISFIED_KHR: c_int = 0x30F6;

/// Distinct sentinels so a client that compares syncs sees different objects.
static SYNC_SENTINEL: u8 = 0;

/// `eglCreateSyncKHR(dpy, type, attrib_list)`.
#[no_mangle]
pub unsafe extern "C" fn eglCreateSyncKHR(
    _dpy: EGLDisplay,
    _type: u32,
    _attrib_list: *const c_int,
) -> *mut c_void {
    std::ptr::addr_of!(SYNC_SENTINEL) as *mut c_void
}

/// `eglDestroySyncKHR(dpy, sync)`.
#[no_mangle]
pub unsafe extern "C" fn eglDestroySyncKHR(_dpy: EGLDisplay, _sync: *mut c_void) -> u32 {
    EGL_TRUE
}

/// `eglClientWaitSyncKHR(dpy, sync, flags, timeout)` — always already signalled.
#[no_mangle]
pub unsafe extern "C" fn eglClientWaitSyncKHR(
    _dpy: EGLDisplay,
    _sync: *mut c_void,
    _flags: c_int,
    _timeout: u64,
) -> c_int {
    EGL_CONDITION_SATISFIED_KHR
}

/// `eglWaitSyncKHR(dpy, sync, flags)`.
#[no_mangle]
pub unsafe extern "C" fn eglWaitSyncKHR(
    _dpy: EGLDisplay,
    _sync: *mut c_void,
    _flags: c_int,
) -> u32 {
    EGL_TRUE
}

/// `eglGetSyncAttribKHR(dpy, sync, attribute, value)`.
#[no_mangle]
pub unsafe extern "C" fn eglGetSyncAttribKHR(
    _dpy: EGLDisplay,
    _sync: *mut c_void,
    attribute: c_int,
    value: *mut c_int,
) -> u32 {
    if !value.is_null() {
        // EGL_SYNC_STATUS_KHR -> EGL_SIGNALED_KHR, EGL_SYNC_TYPE_KHR -> EGL_SYNC_FENCE_KHR.
        *value = match attribute {
            0x30F1 => 0x30F2, // EGL_SYNC_STATUS_KHR -> EGL_SIGNALED_KHR
            0x30F0 => 0x30F9, // EGL_SYNC_TYPE_KHR -> EGL_SYNC_FENCE_KHR
            _ => 0,
        };
    }
    EGL_TRUE
}

/// `eglDupNativeFenceFDANDROID(dpy, sync)` — no fence, so `EGL_NO_NATIVE_FENCE_FD_ANDROID`.
#[no_mangle]
pub unsafe extern "C" fn eglDupNativeFenceFDANDROID(
    _dpy: EGLDisplay,
    _sync: *mut c_void,
) -> c_int {
    -1
}

/// `eglGetNativeClientBufferANDROID(buffer)` — `AHardwareBuffer` is already the
/// native client buffer, so this is the identity.
#[no_mangle]
pub unsafe extern "C" fn eglGetNativeClientBufferANDROID(buffer: *mut c_void) -> EGLClientBuffer {
    buffer
}

/// `eglCreateImageKHR(dpy, ctx, target, buffer, attrib_list)`.
#[no_mangle]
pub unsafe extern "C" fn eglCreateImageKHR(
    dpy: EGLDisplay,
    ctx: EGLContext,
    target: u32,
    buffer: EGLClientBuffer,
    attrib_list: *const c_int,
) -> EGLImageKHR {
    match host().ok().and_then(|h| h.create_image_khr) {
        // SAFETY: forwarded verbatim; the host owns the argument contract.
        Some(create) => create(resolve_display(dpy), ctx, target, buffer, attrib_list),
        None => std::ptr::null_mut(),
    }
}

/// `eglDestroyImageKHR(dpy, image)`.
#[no_mangle]
pub unsafe extern "C" fn eglDestroyImageKHR(dpy: EGLDisplay, image: EGLImageKHR) -> u32 {
    match host().ok().and_then(|h| h.destroy_image_khr) {
        // SAFETY: forwarded verbatim.
        Some(destroy) => destroy(resolve_display(dpy), image),
        None => EGL_FALSE,
    }
}

/// `eglPresentationTimeANDROID(dpy, surface, time)`.
///
/// Android's `libEGL` extension. `libandroid_runtime` imports it, and that was
/// the per-frame path that reached the Bionic `libEGL` instead of this shim.
/// A surface here is a synchronous offscreen FBO, so a presentation timestamp
/// has nothing to do; accepting it is the whole contract.
#[no_mangle]
pub unsafe extern "C" fn eglPresentationTimeANDROID(
    _dpy: EGLDisplay,
    surface: EGLSurface,
    _time: i64,
) -> u32 {
    if surface.is_null() {
        return EGL_FALSE;
    }
    EGL_TRUE
}

// ---------------------------------------------------------------------------
// Bionic libEGL internals
// ---------------------------------------------------------------------------
//
// The Bionic `libGLESv1_CM`/`libGLESv2`/`libGLESv3` are thin trampolines: their
// `gl*` entry points dispatch through the connection that `libEGL` sets up. Three
// of them plus one `libandroid_runtime` hook are the *only* symbols the rest of
// the tree takes from the Bionic `libEGL`, and leaving them unexported meant
// that library stayed in the call path — where its own `EGLDisplay` registry
// rejects this shim's surfaceless display:
//
//     libEGL: validate_display:567 error 3008 (EGL_BAD_DISPLAY)
//
// GL work never actually goes through those trampolines here: `LD_DEBUG=bindings`
// shows every `gl*` binding resolving to the host's `libGLESv2.so.2`. So they only
// have to link and be callable without crashing.

/// Opaque stand-in for the Bionic `egl_connection_t`.
///
/// Non-null so a trampoline that stores it does not fault immediately; its
/// dispatch table is empty on purpose, because the host's GL library is what
/// actually gets called.
static EGL_CONNECTION: [u8; 256] = [0; 256];

/// `android::egl_get_connection()`.
#[export_name = "_ZN7android18egl_get_connectionEv"]
pub unsafe extern "C" fn egl_get_connection() -> *mut c_void {
    std::ptr::addr_of!(EGL_CONNECTION) as *mut c_void
}

/// `android::egl_get_init_count(void*)` — `libandroid_runtime` asks whether EGL
/// has been initialized for a display.
#[export_name = "_ZN7android18egl_get_init_countEPv"]
pub unsafe extern "C" fn egl_get_init_count(_dpy: *mut c_void) -> u32 {
    if _dpy.is_null() {
        0
    } else {
        1
    }
}

/// `android::egl_get_string_for_current_context(unsigned int)` — the GLESv1
/// trampoline's `glGetString` for extension strings.
#[export_name = "_ZN7android34egl_get_string_for_current_contextEj"]
#[allow(non_snake_case)]
#[no_mangle]
pub unsafe extern "C" fn egl_get_string_for_current_context(_name: u32) -> *const c_char {
    std::ptr::null()
}

/// `eglBeginFrame(EGLDisplay, EGLSurface)`.
///
/// `libhwui` calls this once per frame when the driver advertises
/// `EGL_ANDROID_frame_timestamps`/debug-marking support, and it was the *only*
/// per-frame symbol still resolving to the Bionic `libEGL`. That copy validates
/// the display against its own registry first, which is where the
/// `validate_display:567 error 3008` flood came from. There is nothing to begin:
/// the surface is a synchronous offscreen FBO.
#[export_name = "_Z13eglBeginFramePvS_"]
pub unsafe extern "C" fn eglBeginFrame(_dpy: EGLDisplay, _surface: EGLSurface) {}

#[no_mangle]
pub unsafe extern "C" fn eglSwapBuffers(_dpy: EGLDisplay, surface: EGLSurface) -> u32 {
    if std::env::var_os("PENGUDROID_TRACE_GLESHIM").is_some() {
        static SWAPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SWAPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if n < 5 || n % 60 == 0 {
            eprintln!("[gleshim] eglSwapBuffers #{n} surface={surface:p}");
        }
    }
    if surface.is_null() {
        return EGL_FALSE;
    }
    let host = match host() {
        Ok(host) => host,
        Err(e) => {
            log_once(format_args!("{e}"));
            return EGL_FALSE;
        }
    };

    if let Err(e) = ensure_host_context(host) {
        log_once(format_args!("eglSwapBuffers: {e}"));
        return EGL_FALSE;
    }
    // Bind this surface's FBO: `ensure_fbo` alone leaves whatever the last
    // `eglMakeCurrent` bound, and two windows share one render thread.
    if let Err(e) = bind_surface_fbo(surface, host) {
        log_once(format_args!("eglSwapBuffers: {e}"));
        return EGL_FALSE;
    }
    if let Some(finish) = host.gl_finish {
        finish();
    }
    dump_frame_once(surface, host);

    let exported = match export_surface_dmabuf(surface) {
        Ok(exported) => exported,
        Err(e) => {
            log_once(format_args!("eglSwapBuffers: {e}"));
            return EGL_FALSE;
        }
    };
    let rec = &*(surface as *const SurfaceRec);

    // Take ownership so the fd is closed no matter how presentation ends; the
    // daemon receives its own descriptor via SCM_RIGHTS.
    // SAFETY: `exported.fd` is a freshly exported descriptor we own.
    let fd = OwnedFd::from_raw_fd(exported.fd);

    match present_dmabuf(
        surface,
        rec.w,
        rec.h,
        exported.stride,
        exported.fourcc,
        exported.modifier,
        exported.offset,
        fd.as_raw_fd(),
    ) {
        Ok(()) => EGL_TRUE,
        Err(e) => {
            log_once(format_args!("eglSwapBuffers: {e}"));
            EGL_FALSE
        }
    }
}

/// `PENGUDROID_DUMP_FRAME=/path/frame.ppm`: write the first few frames to disk.
///
/// The compositor cannot be screenshotted from a sandboxed session, so this is
/// how the render output is inspected: the FBO is read back with
/// `glReadPixels` and written as a binary PPM (no image crate needed).
unsafe fn dump_frame_once(surface: EGLSurface, host: &Host) {
    let Some(path) = std::env::var_os("PENGUDROID_DUMP_FRAME") else {
        return;
    };
    static DUMPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    // Skip the first frames: the first traversal draws the window background
    // before the app has laid anything out. `PENGUDROID_DUMP_FRAME_AT` overrides
    // the frame number, which matters for an app that only swaps a handful of
    // times (a static screen) and would never reach the default.
    let at: u64 = std::env::var("PENGUDROID_DUMP_FRAME_AT")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(200);
    let n = DUMPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if n != at {
        return;
    }
    let Some(read_pixels) = host.gl_read_pixels else {
        return;
    };
    let rec = &*(surface as *const SurfaceRec);
    let (w, h) = (rec.w as usize, rec.h as usize);
    let mut rgba = vec![0u8; w * h * 4];
    read_pixels(
        0,
        0,
        w as i32,
        h as i32,
        0x1908, // GL_RGBA
        0x1401, // GL_UNSIGNED_BYTE
        rgba.as_mut_ptr() as *mut c_void,
    );
    let mut ppm = Vec::with_capacity(w * h * 3 + 32);
    ppm.extend_from_slice(format!("P6\n{w} {h}\n255\n").as_bytes());
    // GL's origin is bottom-left; PPM's is top-left. `PENGUDROID_DUMP_FRAME_NOFLIP`
    // writes the rows in readback order instead, which is exactly what the
    // dma-buf handed to the compositor contains — so the two images together show
    // whether presentation needs a flip.
    let y_range: Vec<usize> = if std::env::var_os("PENGUDROID_DUMP_FRAME_NOFLIP").is_some() {
        (0..h).collect()
    } else {
        (0..h).rev().collect()
    };
    for y in y_range {
        for x in 0..w {
            let i = (y * w + x) * 4;
            ppm.extend_from_slice(&rgba[i..i + 3]);
        }
    }
    let path = std::path::PathBuf::from(path);
    // The surface id is part of the name because two windows render at once and
    // the frame counter is global.
    let path = path.with_extension(format!("s{}frame{n}.ppm", rec.surface_id));
    match std::fs::write(&path, &ppm) {
        Ok(()) => eprintln!("[gleshim] wrote {} ({w}x{h})", path.display()),
        Err(e) => eprintln!("[gleshim] cannot write {}: {e}", path.display()),
    }
}

#[no_mangle]
pub unsafe extern "C" fn eglReleaseThread() -> u32 {
    EGL_TRUE
}

// ---------------------------------------------------------------------------
// Exported GLESv2 API (a small forwarding surface)
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn glClear(mask: u32) {
    if let Some(f) = host().ok().and_then(|h| h.gl_clear) {
        // SAFETY: forwarded verbatim.
        f(mask);
    }
}

#[no_mangle]
pub unsafe extern "C" fn glClearColor(red: f32, green: f32, blue: f32, alpha: f32) {
    if let Some(f) = host().ok().and_then(|h| h.gl_clear_color) {
        // SAFETY: forwarded verbatim.
        f(red, green, blue, alpha);
    }
}

#[no_mangle]
pub unsafe extern "C" fn glViewport(x: c_int, y: c_int, width: c_int, height: c_int) {
    if let Some(f) = host().ok().and_then(|h| h.gl_viewport) {
        // SAFETY: forwarded verbatim.
        f(x, y, width, height);
    }
}

#[no_mangle]
pub unsafe extern "C" fn glFinish() {
    if let Some(f) = host().ok().and_then(|h| h.gl_finish) {
        // SAFETY: forwarded verbatim.
        f();
    }
}

#[no_mangle]
pub unsafe extern "C" fn glFlush() {
    if let Some(f) = host().ok().and_then(|h| h.gl_flush) {
        // SAFETY: forwarded verbatim.
        f();
    }
}

#[no_mangle]
pub unsafe extern "C" fn glGetError() -> u32 {
    match host().ok().and_then(|h| h.gl_get_error) {
        // SAFETY: forwarded verbatim.
        Some(f) => f(),
        None => 0,
    }
}

#[no_mangle]
pub unsafe extern "C" fn glGetString(name: u32) -> *const u8 {
    match host().ok().and_then(|h| h.gl_get_string) {
        // SAFETY: forwarded verbatim.
        Some(f) => f(name),
        None => std::ptr::null(),
    }
}

/// `glBindFramebuffer(target, framebuffer)`.
///
/// hwui's Skia backend renders into the *default* framebuffer (id 0) and binds
/// it explicitly before every flush. A surfaceless host context has no default
/// framebuffer, so binding 0 is redirected to the current surface's FBO — that
/// is what makes the app's pixels land somewhere we can export.
#[no_mangle]
pub unsafe extern "C" fn glBindFramebuffer(target: u32, framebuffer: u32) {
    let Ok(host) = host() else {
        return;
    };
    let Some(bind) = host.gl_bind_framebuffer else {
        return;
    };
    if framebuffer == 0 {
        let surface = {
            let st = lock_state();
            st.current_surface as EGLSurface
        };
        if !surface.is_null() && ensure_fbo(surface, host).is_ok() {
            let rec = &*(surface as *const SurfaceRec);
            bind(target, rec.fbo);
            return;
        }
    }
    bind(target, framebuffer);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Resolve a GL entry point through the shim's own `eglGetProcAddress`.
    unsafe fn gl_proc<T: Copy>(name: &str) -> Option<T> {
        let c_name = CString::new(name).ok()?;
        let ptr = eglGetProcAddress(c_name.as_ptr());
        if ptr.is_null() {
            return None;
        }
        Some(std::mem::transmute_copy(&ptr))
    }

    #[test]
    fn host_egl_loads() {
        match host() {
            Ok(host) => {
                assert!(
                    host.get_proc_address.is_some(),
                    "host eglGetProcAddress did not resolve"
                );
                assert!(
                    host.create_image_khr.is_some(),
                    "host eglCreateImageKHR did not resolve"
                );
                assert!(
                    host.export_dmabuf_query.is_some(),
                    "host eglExportDMABUFImageQueryMESA did not resolve"
                );
                assert!(host.gl_clear.is_some(), "host glClear did not resolve");
                assert!(host.gl_finish.is_some(), "host glFinish did not resolve");
            }
            Err(e) => eprintln!("skip host_egl_loads: {e}"),
        }
    }

    #[test]
    fn create_context_and_swap_geometry() {
        if let Err(e) = host() {
            eprintln!("skip create_context_and_swap_geometry: {e}");
            return;
        }

        // This test must exercise the no-daemon path.
        std::env::remove_var("PENGUDROID_SURFACE_ID");
        std::env::set_var("PENGUDROID_SURFACE_W", "64");
        std::env::set_var("PENGUDROID_SURFACE_H", "48");

        unsafe {
            let dpy = eglGetDisplay(std::ptr::null_mut());
            if dpy.is_null() {
                eprintln!("skip create_context_and_swap_geometry: no EGL display");
                return;
            }
            let mut major: c_int = 0;
            let mut minor: c_int = 0;
            if eglInitialize(dpy, &mut major, &mut minor) != EGL_TRUE {
                eprintln!("skip create_context_and_swap_geometry: eglInitialize failed");
                return;
            }
            assert_eq!(eglBindAPI(0x30A0), EGL_TRUE, "eglBindAPI(ES) failed");

            let config_attribs: [c_int; 13] = [
                0x3033, 0x0001, // EGL_SURFACE_TYPE, EGL_PBUFFER_BIT
                0x3040, 0x0004, // EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT
                0x3024, 8, // EGL_RED_SIZE
                0x3023, 8, // EGL_GREEN_SIZE
                0x3022, 8, // EGL_BLUE_SIZE
                0x3021, 8, // EGL_ALPHA_SIZE
                0x3038, // EGL_NONE
            ];
            let mut config: EGLConfig = std::ptr::null_mut();
            let mut num_configs: c_int = 0;
            assert_eq!(
                eglChooseConfig(
                    dpy,
                    config_attribs.as_ptr(),
                    &mut config,
                    1,
                    &mut num_configs,
                ),
                EGL_TRUE,
                "eglChooseConfig failed"
            );
            assert!(num_configs >= 1 && !config.is_null(), "no config returned");

            let context_attribs: [c_int; 3] = [0x3098, 2, 0x3038]; // client version 2
            let ctx = eglCreateContext(
                dpy,
                config,
                std::ptr::null_mut(),
                context_attribs.as_ptr(),
            );
            assert!(!ctx.is_null(), "eglCreateContext failed");

            let surface =
                eglCreateWindowSurface(dpy, config, std::ptr::null_mut(), std::ptr::null());
            assert!(!surface.is_null(), "eglCreateWindowSurface failed");

            assert_eq!(
                eglMakeCurrent(dpy, surface, surface, ctx),
                EGL_TRUE,
                "eglMakeCurrent failed"
            );

            let clear_color: Option<unsafe extern "C" fn(f32, f32, f32, f32)> =
                gl_proc("glClearColor");
            let clear: Option<unsafe extern "C" fn(u32)> = gl_proc("glClear");
            let clear_color = clear_color.expect("glClearColor must resolve");
            let clear = clear.expect("glClear must resolve");
            clear_color(1.0, 0.0, 0.0, 1.0);
            clear(0x4000); // GL_COLOR_BUFFER_BIT

            let exported =
                export_surface_dmabuf(surface).expect("dmabuf export must succeed");
            assert!(exported.fd >= 0, "export returned an invalid fd");
            let rec = &*(surface as *const SurfaceRec);
            assert_eq!(exported.width, 64, "export width must match the surface");
            assert_eq!(exported.height, 48, "export height must match the surface");
            assert_eq!(rec.w, 64);
            assert_eq!(rec.h, 48);
            assert_eq!(exported.stride % 4, 0, "stride is not pixel aligned");
            assert!(exported.fourcc != 0, "export returned no fourcc");

            // We own the fd from the export.
            libc::close(exported.fd);

            assert_eq!(eglDestroySurface(dpy, surface), EGL_TRUE);
            assert_eq!(eglDestroyContext(dpy, ctx), EGL_TRUE);
        }
    }
}
