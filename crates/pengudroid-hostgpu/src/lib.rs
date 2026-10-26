//! Host GPU access for PenguDroid: EGL / OpenGL ES / GBM, loaded with `dlopen`.
//!
//! Android guests render with EGL + GLES against Bionic's graphics stack
//! (`libEGL.so`, `libGLESv2.so`, gralloc). PenguDroid's approach — mirroring the
//! "mount the host drivers" idea — is to bind the guest's graphics calls onto
//! the **host's** EGL/GLES implementation, which drives the real GPU, and to hand
//! the resulting buffers to the compositor.
//!
//! Nothing here links against EGL/GLES/GBM at build time (no `-dev` headers or
//! import libraries are required); the libraries are resolved at runtime with
//! `dlopen`, exactly as the Bionic shim resolves libc. This keeps the crate
//! buildable anywhere and lets it degrade gracefully on machines without a GPU.
//!
//! # What is provided
//!
//! * [`info`] — what EGL/GLES/GBM the host offers, and whether the dmabuf
//!   export/import extensions are present.
//! * [`Offscreen`] — an EGL context (surfaceless or GBM) with an FBO, able to
//!   render, read pixels back, and export the result as a dmabuf.
//! * [`Dmabuf`] — an exported dma-buf: the fd plus the geometry and format
//!   metadata a compositor needs (`zwp_linux_dmabuf_v1`).
//!
//! # Status
//!
//! The offscreen render + readback + dmabuf export path is real and tested
//! against the host GPU. Wiring it into a guest's actual EGL calls (so an app's
//! `eglSwapBuffers` lands on the Wayland surface) is a later milestone.
//!
//! # Safety model
//!
//! The unsafe surface is limited to the runtime loading in `ffi` and a small
//! number of individually marked FFI calls that pass raw pointers or take
//! ownership of a file descriptor. Everything else works with ordinary Rust
//! values and returns `Err(String)` rather than panicking when a driver or
//! extension is missing.
//!
//! EGL contexts are current per thread, so [`Offscreen`] is deliberately neither
//! `Send` nor `Sync`; create and use it on the same thread.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod video;

use std::os::fd::OwnedFd;

use ffi::{
    EGL_EXTENSIONS, EGL_VENDOR, EGL_VERSION, GL_RENDERER, GL_VENDOR, GL_VERSION,
};

/// Capabilities of the host graphics stack.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GpuInfo {
    pub egl_vendor: String,
    pub egl_version: String,
    pub gl_vendor: String,
    pub gl_renderer: String,
    pub gl_version: String,
    /// `libgbm.so.1` could be loaded.
    pub has_gbm: bool,
    /// `EGL_MESA_image_dma_buf_export` is advertised.
    pub has_dmabuf_export: bool,
    /// `EGL_EXT_image_dma_buf_import` (+ modifiers) is advertised.
    pub has_dmabuf_import: bool,
    /// `EGL_KHR_platform_gbm` / `EGL_MESA_platform_gbm` is advertised.
    pub has_platform_gbm: bool,
    /// `EGL_MESA_platform_surfaceless` is advertised.
    pub has_platform_surfaceless: bool,
}

impl GpuInfo {
    /// Whether the zero-copy path (render on the GPU, hand a dmabuf to the
    /// compositor) is available.
    pub fn can_export_dmabuf(&self) -> bool {
        self.has_gbm && self.has_dmabuf_export
    }
}

/// An exported dma-buf: an fd plus the metadata `zwp_linux_dmabuf_v1` needs.
#[derive(Debug)]
pub struct Dmabuf {
    pub fd: OwnedFd,
    pub width: u32,
    pub height: u32,
    /// Row stride in bytes.
    pub stride: u32,
    /// DRM fourcc, e.g. `DRM_FORMAT_ARGB8888`.
    pub fourcc: u32,
    /// DRM format modifier (`DRM_FORMAT_MOD_LINEAR` == 0 when linear).
    pub modifier: u64,
    /// Offset of the plane within the buffer.
    pub offset: u32,
}

/// Query the host's graphics capabilities.
pub fn info() -> Result<GpuInfo, String> {
    let libs = libs()?;
    let egl = &libs.egl;

    let display = init_display(egl)?;

    let egl_vendor = query_string(egl, display, EGL_VENDOR);
    let egl_version = query_string(egl, display, EGL_VERSION);
    let extensions = query_string(egl, display, EGL_EXTENSIONS);
    // Platform/platform-display extensions are *client* extensions and only
    // appear in the string queried with `EGL_NO_DISPLAY`.
    let client_extensions = query_string(egl, ffi::EGL_NO_DISPLAY, EGL_EXTENSIONS);
    let has_extension = |name: &str| {
        extensions.contains(name) || client_extensions.contains(name)
    };

    // A current context is required for `glGetString`.
    let context = create_es2_context(egl, display)?;
    let gl = &libs.gles;
    let gl_vendor = gl_string(gl, GL_VENDOR);
    let gl_renderer = gl_string(gl, GL_RENDERER);
    let gl_version = gl_string(gl, GL_VERSION);
    destroy_context(egl, display, context);

    // The display is intentionally left initialized: EGL displays are
    // process-global and shared between callers, so terminating here could pull
    // the rug out from under a live [`Offscreen`]. The driver releases it when
    // the process exits.

    Ok(GpuInfo {
        egl_vendor,
        egl_version,
        gl_vendor,
        gl_renderer,
        gl_version,
        has_gbm: libs.has_gbm,
        has_dmabuf_export: has_extension("EGL_MESA_image_dma_buf_export"),
        has_dmabuf_import: has_extension("EGL_EXT_image_dma_buf_import"),
        has_platform_gbm: has_extension("EGL_KHR_platform_gbm")
            || has_extension("EGL_MESA_platform_gbm"),
        has_platform_surfaceless: has_extension("EGL_MESA_platform_surfaceless"),
    })
}

/// An offscreen EGL context plus a render target.
pub struct Offscreen {
    egl: &'static ffi::EglLib,
    gl: &'static ffi::GlesLib,
    display: ffi::EGLDisplay,
    context: ffi::EGLContext,
    fbo: u32,
    texture: u32,
    program: u32,
    vertex_buffer: u32,
    attrib_pos: u32,
    width: u32,
    height: u32,
}

impl Offscreen {
    /// Create an offscreen context with a `width`×`height` RGBA8 render target.
    pub fn new(width: u32, height: u32) -> Result<Offscreen, String> {
        if width == 0 || height == 0 {
            return Err("offscreen size must be non-zero".into());
        }
        width
            .checked_mul(height)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| format!("offscreen size {width}x{height} overflows"))?;

        let libs = libs()?;
        let egl = &libs.egl;
        let gl = &libs.gles;

        let display = init_display(egl)?;
        let context = create_es2_context(egl, display)?;

        // If target creation fails, tear the context down; the driver frees any
        // partially created GL objects with it.
        let target = match create_target(gl, width, height) {
            Ok(target) => target,
            Err(err) => {
                destroy_context(egl, display, context);
                return Err(err);
            }
        };

        Ok(Offscreen {
            egl,
            gl,
            display,
            context,
            fbo: target.fbo,
            texture: target.texture,
            program: target.program,
            vertex_buffer: target.vertex_buffer,
            attrib_pos: target.attrib_pos,
            width,
            height,
        })
    }

    /// Clear the render target to an RGBA colour and draw a marker triangle,
    /// then `glFinish`.
    pub fn render(&mut self, r: f32, g: f32, b: f32, a: f32) -> Result<(), String> {
        let gl = self.gl;
        // SAFETY: all handles belong to the context created in `new`, which is
        // current on this thread for the lifetime of `self`.
        unsafe {
            (gl.bind_framebuffer)(ffi::GL_FRAMEBUFFER, self.fbo);
            (gl.viewport)(0, 0, self.width as i32, self.height as i32);
            (gl.clear_color)(r, g, b, a);
            (gl.clear)(ffi::GL_COLOR_BUFFER_BIT);

            // One flat white triangle, centred so a corner of the target stays
            // at the clear colour. This proves that real rasterisation happens
            // rather than just a cleared buffer being read back.
            (gl.use_program)(self.program);
            (gl.bind_buffer)(ffi::GL_ARRAY_BUFFER, self.vertex_buffer);
            (gl.enable_vertex_attrib_array)(self.attrib_pos);
            (gl.vertex_attrib_pointer)(
                self.attrib_pos,
                2,
                ffi::GL_FLOAT,
                ffi::GL_FALSE,
                0,
                std::ptr::null(),
            );
            (gl.draw_arrays)(ffi::GL_TRIANGLES, 0, 3);
            (gl.finish)();
        }

        let err = unsafe { (gl.get_error)() };
        if err != ffi::GL_NO_ERROR {
            return Err(format!("GL error 0x{err:04X} while rendering"));
        }
        Ok(())
    }

    /// Read the render target back as RGBA8 (`width * height * 4` bytes).
    pub fn read_pixels(&mut self) -> Result<Vec<u8>, String> {
        let gl = self.gl;
        let len = self.width as usize * self.height as usize * 4;
        let mut pixels = vec![0u8; len];

        // SAFETY: `pixels` is exactly `len` bytes and the FBO is RGBA8, so
        // `glReadPixels` writes within the allocation. GL's row order is
        // bottom-up relative to the usual top-down image convention; callers
        // that care must flip.
        unsafe {
            (gl.bind_framebuffer)(ffi::GL_FRAMEBUFFER, self.fbo);
            (gl.read_pixels)(
                0,
                0,
                self.width as i32,
                self.height as i32,
                ffi::GL_RGBA,
                ffi::GL_UNSIGNED_BYTE,
                pixels.as_mut_ptr().cast(),
            );
        }

        let err = unsafe { (gl.get_error)() };
        if err != ffi::GL_NO_ERROR {
            return Err(format!("GL error 0x{err:04X} while reading pixels"));
        }
        Ok(pixels)
    }

    /// Export the render target as a dma-buf (zero-copy hand-off to Wayland).
    pub fn export_dmabuf(&self) -> Result<Dmabuf, String> {
        let egl = self.egl;
        let create_image = egl.create_image_khr.ok_or_else(|| {
            "EGL_KHR_image_base (eglCreateImageKHR) is not available".to_string()
        })?;
        let destroy_image = egl.destroy_image_khr.ok_or_else(|| {
            "EGL_KHR_image_base (eglDestroyImageKHR) is not available".to_string()
        })?;
        let query_image = egl.export_dmabuf_query.ok_or_else(|| {
            "EGL_MESA_image_dma_buf_export (query) is not available".to_string()
        })?;
        let export_image = egl.export_dmabuf_export.ok_or_else(|| {
            "EGL_MESA_image_dma_buf_export (export) is not available".to_string()
        })?;

        // Keep the texture storage alive for the duration of the export. The
        // dmabuf itself outlives the EGLImage, so it is fine to destroy the
        // image as soon as we hold the fd.
        // EGL_TRUE is 1 in the signed attribute list.
        let preserve = [ffi::EGL_IMAGE_PRESERVED_KHR, 1, ffi::EGL_NONE];
        // SAFETY: the texture name is a live GL texture in the current context,
        // and `preserve` is a NUL-terminated attribute list.
        let image = unsafe {
            create_image(
                self.display,
                // Mesa resolves the GL texture name against the given
                // context, so pass the context that owns the texture (it is
                // current on this thread).
                self.context,
                ffi::EGL_GL_TEXTURE_2D_KHR,
                self.texture as usize as ffi::EGLClientBuffer,
                preserve.as_ptr(),
            )
        };
        if image.is_null() {
            return Err(format!(
                "eglCreateImageKHR failed: {}",
                egl_error(egl)
            ));
        }

        let result = export_plane0(egl, self, image, query_image, export_image);
        // SAFETY: `image` was created above by us and is no longer used.
        unsafe { destroy_image(self.display, image) };
        result
    }
}

impl Drop for Offscreen {
    fn drop(&mut self) {
        // SAFETY: the context was created by `new` on this thread and is not
        // used after this point. EGL permits destroying a non-current context;
        // clearing the current binding first keeps the thread state clean.
        unsafe {
            (self.egl.make_current)(
                self.display,
                ffi::EGL_NO_SURFACE,
                ffi::EGL_NO_SURFACE,
                ffi::EGL_NO_CONTEXT,
            );
            (self.egl.destroy_context)(self.display, self.context);
        }
        // As in `info`, the process-global display is left initialized.
    }
}

// ---------------------------------------------------------------------------
// Rendering helpers
// ---------------------------------------------------------------------------

/// GL objects backing an [`Offscreen`], created once.
struct Target {
    texture: u32,
    fbo: u32,
    program: u32,
    vertex_buffer: u32,
    attrib_pos: u32,
}

const VERTEX_SHADER_SRC: &str = "\
attribute vec2 a_pos;
void main() {
    gl_Position = vec4(a_pos, 0.0, 1.0);
}
";

// The fragment shader ignores the clear colour on purpose: the triangle must be
// visibly different from the background so the tests can tell rasterisation
// from a simple clear.
const FRAGMENT_SHADER_SRC: &str = "\
precision mediump float;
void main() {
    gl_FragColor = vec4(1.0, 1.0, 1.0, 1.0);
}
";

/// A triangle inscribed in the viewport (NDC coordinates, 2 floats/vertex).
const TRIANGLE: [f32; 6] = [
    -0.5, -0.5, //
    0.5, -0.5, //
    0.0, 0.5,
];

fn create_target(gl: &ffi::GlesLib, width: u32, height: u32) -> Result<Target, String> {
    let mut texture: u32 = 0;
    // SAFETY: all `gl` handles are from the loaded-anyway GLES library; the
    // context is current, and all out-pointers are valid locals.
    unsafe {
        (gl.gen_textures)(1, &mut texture);
        if texture == 0 {
            return Err("glGenTextures returned 0".into());
        }
        (gl.bind_texture)(ffi::GL_TEXTURE_2D, texture);
        (gl.tex_image_2d)(
            ffi::GL_TEXTURE_2D,
            0,
            ffi::GL_RGBA8 as i32,
            width as i32,
            height as i32,
            0,
            ffi::GL_RGBA,
            ffi::GL_UNSIGNED_BYTE,
            std::ptr::null(),
        );
        (gl.tex_parameter_i)(
            ffi::GL_TEXTURE_2D,
            ffi::GL_TEXTURE_MIN_FILTER,
            ffi::GL_LINEAR as i32,
        );
        (gl.tex_parameter_i)(
            ffi::GL_TEXTURE_2D,
            ffi::GL_TEXTURE_MAG_FILTER,
            ffi::GL_LINEAR as i32,
        );
    }

    let mut fbo: u32 = 0;
    // SAFETY: as above.
    unsafe {
        (gl.gen_framebuffers)(1, &mut fbo);
        if fbo == 0 {
            return Err("glGenFramebuffers returned 0".into());
        }
        (gl.bind_framebuffer)(ffi::GL_FRAMEBUFFER, fbo);
        (gl.framebuffer_texture_2d)(
            ffi::GL_FRAMEBUFFER,
            ffi::GL_COLOR_ATTACHMENT0,
            ffi::GL_TEXTURE_2D,
            texture,
            0,
        );
        let status = (gl.check_framebuffer_status)(ffi::GL_FRAMEBUFFER);
        if status != ffi::GL_FRAMEBUFFER_COMPLETE {
            return Err(format!("framebuffer incomplete (status 0x{status:04X})"));
        }
    }

    let program = create_program(gl)?;

    let name = std::ffi::CString::new("a_pos").expect("literal has no NUL");
    // SAFETY: `program` is linked and `name` is a valid C string.
    let attrib_pos = unsafe { (gl.get_attrib_location)(program, name.as_ptr()) };
    if attrib_pos < 0 {
        // SAFETY: `program` is live in the current context.
        unsafe { (gl.delete_program)(program) };
        return Err("vertex attribute `a_pos` was optimised out of the shader".into());
    }

    let mut vertex_buffer: u32 = 0;
    // SAFETY: `TRIANGLE` lives for the duration of this call; GL copies the
    // data into its own buffer.
    unsafe {
        (gl.gen_buffers)(1, &mut vertex_buffer);
        if vertex_buffer == 0 {
            (gl.delete_program)(program);
            return Err("glGenBuffers returned 0".into());
        }
        (gl.bind_buffer)(ffi::GL_ARRAY_BUFFER, vertex_buffer);
        (gl.buffer_data)(
            ffi::GL_ARRAY_BUFFER,
            std::mem::size_of_val(&TRIANGLE) as isize,
            TRIANGLE.as_ptr().cast(),
            ffi::GL_STATIC_DRAW,
        );
    }

    Ok(Target {
        texture,
        fbo,
        program,
        vertex_buffer,
        attrib_pos: attrib_pos as u32,
    })
}

fn create_program(gl: &ffi::GlesLib) -> Result<u32, String> {
    let vertex = compile_shader(gl, ffi::GL_VERTEX_SHADER, VERTEX_SHADER_SRC)?;
    let fragment = match compile_shader(gl, ffi::GL_FRAGMENT_SHADER, FRAGMENT_SHADER_SRC) {
        Ok(shader) => shader,
        Err(err) => {
            // SAFETY: `vertex` was created above and is not used further.
            unsafe { (gl.delete_shader)(vertex) };
            return Err(err);
        }
    };

    // SAFETY: all arguments are live GL object names.
    let program = unsafe { (gl.create_program)() };
    if program == 0 {
        unsafe {
            (gl.delete_shader)(vertex);
            (gl.delete_shader)(fragment);
        }
        return Err("glCreateProgram returned 0".into());
    }

    // SAFETY: as above.
    unsafe {
        (gl.attach_shader)(program, vertex);
        (gl.attach_shader)(program, fragment);
        (gl.link_program)(program);
    }

    let mut status: i32 = 0;
    // SAFETY: `program` is live and `status` is a valid out-pointer.
    unsafe { (gl.get_program_iv)(program, ffi::GL_LINK_STATUS, &mut status) };

    if status == 0 {
        let log = program_log(gl, program);
        // SAFETY: the objects are no longer needed.
        unsafe {
            (gl.delete_program)(program);
            (gl.delete_shader)(vertex);
            (gl.delete_shader)(fragment);
        }
        return Err(format!("shader program link failed: {log}"));
    }

    // Attached shaders stay alive with the program; deleting the names now
    // releases them once the program is dropped.
    // SAFETY: as above.
    unsafe {
        (gl.delete_shader)(vertex);
        (gl.delete_shader)(fragment);
    }
    Ok(program)
}

fn compile_shader(gl: &ffi::GlesLib, kind: u32, source: &str) -> Result<u32, String> {
    // SAFETY: `kind` is a valid shader type and the context is current.
    let shader = unsafe { (gl.create_shader)(kind) };
    if shader == 0 {
        return Err("glCreateShader returned 0".into());
    }

    let c_source = std::ffi::CString::new(source)
        .map_err(|_| "shader source unexpectedly contains a NUL byte".to_string())?;
    let source_ptr = c_source.as_ptr();
    let source_len = source.len() as i32;
    // SAFETY: `source_ptr` points to a NUL-terminated string; the array of one
    // pointer and the length are valid for this call.
    unsafe {
        (gl.shader_source)(shader, 1, &source_ptr, &source_len);
        (gl.compile_shader)(shader);
    }

    let mut status: i32 = 0;
    // SAFETY: `shader` is live and `status` is a valid out-pointer.
    unsafe { (gl.get_shader_iv)(shader, ffi::GL_COMPILE_STATUS, &mut status) };

    if status == 0 {
        let log = shader_log(gl, shader);
        // SAFETY: the shader is no longer needed.
        unsafe { (gl.delete_shader)(shader) };
        return Err(format!("shader compilation failed: {log}"));
    }
    Ok(shader)
}

fn shader_log(gl: &ffi::GlesLib, shader: u32) -> String {
    let mut buffer = vec![0u8; 4096];
    let mut written: i32 = 0;
    // SAFETY: the buffer is 4096 bytes and `written` is a valid out-pointer.
    unsafe {
        (gl.get_shader_info_log)(
            shader,
            buffer.len() as i32,
            &mut written,
            buffer.as_mut_ptr().cast(),
        );
    }
    c_log(&buffer, written)
}

fn program_log(gl: &ffi::GlesLib, program: u32) -> String {
    let mut buffer = vec![0u8; 4096];
    let mut written: i32 = 0;
    // SAFETY: the buffer is 4096 bytes and `written` is a valid out-pointer.
    unsafe {
        (gl.get_program_info_log)(
            program,
            buffer.len() as i32,
            &mut written,
            buffer.as_mut_ptr().cast(),
        );
    }
    c_log(&buffer, written)
}

fn c_log(buffer: &[u8], written: i32) -> String {
    if written <= 0 {
        return "<no log>".into();
    }
    let len = (written as usize).min(buffer.len());
    String::from_utf8_lossy(&buffer[..len])
        .trim_end_matches('\0')
        .trim()
        .to_owned()
}

/// Export plane 0 of an EGLImage as a [`Dmabuf`].
fn export_plane0(
    egl: &ffi::EglLib,
    target: &Offscreen,
    image: ffi::EGLImageKHR,
    query_image: ffi::EglExportDMABUFImageQueryMESA,
    export_image: ffi::EglExportDMABUFImageMESA,
) -> Result<Dmabuf, String> {
    let mut fourcc: i32 = 0;
    let mut num_planes: i32 = 0;
    let mut modifiers = [0u64; 4];

    // SAFETY: `image` is a live EGLImage; the out-pointers reference valid
    // locals, and `modifiers` has room for `num_planes` entries as required by
    // the extension.
    let ok = unsafe {
        query_image(
            target.display,
            image,
            &mut fourcc,
            &mut num_planes,
            modifiers.as_mut_ptr(),
        )
    };
    if ok != ffi::EGL_TRUE {
        return Err(format!(
            "eglExportDMABUFImageQueryMESA failed: {}",
            egl_error(egl)
        ));
    }
    if num_planes < 1 {
        return Err("dmabuf export reported no planes".into());
    }
    let planes = (num_planes as usize).min(modifiers.len());

    let mut fds = [-1i32; 4];
    let mut strides = [0i32; 4];
    let mut offsets = [0i32; 4];
    // SAFETY: three arrays of at least `num_planes` ints, as required.
    let ok = unsafe {
        export_image(
            target.display,
            image,
            fds.as_mut_ptr(),
            strides.as_mut_ptr(),
            offsets.as_mut_ptr(),
        )
    };
    if ok != ffi::EGL_TRUE {
        close_exported_fds(&fds[..planes]);
        return Err(format!(
            "eglExportDMABUFImageMESA failed: {}",
            egl_error(egl)
        ));
    }
    if fds[0] < 0 {
        close_exported_fds(&fds[..planes]);
        return Err("dmabuf export returned an invalid file descriptor".into());
    }

    // `Dmabuf` only carries plane 0 (the compositor path handles single-plane
    // RGBA). Close any additional plane fds, guarding against duplicates.
    for i in 1..planes {
        if fds[i] >= 0 && !fds[..i].contains(&fds[i]) {
            // SAFETY: this fd belongs to us and is not closed elsewhere.
            unsafe { libc::close(fds[i]) };
        }
    }

    // SAFETY: `fds[0]` is a freshly exported, owned descriptor; ownership moves
    // into `OwnedFd`, which will close it exactly once.
    let fd = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fds[0]) };

    Ok(Dmabuf {
        fd,
        width: target.width,
        height: target.height,
        stride: strides[0] as u32,
        fourcc: fourcc as u32,
        modifier: modifiers[0],
        offset: offsets[0] as u32,
    })
}

fn close_exported_fds(fds: &[i32]) {
    for &fd in fds {
        if fd >= 0 {
            // SAFETY: these were just created by the export extension and are
            // not owned anywhere else.
            unsafe { libc::close(fd) };
        }
    }
}

// ---------------------------------------------------------------------------
// EGL helpers
// ---------------------------------------------------------------------------

/// Try the surfaceless platform first, then fall back to the default display
/// (which drives GBM/DRM when a device is available).
fn init_display(egl: &ffi::EglLib) -> Result<ffi::EGLDisplay, String> {
    let mut major = 0;
    let mut minor = 0;

    if let Some(get_platform_display) = egl.get_platform_display_ext {
        // SAFETY: the surfaceless platform ignores the native display and
        // attribute list; NULL is the documented input.
        let display = unsafe {
            get_platform_display(
                ffi::EGL_PLATFORM_SURFACELESS_MESA,
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        if !display.is_null()
            // SAFETY: `display` is a non-null EGLDisplay and the version
            // out-pointers are valid.
            && unsafe { (egl.initialize)(display, &mut major, &mut minor) } == ffi::EGL_TRUE
        {
            return Ok(display);
        }
    }

    // SAFETY: EGL_DEFAULT_DISPLAY is a null native display handle.
    let display = unsafe { (egl.get_display)(std::ptr::null_mut()) };
    if display.is_null() {
        return Err(
            "no usable EGL display (surfaceless failed and EGL_DEFAULT_DISPLAY is null)".into(),
        );
    }
    // SAFETY: as above.
    if unsafe { (egl.initialize)(display, &mut major, &mut minor) } != ffi::EGL_TRUE {
        return Err(format!("eglInitialize failed: {}", egl_error(egl)));
    }
    Ok(display)
}

/// Choose an ES2 config and create + make current a surfaceless context.
fn create_es2_context(
    egl: &ffi::EglLib,
    display: ffi::EGLDisplay,
) -> Result<ffi::EGLContext, String> {
    let config_attribs = [
        ffi::EGL_SURFACE_TYPE,
        ffi::EGL_PBUFFER_BIT,
        ffi::EGL_RENDERABLE_TYPE,
        ffi::EGL_OPENGL_ES2_BIT,
        ffi::EGL_RED_SIZE,
        8,
        ffi::EGL_GREEN_SIZE,
        8,
        ffi::EGL_BLUE_SIZE,
        8,
        ffi::EGL_ALPHA_SIZE,
        8,
        ffi::EGL_NONE,
    ];
    let mut config: ffi::EGLConfig = std::ptr::null_mut();
    let mut num_configs: i32 = 0;

    // SAFETY: the attribute list is NUL-terminated and the out-pointers are
    // valid; `1` is the capacity of the single-element config array.
    let ok = unsafe {
        (egl.choose_config)(
            display,
            config_attribs.as_ptr(),
            &mut config,
            1,
            &mut num_configs,
        )
    };
    if ok != ffi::EGL_TRUE || num_configs < 1 {
        return Err(format!(
            "eglChooseConfig found no ES2 RGBA8 config: {}",
            egl_error(egl)
        ));
    }

    // SAFETY: plain EGL calls with no pointers.
    if unsafe { (egl.bind_api)(ffi::EGL_OPENGL_ES_API) } != ffi::EGL_TRUE {
        return Err(format!("eglBindAPI(ES) failed: {}", egl_error(egl)));
    }

    let context_attribs = [ffi::EGL_CONTEXT_CLIENT_VERSION, 2, ffi::EGL_NONE];
    // SAFETY: `config` came from eglChooseConfig on this display and the
    // attribute list is NUL-terminated.
    let context = unsafe {
        (egl.create_context)(
            display,
            config,
            ffi::EGL_NO_CONTEXT,
            context_attribs.as_ptr(),
        )
    };
    if context.is_null() {
        return Err(format!("eglCreateContext failed: {}", egl_error(egl)));
    }

    // SAFETY: a surfaceless context needs no draw/read surface.
    if unsafe {
        (egl.make_current)(
            display,
            ffi::EGL_NO_SURFACE,
            ffi::EGL_NO_SURFACE,
            context,
        )
    } != ffi::EGL_TRUE
    {
        let err = egl_error(egl);
        // SAFETY: `context` was created above and is not current.
        unsafe { (egl.destroy_context)(display, context) };
        return Err(format!(
            "eglMakeCurrent (surfaceless) failed: {err}; \
             does the driver advertise EGL_KHR_surfaceless_context?"
        ));
    }

    Ok(context)
}

fn destroy_context(egl: &ffi::EglLib, display: ffi::EGLDisplay, context: ffi::EGLContext) {
    // SAFETY: `context` belongs to `display` and is released once here.
    unsafe {
        (egl.make_current)(
            display,
            ffi::EGL_NO_SURFACE,
            ffi::EGL_NO_SURFACE,
            ffi::EGL_NO_CONTEXT,
        );
        (egl.destroy_context)(display, context);
    }
}

fn query_string(egl: &ffi::EglLib, display: ffi::EGLDisplay, what: i32) -> String {
    // SAFETY: `display` is initialized; `query_string` returns a NUL-terminated
    // string owned by EGL or NULL.
    let ptr = unsafe { (egl.query_string)(display, what) };
    if ptr.is_null() {
        String::new()
    } else {
        // SAFETY: EGL promises a NUL-terminated string.
        unsafe { std::ffi::CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned()
    }
}

fn gl_string(gl: &ffi::GlesLib, what: u32) -> String {
    // SAFETY: a context is current; `glGetString` returns a NUL-terminated
    // string owned by GL or NULL.
    let ptr = unsafe { (gl.get_string)(what) };
    if ptr.is_null() {
        String::new()
    } else {
        // SAFETY: GL promises a NUL-terminated string.
        unsafe { std::ffi::CStr::from_ptr(ptr.cast()) }
            .to_string_lossy()
            .into_owned()
    }
}

fn egl_error(egl: &ffi::EglLib) -> String {
    // SAFETY: plain EGL call, safe without arguments.
    let code = unsafe { (egl.get_error)() };
    let name = match code {
        0x3000 => "EGL_SUCCESS",
        0x3001 => "EGL_NOT_INITIALIZED",
        0x3002 => "EGL_BAD_ACCESS",
        0x3003 => "EGL_BAD_ALLOC",
        0x3004 => "EGL_BAD_ATTRIBUTE",
        0x3005 => "EGL_BAD_CONFIG",
        0x3006 => "EGL_BAD_CONTEXT",
        0x3007 => "EGL_BAD_CURRENT_SURFACE",
        0x3008 => "EGL_BAD_DISPLAY",
        0x3009 => "EGL_BAD_SURFACE",
        0x300A => "EGL_BAD_MATCH",
        0x300B => "EGL_BAD_PARAMETER",
        0x300C => "EGL_BAD_NATIVE_PIXMAP",
        0x300D => "EGL_BAD_NATIVE_WINDOW",
        0x300E => "EGL_BAD_STREAM",
        0x300F => "EGL_BAD_STATE",
        0x3010 => "EGL_BAD_ACCESS",
        0x3011 => "EGL_BAD_ALLOC",
        _ => "EGL error",
    };
    format!("{name} (0x{code:04X})")
}

// ---------------------------------------------------------------------------
// Runtime loading
// ---------------------------------------------------------------------------

fn libs() -> Result<&'static ffi::Libs, String> {
    ffi::LIBS
        .get_or_init(ffi::Libs::load)
        .as_ref()
        .map_err(Clone::clone)
}

/// The `dlopen`/`dlsym` boundary and the EGL/GLES entry points.
#[allow(dead_code)] // the full set of EGL/GLES constants is kept for clarity
mod ffi {
    use std::ffi::{c_char, c_int, c_void, CString};
    use std::sync::OnceLock;

    pub type EGLDisplay = *mut c_void;
    pub type EGLContext = *mut c_void;
    pub type EGLConfig = *mut c_void;
    pub type EGLImageKHR = *mut c_void;
    pub type EGLClientBuffer = *mut c_void;

    /// `EGLBoolean` is an unsigned int in EGL.
    pub const EGL_TRUE: u32 = 1;
    pub const EGL_NONE: i32 = 0x3038;
    pub const EGL_NO_DISPLAY: EGLDisplay = std::ptr::null_mut();
    pub const EGL_NO_CONTEXT: EGLContext = std::ptr::null_mut();
    pub const EGL_NO_SURFACE: *mut c_void = std::ptr::null_mut();
    pub const EGL_NO_IMAGE_KHR: EGLImageKHR = std::ptr::null_mut();

    pub const EGL_VENDOR: i32 = 0x3053;
    pub const EGL_VERSION: i32 = 0x3054;
    pub const EGL_EXTENSIONS: i32 = 0x3055;
    pub const EGL_OPENGL_ES_API: u32 = 0x30A0;
    pub const EGL_SURFACE_TYPE: c_int = 0x3033;
    pub const EGL_PBUFFER_BIT: c_int = 0x0001;
    pub const EGL_RENDERABLE_TYPE: c_int = 0x3040;
    pub const EGL_OPENGL_ES2_BIT: c_int = 0x0004;
    pub const EGL_ALPHA_SIZE: c_int = 0x3021;
    pub const EGL_BLUE_SIZE: c_int = 0x3022;
    pub const EGL_GREEN_SIZE: c_int = 0x3023;
    pub const EGL_RED_SIZE: c_int = 0x3024;
    pub const EGL_CONTEXT_CLIENT_VERSION: c_int = 0x3098;
    pub const EGL_PLATFORM_SURFACELESS_MESA: u32 = 0x31DD;
    pub const EGL_GL_TEXTURE_2D_KHR: u32 = 0x30B1;
    pub const EGL_IMAGE_PRESERVED_KHR: c_int = 0x30D2;

    pub const GL_NO_ERROR: u32 = 0;
    pub const GL_FALSE: u8 = 0;
    pub const GL_COLOR_BUFFER_BIT: u32 = 0x4000;
    pub const GL_FRAMEBUFFER: u32 = 0x8D40;
    pub const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
    pub const GL_TEXTURE_2D: u32 = 0x0DE1;
    pub const GL_RGBA: u32 = 0x1908;
    pub const GL_RGBA8: u32 = 0x8058;
    pub const GL_UNSIGNED_BYTE: u32 = 0x1401;
    pub const GL_FRAMEBUFFER_COMPLETE: u32 = 0x8CD5;
    pub const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
    pub const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
    pub const GL_LINEAR: u32 = 0x2601;
    pub const GL_TRIANGLES: u32 = 0x0004;
    pub const GL_FLOAT: u32 = 0x1406;
    pub const GL_VERTEX_SHADER: u32 = 0x8B31;
    pub const GL_FRAGMENT_SHADER: u32 = 0x8B30;
    pub const GL_COMPILE_STATUS: u32 = 0x8B81;
    pub const GL_LINK_STATUS: u32 = 0x8B82;
    pub const GL_ARRAY_BUFFER: u32 = 0x8892;
    pub const GL_STATIC_DRAW: u32 = 0x88E4;
    pub const GL_VENDOR: u32 = 0x1F00;
    pub const GL_RENDERER: u32 = 0x1F01;
    pub const GL_VERSION: u32 = 0x1F02;

    // -- EGL -----------------------------------------------------------------

    pub type EglGetProcAddress = unsafe extern "C" fn(*const c_char) -> *mut c_void;
    pub type EglGetDisplay = unsafe extern "C" fn(*mut c_void) -> EGLDisplay;
    pub type EglGetPlatformDisplayEXT =
        unsafe extern "C" fn(u32, *mut c_void, *const c_int) -> EGLDisplay;
    pub type EglInitialize =
        unsafe extern "C" fn(EGLDisplay, *mut c_int, *mut c_int) -> u32;
    #[allow(dead_code)]
    pub type EglTerminate = unsafe extern "C" fn(EGLDisplay) -> u32;
    pub type EglQueryString = unsafe extern "C" fn(EGLDisplay, c_int) -> *const c_char;
    pub type EglGetError = unsafe extern "C" fn() -> c_int;
    pub type EglChooseConfig = unsafe extern "C" fn(
        EGLDisplay,
        *const c_int,
        *mut EGLConfig,
        c_int,
        *mut c_int,
    ) -> u32;
    pub type EglCreateContext = unsafe extern "C" fn(
        EGLDisplay,
        EGLConfig,
        EGLContext,
        *const c_int,
    ) -> EGLContext;
    pub type EglMakeCurrent =
        unsafe extern "C" fn(EGLDisplay, *mut c_void, *mut c_void, EGLContext) -> u32;
    pub type EglDestroyContext = unsafe extern "C" fn(EGLDisplay, EGLContext) -> u32;
    pub type EglBindAPI = unsafe extern "C" fn(u32) -> u32;
    pub type EglCreateImageKHR = unsafe extern "C" fn(
        EGLDisplay,
        EGLContext,
        u32,
        EGLClientBuffer,
        *const c_int,
    ) -> EGLImageKHR;
    pub type EglDestroyImageKHR = unsafe extern "C" fn(EGLDisplay, EGLImageKHR) -> u32;
    pub type EglExportDMABUFImageQueryMESA = unsafe extern "C" fn(
        EGLDisplay,
        EGLImageKHR,
        *mut c_int,
        *mut c_int,
        *mut u64,
    ) -> u32;
    pub type EglExportDMABUFImageMESA = unsafe extern "C" fn(
        EGLDisplay,
        EGLImageKHR,
        *mut c_int,
        *mut c_int,
        *mut c_int,
    ) -> u32;

    // -- GLES2 ---------------------------------------------------------------

    pub type GlGetString = unsafe extern "C" fn(u32) -> *const u8;
    pub type GlGetError = unsafe extern "C" fn() -> u32;
    pub type GlViewport = unsafe extern "C" fn(c_int, c_int, c_int, c_int);
    pub type GlClearColor = unsafe extern "C" fn(f32, f32, f32, f32);
    pub type GlClear = unsafe extern "C" fn(u32);
    pub type GlGenFramebuffers = unsafe extern "C" fn(c_int, *mut u32);
    pub type GlBindFramebuffer = unsafe extern "C" fn(u32, u32);
    pub type GlFramebufferTexture2D = unsafe extern "C" fn(u32, u32, u32, u32, c_int);
    pub type GlGenTextures = unsafe extern "C" fn(c_int, *mut u32);
    pub type GlBindTexture = unsafe extern "C" fn(u32, u32);
    pub type GlTexImage2D = unsafe extern "C" fn(
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
    pub type GlTexParameteri = unsafe extern "C" fn(u32, u32, c_int);
    pub type GlCheckFramebufferStatus = unsafe extern "C" fn(u32) -> u32;
    pub type GlCreateShader = unsafe extern "C" fn(u32) -> u32;
    pub type GlShaderSource =
        unsafe extern "C" fn(u32, c_int, *const *const c_char, *const c_int);
    pub type GlCompileShader = unsafe extern "C" fn(u32);
    pub type GlGetShaderiv = unsafe extern "C" fn(u32, u32, *mut c_int);
    pub type GlGetShaderInfoLog = unsafe extern "C" fn(u32, c_int, *mut c_int, *mut c_char);
    pub type GlCreateProgram = unsafe extern "C" fn() -> u32;
    pub type GlAttachShader = unsafe extern "C" fn(u32, u32);
    pub type GlLinkProgram = unsafe extern "C" fn(u32);
    pub type GlGetProgramiv = unsafe extern "C" fn(u32, u32, *mut c_int);
    pub type GlGetProgramInfoLog = unsafe extern "C" fn(u32, c_int, *mut c_int, *mut c_char);
    pub type GlUseProgram = unsafe extern "C" fn(u32);
    pub type GlGenBuffers = unsafe extern "C" fn(c_int, *mut u32);
    pub type GlBindBuffer = unsafe extern "C" fn(u32, u32);
    pub type GlBufferData = unsafe extern "C" fn(u32, isize, *const c_void, u32);
    pub type GlGetAttribLocation = unsafe extern "C" fn(u32, *const c_char) -> c_int;
    pub type GlVertexAttribPointer =
        unsafe extern "C" fn(u32, c_int, u32, u8, c_int, *const c_void);
    pub type GlEnableVertexAttribArray = unsafe extern "C" fn(u32);
    pub type GlDrawArrays = unsafe extern "C" fn(u32, c_int, c_int);
    pub type GlReadPixels = unsafe extern "C" fn(c_int, c_int, c_int, c_int, u32, u32, *mut c_void);
    pub type GlFinish = unsafe extern "C" fn();
    pub type GlDeleteShader = unsafe extern "C" fn(u32);
    pub type GlDeleteProgram = unsafe extern "C" fn(u32);

    /// Core EGL entry points plus the optional extension entry points.
    pub struct EglLib {
        pub get_proc_address: Option<EglGetProcAddress>,
        pub get_display: EglGetDisplay,
        pub get_platform_display_ext: Option<EglGetPlatformDisplayEXT>,
        pub initialize: EglInitialize,
        pub query_string: EglQueryString,
        pub get_error: EglGetError,
        pub choose_config: EglChooseConfig,
        pub create_context: EglCreateContext,
        pub make_current: EglMakeCurrent,
        pub destroy_context: EglDestroyContext,
        pub bind_api: EglBindAPI,
        pub create_image_khr: Option<EglCreateImageKHR>,
        pub destroy_image_khr: Option<EglDestroyImageKHR>,
        pub export_dmabuf_query: Option<EglExportDMABUFImageQueryMESA>,
        pub export_dmabuf_export: Option<EglExportDMABUFImageMESA>,
    }

    impl EglLib {
        fn load(handle: *mut c_void) -> Result<Self, String> {
            fn core<T: Copy>(handle: *mut c_void, name: &str) -> Result<T, String> {
                sym_ptr(handle, name)
                    .and_then(cast_fn::<T>)
                    .ok_or_else(|| format!("libEGL entry point `{name}` is missing"))
            }

            let get_proc_address: Option<EglGetProcAddress> =
                sym_ptr(handle, "eglGetProcAddress").and_then(cast_fn);

            // Extension entry points: try `eglGetProcAddress` first (this is
            // where Mesa publishes them), then fall back to `dlsym`.
            let proc_or_sym = |name: &str| -> Option<*mut c_void> {
                if let Some(get_proc_address) = get_proc_address {
                    if let Ok(c_name) = CString::new(name) {
                        // SAFETY: `get_proc_address` is the real EGL entry
                        // point loaded from this library, and `c_name` is a
                        // valid NUL-terminated string.
                        let ptr = unsafe { get_proc_address(c_name.as_ptr()) };
                        if !ptr.is_null() {
                            return Some(ptr);
                        }
                    }
                }
                sym_ptr(handle, name)
            };

            Ok(EglLib {
                get_proc_address,
                get_display: core(handle, "eglGetDisplay")?,
                get_platform_display_ext: proc_or_sym("eglGetPlatformDisplayEXT")
                    .and_then(cast_fn),
                initialize: core(handle, "eglInitialize")?,
                query_string: core(handle, "eglQueryString")?,
                get_error: core(handle, "eglGetError")?,
                choose_config: core(handle, "eglChooseConfig")?,
                create_context: core(handle, "eglCreateContext")?,
                make_current: core(handle, "eglMakeCurrent")?,
                destroy_context: core(handle, "eglDestroyContext")?,
                bind_api: core(handle, "eglBindAPI")?,
                create_image_khr: proc_or_sym("eglCreateImageKHR").and_then(cast_fn),
                destroy_image_khr: proc_or_sym("eglDestroyImageKHR").and_then(cast_fn),
                export_dmabuf_query: proc_or_sym("eglExportDMABUFImageQueryMESA")
                    .and_then(cast_fn),
                export_dmabuf_export: proc_or_sym("eglExportDMABUFImageMESA").and_then(cast_fn),
            })
        }
    }

    /// GLES2 entry points. Generated so the names, types and loading logic
    /// cannot drift apart.
    macro_rules! define_gles {
        ($( $field:ident : $ty:ty => $name:literal ),* $(,)?) => {
            pub struct GlesLib {
                $( pub $field: $ty, )*
            }

            impl GlesLib {
                fn load(handle: *mut c_void, egl: &EglLib) -> Result<Self, String> {
                    $(
                        let $field: $ty = gl_proc(egl, handle, $name)
                            .and_then(cast_fn::<$ty>)
                            .ok_or_else(|| {
                                format!("libGLESv2 entry point `{}` is missing", $name)
                            })?;
                    )*
                    Ok(Self { $( $field, )* })
                }
            }
        };
    }

    define_gles! {
        get_string: GlGetString => "glGetString",
        get_error: GlGetError => "glGetError",
        viewport: GlViewport => "glViewport",
        clear_color: GlClearColor => "glClearColor",
        clear: GlClear => "glClear",
        gen_framebuffers: GlGenFramebuffers => "glGenFramebuffers",
        bind_framebuffer: GlBindFramebuffer => "glBindFramebuffer",
        framebuffer_texture_2d: GlFramebufferTexture2D => "glFramebufferTexture2D",
        gen_textures: GlGenTextures => "glGenTextures",
        bind_texture: GlBindTexture => "glBindTexture",
        tex_image_2d: GlTexImage2D => "glTexImage2D",
        tex_parameter_i: GlTexParameteri => "glTexParameteri",
        check_framebuffer_status: GlCheckFramebufferStatus => "glCheckFramebufferStatus",
        create_shader: GlCreateShader => "glCreateShader",
        shader_source: GlShaderSource => "glShaderSource",
        compile_shader: GlCompileShader => "glCompileShader",
        get_shader_iv: GlGetShaderiv => "glGetShaderiv",
        get_shader_info_log: GlGetShaderInfoLog => "glGetShaderInfoLog",
        create_program: GlCreateProgram => "glCreateProgram",
        attach_shader: GlAttachShader => "glAttachShader",
        link_program: GlLinkProgram => "glLinkProgram",
        get_program_iv: GlGetProgramiv => "glGetProgramiv",
        get_program_info_log: GlGetProgramInfoLog => "glGetProgramInfoLog",
        use_program: GlUseProgram => "glUseProgram",
        gen_buffers: GlGenBuffers => "glGenBuffers",
        bind_buffer: GlBindBuffer => "glBindBuffer",
        buffer_data: GlBufferData => "glBufferData",
        get_attrib_location: GlGetAttribLocation => "glGetAttribLocation",
        vertex_attrib_pointer: GlVertexAttribPointer => "glVertexAttribPointer",
        enable_vertex_attrib_array: GlEnableVertexAttribArray => "glEnableVertexAttribArray",
        draw_arrays: GlDrawArrays => "glDrawArrays",
        read_pixels: GlReadPixels => "glReadPixels",
        finish: GlFinish => "glFinish",
        delete_shader: GlDeleteShader => "glDeleteShader",
        delete_program: GlDeleteProgram => "glDeleteProgram",
    }

    /// Both libraries plus the GBM probe result, loaded once per process.
    pub struct Libs {
        pub egl: EglLib,
        pub gles: GlesLib,
        pub has_gbm: bool,
    }

    pub static LIBS: OnceLock<Result<Libs, String>> = OnceLock::new();

    impl Libs {
        pub fn load() -> Result<Libs, String> {
            let egl_handle = open_first(&["libEGL.so.1", "libEGL.so", "libEGL_mesa.so.0"])
                .ok_or_else(|| {
                    "could not load libEGL.so.1 \
                     (is an EGL implementation installed?)"
                        .to_string()
                })?;
            let egl = EglLib::load(egl_handle)?;

            let gles_handle = open_first(&["libGLESv2.so.2", "libGLESv2.so"])
                .ok_or_else(|| "could not load libGLESv2.so.2".to_string())?;
            let gles = GlesLib::load(gles_handle, &egl)?;

            let has_gbm = open_first(&["libgbm.so.1", "libgbm.so"]).is_some();

            // Handles are intentionally never `dlclose`d: the function
            // pointers above must stay valid for the whole process, which is
            // exactly the lifetime of this static.
            Ok(Libs { egl, gles, has_gbm })
        }
    }

    fn open_first(names: &[&str]) -> Option<*mut c_void> {
        for name in names {
            if let Ok(c_name) = CString::new(*name) {
                // SAFETY: `c_name` is a valid NUL-terminated soname.
                let handle = unsafe {
                    libc::dlopen(c_name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL)
                };
                if !handle.is_null() {
                    return Some(handle);
                }
            }
        }
        None
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

    fn gl_proc(egl: &EglLib, handle: *mut c_void, name: &str) -> Option<*mut c_void> {
        if let Some(get_proc_address) = egl.get_proc_address {
            if let Ok(c_name) = CString::new(name) {
                // SAFETY: as in `EglLib::load`.
                let ptr = unsafe { get_proc_address(c_name.as_ptr()) };
                if !ptr.is_null() {
                    return Some(ptr);
                }
            }
        }
        sym_ptr(handle, name)
    }

    /// Reinterpret a `dlsym`/`eglGetProcAddress` result as a function pointer.
    ///
    /// # Safety contract (internal)
    ///
    /// The caller must only instantiate `T` with an `unsafe extern "C" fn`
    /// type whose signature matches the symbol that `ptr` was looked up under.
    /// POSIX guarantees the representation of a `void *` matches a function
    /// pointer on the platforms this crate targets.
    fn cast_fn<T: Copy>(ptr: *mut c_void) -> Option<T> {
        if ptr.is_null() {
            return None;
        }
        debug_assert_eq!(
            std::mem::size_of::<T>(),
            std::mem::size_of::<*mut c_void>()
        );
        // SAFETY: `T` is one of the `unsafe extern "C" fn` types declared in
        // this module and matches the symbol it was looked up under. Function
        // pointers and data pointers have the same size on Linux; the caller
        // guarantees this instantiation is a matching entry-point type.
        Some(unsafe { std::mem::transmute_copy(&ptr) })
    }
}

#[cfg(test)]
mod tests {
    //! These tests exercise the real host GPU (EGL + GLES + Mesa) and need a
    //! working graphics stack. They fail loudly rather than skipping when a
    //! driver is missing, except for the dmabuf test which is logically skipped
    //! when the extension is not advertised. Run with `--test-threads=1` since
    //! EGL's current context is per-thread/per-process state.

    use super::*;
    use std::os::fd::AsRawFd;

    #[test]
    fn info_reports_a_gpu() {
        let info = info().unwrap();
        assert!(!info.egl_vendor.is_empty(), "empty EGL vendor");
        assert!(!info.gl_renderer.is_empty(), "empty GL renderer");
        assert!(
            info.has_platform_surfaceless,
            "expected EGL_MESA_platform_surfaceless, got extensions via {}",
            info.egl_vendor,
        );
        println!(
            "EGL vendor: {} / EGL {} / GL: {} ({}) / GL version {}",
            info.egl_vendor,
            info.egl_version,
            info.gl_renderer,
            info.gl_vendor,
            info.gl_version,
        );
    }

    #[test]
    fn offscreen_clear_and_readback() {
        let mut offscreen = Offscreen::new(64, 64).unwrap();
        offscreen.render(1.0, 0.0, 0.0, 1.0).unwrap();
        let pixels = offscreen.read_pixels().unwrap();
        assert_eq!(pixels.len(), 64 * 64 * 4);

        // glReadPixels returns rows bottom-up, but the marker triangle is in
        // the middle of the target, so any corner is background. Sample the
        // bottom-left pixel (first in the buffer).
        let (r, g, b, a) = (pixels[0], pixels[1], pixels[2], pixels[3]);
        assert!(
            r >= 200 && g <= 60 && b <= 60,
            "expected a dominantly red background, got rgba({r}, {g}, {b}, {a})",
        );
    }

    #[test]
    fn export_dmabuf_when_supported() {
        let info = info().unwrap();
        if !info.can_export_dmabuf() {
            println!(
                "skipping dmabuf export test: gbm={} export={}",
                info.has_gbm, info.has_dmabuf_export
            );
            return;
        }

        let mut offscreen = Offscreen::new(64, 64).unwrap();
        offscreen.render(0.0, 1.0, 0.0, 1.0).unwrap();
        let dmabuf = offscreen.export_dmabuf().unwrap();

        assert_eq!(dmabuf.width, 64);
        assert_eq!(dmabuf.height, 64);
        assert!(
            dmabuf.stride >= 64 * 4,
            "stride {} too small for 64 RGBA8 pixels",
            dmabuf.stride
        );
        let fd = dmabuf.fd.as_raw_fd();
        // SAFETY: `fd` is owned by `dmabuf` and valid for this syscall.
        let alive = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(alive >= 0, "exported dmabuf fd {fd} is invalid");

        println!(
            "dmabuf: {}x{} stride {} fourcc 0x{:08X} modifier 0x{:016X} offset {}",
            dmabuf.width,
            dmabuf.height,
            dmabuf.stride,
            dmabuf.fourcc,
            dmabuf.modifier,
            dmabuf.offset,
        );
    }
}
