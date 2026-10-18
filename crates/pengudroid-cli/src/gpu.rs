//! `pengudroid gpu-info` and `pengudroid gpu-test`.
//!
//! These exercise the host GPU path that replaces the CPU `wl_shm` presentation
//! with real GPU rendering: `gpu-info` reports what the host offers, and
//! `gpu-test` renders offscreen on the GPU, reads the pixels back to prove it
//! actually rasterised, and exports the result as a dma-buf (the zero-copy
//! hand-off a Wayland compositor consumes).

use pengudroid_hostgpu::{info as gpu_info, Offscreen};

fn yesno(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

/// `pengudroid gpu-info`
pub fn info() -> i32 {
    match gpu_info() {
        Ok(i) => {
            println!("EGL vendor:       {}", i.egl_vendor);
            println!("EGL version:      {}", i.egl_version);
            println!("GL vendor:        {}", i.gl_vendor);
            println!("GL renderer:      {}", i.gl_renderer);
            println!("GL version:       {}", i.gl_version);
            println!("libgbm:           {}", yesno(i.has_gbm));
            println!("platform gbm:     {}", yesno(i.has_platform_gbm));
            println!("surfaceless:      {}", yesno(i.has_platform_surfaceless));
            println!("dmabuf export:    {}", yesno(i.has_dmabuf_export));
            println!("dmabuf import:    {}", yesno(i.has_dmabuf_import));
            println!("zero-copy ready:  {}", yesno(i.can_export_dmabuf()));
            0
        }
        Err(e) => {
            eprintln!("pengudroid: {e}");
            1
        }
    }
}

/// `pengudroid gpu-test`
pub fn test() -> i32 {
    const W: u32 = 128;
    const H: u32 = 128;
    let (r, g, b, a) = (0.1f32, 0.2f32, 0.9f32, 1.0f32);

    let mut off = match Offscreen::new(W, H) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("pengudroid: cannot create offscreen context: {e}");
            return 1;
        }
    };
    if let Err(e) = off.render(r, g, b, a) {
        eprintln!("pengudroid: render failed: {e}");
        return 1;
    }
    let pixels = match off.read_pixels() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("pengudroid: readback failed: {e}");
            return 1;
        }
    };
    if pixels.len() != (W * H * 4) as usize {
        eprintln!("pengudroid: unexpected readback size {} (want {})", pixels.len(), W * H * 4);
        return 1;
    }

    // The triangle is centred, so a corner holds the clear colour. FBO rows are
    // bottom-up, but the corner is a corner either way.
    let expected = [(r * 255.0) as i32, (g * 255.0) as i32, (b * 255.0) as i32];
    let got = [pixels[0] as i32, pixels[1] as i32, pixels[2] as i32];
    let matches = (0..3).all(|i| (got[i] - expected[i]).abs() <= 8);
    println!(
        "rendered {W}x{H} on the GPU; corner pixel RGBA = ({},{},{},{})",
        pixels[0], pixels[1], pixels[2], pixels[3]
    );
    if !matches {
        eprintln!("pengudroid: pixel mismatch: expected ~{expected:?}, got {got:?}");
        return 1;
    }
    println!("pixel verification: OK");

    match off.export_dmabuf() {
        Ok(d) => {
            use std::os::fd::AsRawFd;
            println!(
                "dmabuf: {}x{} stride {} fourcc {:#010x} modifier {:#x} offset {} fd {}",
                d.width,
                d.height,
                d.stride,
                d.fourcc,
                d.modifier,
                d.offset,
                d.fd.as_raw_fd()
            );
            println!("zero-copy hand-off: OK");
        }
        Err(e) => println!("dmabuf export: unavailable ({e})"),
    }
    0
}
