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
            match std::fs::read_dir("/dev/dri") {
                Ok(nodes) => {
                    let mut nodes: Vec<_> = nodes.flatten().filter(|n| n.file_name().to_string_lossy().starts_with("renderD")).collect();
                    nodes.sort_by_key(|n| n.file_name());
                    for node in nodes {
                        match pengudroid_hostgpu::video::decode_info(&node.path()) {
                            Ok(video) => {
                                println!("video device:     {}", video.device);
                                println!("VA-API vendor:    {}", video.vendor);
                                println!("VA-API version:   {}.{}", video.version.0, video.version.1);
                                println!("VLD profiles:     {}", video.profiles.join(", "));
                            }
                            Err(error) => println!("VA-API:           unavailable ({error}); other decode backends not probed"),
                        }
                    }
                }
                Err(error) => println!("video devices:    unavailable ({error})"),
            }
            println!("guest video path: Android software Codec2; host hardware decode adapter pending");
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

/// Decode one Annex-B H.264 access unit on VA-API and export GPU planes.
/// The optional readback verifies a fixture; normal decode never downloads it.
pub fn video_test(args: &[String]) -> i32 {
    let mut device = std::path::PathBuf::from("/dev/dri/renderD128");
    let mut verify = false;
    let mut broker = false;
    let mut path = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--device" => match args.next() { Some(p) => device = p.into(), None => return 2 },
            "--verify-gradient" => verify = true,
            "--broker" => broker = true,
            value if !value.starts_with('-') && path.is_none() => path = Some(value.to_owned()),
            _ => { eprintln!("unknown video-decode-test argument: {arg}"); return 2; }
        }
    }
    let result = (|| -> Result<(), String> {
        use pengudroid_hostgpu::video::{Decoder, ReceiveStatus, SendStatus};
        let path = path.ok_or("usage: pengudroid video-decode-test <one-frame.h264> [--device /dev/dri/renderD128] [--verify-gradient]")?;
        if std::fs::metadata(&path).map_err(|e| e.to_string())?.len() > 16 * 1024 * 1024 {
            return Err("access unit exceeds 16 MiB".into());
        }
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        if broker {
            if verify { return Err("--verify-gradient uses diagnostic readback; run it without --broker".into()); }
            return video_broker_test(&bytes, &device);
        }
        let mut decoder = Decoder::open("h264", &device, &[])?;
        if decoder.send(&bytes, 123456)? != SendStatus::Accepted { return Err("initial packet was not accepted".into()); }
        let mut eos = false;
        let mut count = 0;
        loop {
            match decoder.receive()? {
                ReceiveStatus::Frame(frame) => {
                    count += 1;
                    println!("hardware decoded: {}x{}, pts={} us, fourcc={:#010x}, objects={}, planes={}",
                        frame.width(), frame.height(), frame.pts_us(), frame.fourcc(), frame.object_count(), frame.planes().len());
                    for (index, plane) in frame.planes().iter().enumerate() {
                        println!("plane {index}: object={} stride={} offset={} modifier={:#x}",
                            plane.object_index, plane.stride, plane.offset,
                            frame.object_modifier(plane.object_index as usize).unwrap());
                    }
                    if verify {
                        let luma = frame.test_luma()?;
                        if luma != 64 { return Err(format!("gradient verification failed: luma={luma}, expected 64")); }
                        println!("diagnostic GPU readback: first luma=64, verified");
                    }
                }
                ReceiveStatus::NeedsInput if !eos => {
                    eos = decoder.end_of_stream()? == SendStatus::Accepted;
                }
                ReceiveStatus::EndOfStream => break,
                _ => return Err("decoder made no progress".into()),
            }
        }
        if count == 0 { return Err("no hardware-decoded frame received".into()); }
        println!("VA-API decode and synchronized GPU export: OK ({count} frames); NewPipe adapter pending");
        Ok(())
    })();
    match result { Ok(()) => 0, Err(error) => { eprintln!("pengudroid video: {error}"); 1 } }
}

/// Exercise the connection-local packet -> hardware decode -> compositor path.
fn video_broker_test(bytes: &[u8], device: &std::path::Path) -> Result<(), String> {
    use pengudroid_proto::{default_socket_path, Message, Role, PROTO_VERSION};
    use pengudroid_socket::Connection;
    use std::time::{Duration, Instant};
    let conn = Connection::connect_endpoint(&default_socket_path()).map_err(|e| e.to_string())?;
    conn.set_read_timeout(Some(Duration::from_secs(5))).map_err(|e| e.to_string())?;
    let exchange = |request: &Message| -> Result<Message, String> {
        conn.send_message(request, &[]).map_err(|e| e.to_string())?;
        let (reply, fds) = conn.recv_message().map_err(|e| e.to_string())?;
        for fd in fds { unsafe { libc::close(fd); } }
        if let Message::Error { message, .. } = reply { return Err(message); }
        Ok(reply)
    };
    match exchange(&Message::Hello { version: PROTO_VERSION, role: Role::Cli, pid: std::process::id() as i32 })? {
        Message::Welcome { .. } => {}, reply => return Err(format!("unexpected handshake: {reply:?}")),
    }
    let surface_id = match exchange(&Message::CreateSurface { width: 640, height: 360, title: "PenguDroid GPU video broker".into() })? {
        Message::SurfaceCreated { surface_id, .. } => surface_id,
        reply => return Err(format!("unexpected video surface reply: {reply:?}")),
    };
    let result = (|| -> Result<(), String> {
        match exchange(&Message::VideoDecoderOpen { surface_id, codec: "h264".into(), device: device.display().to_string(), extra: vec![] })? {
            Message::VideoDecoderReady => {}, reply => return Err(format!("unexpected decoder reply: {reply:?}")),
        }
        match exchange(&Message::VideoPacket { pts_us: 123456, data: bytes.to_vec(), end_of_stream: false })? {
            Message::VideoDecodeStatus { input_accepted: true, .. } => {},
            reply => return Err(format!("initial video access unit was not accepted: {reply:?}")),
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut eos_sent = false;
        loop {
            if Instant::now() >= deadline { return Err("hardware decoder broker timed out".into()); }
            let command = if eos_sent { Message::VideoDecoderPoll }
                else { Message::VideoPacket { pts_us: 0, data: vec![], end_of_stream: true } };
            match exchange(&command)? {
                Message::VideoDecodeStatus { input_accepted, decoded, presented, end_of_stream, last_pts_us, .. } => {
                    if input_accepted { eos_sent = true; }
                    if end_of_stream {
                        if decoded == 0 || presented == 0 || last_pts_us != Some(123456) {
                            return Err(format!("incomplete broker video output: decoded={decoded}, submitted={presented}, pts={last_pts_us:?}"));
                        }
                        println!("broker hardware decode: {decoded} frames, {presented} GPU imports, pts={last_pts_us:?} us, EOS drained");
                        println!("compressed input reached the daemon; no CPU pixel transport; NewPipe Codec2 adapter pending");
                        return Ok(());
                    }
                }
                reply => return Err(format!("unexpected video status: {reply:?}")),
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    })();
    // Explicit cleanup on both success and decoder/import failure.
    let close = exchange(&Message::VideoDecoderClose);
    let destroy = exchange(&Message::DestroySurface { surface_id });
    result?; close?; destroy?;
    Ok(())
}
