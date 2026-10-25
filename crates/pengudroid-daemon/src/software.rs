//! CPU GraphicBuffer snapshots for wl_shm. Never import these memfds as dma-bufs.
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt;

/// Copy a decoder-owned buffer before acknowledging it: the producer can reuse
/// its storage as soon as the reply arrives, while Wayland reads asynchronously.
/// Output is little-endian XRGB8888 (B, G, R, unused), top row first.
pub fn snapshot(fd: OwnedFd, width: u32, height: u32, stride: u32, format: u32) -> Result<Vec<u8>, String> {
    let bpp = match format {
        1 | 2 | 5 => 4usize, // RGBA8888, RGBX8888, BGRA8888
        4 => 2, // RGB565
        _ => return Err(format!("unsupported software pixel format {format}")),
    };
    let (w, h, stride) = (width as usize, height as usize, stride as usize);
    if w == 0 || h == 0 || w > 8192 || h > 8192 {
        return Err("invalid software frame dimensions".into());
    }
    let row = w.checked_mul(bpp).ok_or("row size overflow")?;
    if stride < row {
        return Err("software frame stride is too short".into());
    }
    let size = stride.checked_mul(h - 1).and_then(|n| n.checked_add(row)).ok_or("frame size overflow")?;
    let output_size = w.checked_mul(h).and_then(|n| n.checked_mul(4)).ok_or("frame size overflow")?;
    if size > 128 * 1024 * 1024 || output_size > 128 * 1024 * 1024 {
        return Err("software frame exceeds memory limit".into());
    }
    let file = std::fs::File::from(fd);
    if file.metadata().map_err(|e| e.to_string())?.len() < size as u64 {
        return Err("software frame fd is shorter than the frame".into());
    }
    let mut input = vec![0u8; size];
    file.read_exact_at(&mut input, 0).map_err(|e| e.to_string())?;
    let mut output = vec![0u8; output_size];
    for y in 0..h {
        for x in 0..w {
            let src = &input[y * stride + x * bpp..][..bpp];
            let dst = &mut output[(y * w + x) * 4..][..4];
            let (r, g, b) = match format {
                4 => {
                    let p = u16::from_le_bytes([src[0], src[1]]);
                    let (r, g, b) = ((p >> 11) as u8, ((p >> 5) & 63) as u8, (p & 31) as u8);
                    ((r << 3) | (r >> 2), (g << 2) | (g >> 4), (b << 3) | (b >> 2))
                }
                5 => (src[2], src[1], src[0]),
                _ => (src[0], src[1], src[2]),
            };
            dst.copy_from_slice(&[b, g, r, 255]);
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ashmem::AshmemRegion;

    fn frame(bytes: &[u8]) -> OwnedFd {
        let region = AshmemRegion::create("test-frame", bytes.len() as u64).unwrap();
        region.map_mut().unwrap().as_mut_slice().copy_from_slice(bytes);
        region.try_clone_fd().unwrap()
    }

    #[test]
    fn rgb565_snapshot_preserves_rows_and_ignores_padding() {
        let pixels = [0, 0xf8, 0xe0, 7, 99, 99, 0x1f, 0, 0xff, 0xff];
        assert_eq!(snapshot(frame(&pixels), 2, 2, 6, 4).unwrap(),
            [0, 0, 255, 255, 0, 255, 0, 255, 255, 0, 0, 255, 255, 255, 255, 255]);
    }

    #[test]
    fn rgba_and_bgra_have_correct_channel_order() {
        assert_eq!(snapshot(frame(&[10, 20, 30, 40]), 1, 1, 4, 1).unwrap(), [30, 20, 10, 255]);
        assert_eq!(snapshot(frame(&[30, 20, 10, 40]), 1, 1, 4, 5).unwrap(), [30, 20, 10, 255]);
    }

    #[test]
    fn invalid_layouts_are_rejected_before_reading() {
        for (w, h, stride, format) in [(0, 1, 4, 1), (2, 1, 2, 4), (1, 2, 4, 1),
            (1, 1, 4, 99), (8193, 1, u32::MAX, 1), (1, 2, u32::MAX, 1)] {
            assert!(snapshot(frame(&[0; 4]), w, h, stride, format).is_err());
        }
    }
}

/// Fit a decoded frame to SurfaceView's layout rectangle. The decoder may send
/// 1280x720 while the app reserves only 720x405; buffer size is not window size.
pub fn resize(input: Vec<u8>, width: u32, height: u32, output_width: u32, output_height: u32) -> Result<Vec<u8>, String> {
    if width == 0 || height == 0 || output_width == 0 || output_height == 0
        || width > 8192 || height > 8192 || output_width > 8192 || output_height > 8192 {
        return Err("invalid video resize dimensions".into());
    }
    let size = output_width as usize * output_height as usize * 4;
    if size > 128 * 1024 * 1024 || input.len() != width as usize * height as usize * 4 {
        return Err("invalid video resize buffer size".into());
    }
    if (width, height) == (output_width, output_height) { return Ok(input); }
    // Nearest-neighbour, with the source index of every output column and row
    // computed once instead of twice per pixel. The mapping is identical to
    // `x * width / output_width`, so the result is byte-for-byte the same; it
    // just removes ~2 divisions per pixel from a path that runs per frame.
    let (src_w, src_h) = (width as usize, height as usize);
    let (dst_w, dst_h) = (output_width as usize, output_height as usize);
    let x_map: Vec<usize> = (0..dst_w).map(|x| x * src_w / dst_w).collect();
    let y_map: Vec<usize> = (0..dst_h).map(|y| y * src_h / dst_h).collect();
    let mut output = vec![0u8; size];
    for (y, &source_y) in y_map.iter().enumerate() {
        let src_row = &input[source_y * src_w * 4..(source_y + 1) * src_w * 4];
        let dst_row = &mut output[y * dst_w * 4..(y + 1) * dst_w * 4];
        for (x, &source_x) in x_map.iter().enumerate() {
            let from = source_x * 4;
            dst_row[x * 4..x * 4 + 4].copy_from_slice(&src_row[from..from + 4]);
        }
    }
    Ok(output)
}

#[cfg(test)]
mod resize_tests {
    use super::resize;

    /// The pre-optimisation implementation, kept as the reference the faster
    /// one must match exactly.
    fn resize_reference(
        input: &[u8],
        width: u32,
        height: u32,
        output_width: u32,
        output_height: u32,
    ) -> Vec<u8> {
        let mut output = vec![0u8; output_width as usize * output_height as usize * 4];
        for y in 0..output_height as usize {
            let source_y = y * height as usize / output_height as usize;
            for x in 0..output_width as usize {
                let source_x = x * width as usize / output_width as usize;
                let from = (source_y * width as usize + source_x) * 4;
                let to = (y * output_width as usize + x) * 4;
                output[to..to + 4].copy_from_slice(&input[from..from + 4]);
            }
        }
        output
    }

    #[test]
    fn fast_resize_matches_the_reference_for_many_shapes() {
        let (w, h) = (37u32, 23u32);
        let input: Vec<u8> = (0..w * h * 4).map(|v| v as u8).collect();
        for (ow, oh) in [
            (37u32, 23u32),
            (18, 11),
            (74, 46),
            (1, 1),
            (37, 1),
            (1, 23),
            (100, 7),
            (7, 100),
            (13, 13),
        ] {
            let fast = resize(input.clone(), w, h, ow, oh).unwrap();
            let reference = resize_reference(&input, w, h, ow, oh);
            assert_eq!(fast, reference, "resize {w}x{h} -> {ow}x{oh} diverged");
        }
    }
    #[test]
    fn decoded_frame_fits_the_view_without_flipping_or_channel_changes() {
        let mut input = Vec::new();
        for n in 0..8 { input.extend_from_slice(&[n, n + 1, n + 2, 255]); }
        let output = resize(input, 4, 2, 2, 2).unwrap();
        assert_eq!(output, [0,1,2,255, 2,3,4,255, 4,5,6,255, 6,7,8,255]);
    }
    #[test]
    fn malformed_resize_cannot_allocate_or_index_outside_the_frame() {
        assert!(resize(vec![0; 4], 2, 2, 1, 1).is_err());
        assert!(resize(vec![0; 4], 1, 1, u32::MAX, 1).is_err());
        assert!(resize(vec![0; 4], 1, 1, 0, 1).is_err());
        assert_eq!(resize(vec![1,2,3,255], 1, 1, 1, 1).unwrap(), [1,2,3,255]);
    }
}
