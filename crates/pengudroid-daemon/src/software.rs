//! CPU GraphicBuffer snapshots for wl_shm. Never import these memfds as dma-bufs.
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt;

use pengudroid_proto::BufferCrop;

/// Copy a decoder-owned buffer before acknowledging it: the producer can reuse
/// its storage as soon as the reply arrives, while Wayland reads asynchronously.
/// Output is little-endian XRGB8888 (B, G, R, unused), top row first, with
/// dimensions given by `crop`. Width/height/stride still describe the allocation:
/// cropping must never move the YUV plane bases or change their row strides.
///
/// `stride` is the byte stride of the source's first (luma/RGB) row. Software
/// AVC decoders hand BLAST a YV12 buffer (`0x32315659`); NV12/NV21 and the
/// flexible YCbCr layout are converted the same way. Wayland's `wl_shm` carries
/// only packed RGB, so the conversion is required before presentation.
pub fn snapshot(fd: OwnedFd, width: u32, height: u32, stride: u32, format: u32, crop: BufferCrop) -> Result<Vec<u8>, String> {
    let yuv = matches!(format, 0x3231_5659 | 0x23 | 0x11 | 0x21);
    let bpp = match format {
        1 | 2 | 5 => 4usize, // RGBA8888, RGBX8888, BGRA8888
        4 => 2, // RGB565
        0x3231_5659 | 0x23 | 0x11 | 0x21 => 1,
        _ => return Err(format!("unsupported software pixel format {format}")),
    };
    let (w, h, stride) = (width as usize, height as usize, stride as usize);
    if w == 0 || h == 0 || w > 8192 || h > 8192 {
        return Err("invalid software frame dimensions".into());
    }
    let (crop_width, crop_height) = crop.dimensions(width, height)?;
    let (cw, ch) = (crop_width as usize, crop_height as usize);
    let row = w.checked_mul(bpp).ok_or("row size overflow")?;
    if stride < row {
        return Err("software frame stride is too short".into());
    }
    let (size, layout) = if yuv {
        // YV12 is fully planar: the chroma row stride is half the luma stride
        // and V precedes U. NV12/NV21 interleave two chroma bytes per sample
        // pair, so their chroma row stride equals the luma byte stride.
        let planar = format == 0x3231_5659;
        let chroma_stride = if planar { (stride / 2).max(1) } else { stride };
        let chroma_width = (w + 1) / 2;
        let chroma_row = if planar { chroma_width } else { chroma_width * 2 };
        if chroma_stride < chroma_row {
            return Err("software frame chroma stride is too short".into());
        }
        let chroma_rows = (h + 1) / 2;
        let luma = stride.checked_mul(h).ok_or("frame size overflow")?;
        let chroma = chroma_stride.checked_mul(chroma_rows).ok_or("frame size overflow")?;
        let chroma_total = if planar {
            chroma.checked_mul(2).ok_or("frame size overflow")?
        } else {
            chroma
        };
        (luma.checked_add(chroma_total).ok_or("frame size overflow")?, (chroma_stride, chroma_rows, luma))
    } else {
        (stride.checked_mul(h - 1).and_then(|n| n.checked_add(row)).ok_or("frame size overflow")?, (0, 0, 0))
    };
    let output_size = cw.checked_mul(ch).and_then(|n| n.checked_mul(4)).ok_or("frame size overflow")?;
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
    for y in 0..ch {
        let source_y = crop.top as usize + y;
        for x in 0..cw {
            let source_x = crop.left as usize + x;
            let (r, g, b) = if yuv {
                let (chroma_stride, chroma_rows, luma) = layout;
                let cx = source_x / 2;
                let cy = source_y / 2;
                let (u, v) = match format {
                    // YV12 stores the V plane before U.
                    0x3231_5659 => (
                        input[luma + chroma_stride * chroma_rows + cy * chroma_stride + cx],
                        input[luma + cy * chroma_stride + cx],
                    ),
                    // NV12 interleaves Cb,Cr; NV21 interleaves Cr,Cb.
                    0x23 => {
                        let i = luma + cy * chroma_stride + cx * 2;
                        (input[i], input[i + 1])
                    }
                    _ => {
                        let i = luma + cy * chroma_stride + cx * 2;
                        (input[i + 1], input[i])
                    }
                };
                yuv_to_rgb(input[source_y * stride + source_x], u, v)
            } else {
                let src = &input[source_y * stride + source_x * bpp..][..bpp];
                match format {
                    4 => {
                        let p = u16::from_le_bytes([src[0], src[1]]);
                        let (r, g, b) = ((p >> 11) as u8, ((p >> 5) & 63) as u8, (p & 31) as u8);
                        ((r << 3) | (r >> 2), (g << 2) | (g >> 4), (b << 3) | (b >> 2))
                    }
                    5 => (src[2], src[1], src[0]),
                    _ => (src[0], src[1], src[2]),
                }
            };
            let dst = &mut output[(y * cw + x) * 4..][..4];
            dst.copy_from_slice(&[b, g, r, 255]);
        }
    }
    Ok(output)
}

/// BT.601 limited-range YUV to RGB, matching the Android software decoder's
/// colour space. Fixed-point, integer-only.
fn yuv_to_rgb(y: u8, u: u8, v: u8) -> (u8, u8, u8) {
    let c = (y as i32 - 16).max(0);
    let d = u as i32 - 128;
    let e = v as i32 - 128;
    let clamp = |value: i32| value.clamp(0, 255) as u8;
    (
        clamp((298 * c + 409 * e + 128) >> 8),
        clamp((298 * c - 100 * d - 208 * e + 128) >> 8),
        clamp((298 * c + 516 * d + 128) >> 8),
    )
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
        assert_eq!(snapshot(frame(&pixels), 2, 2, 6, 4, BufferCrop::full(2, 2)).unwrap(),
            [0, 0, 255, 255, 0, 255, 0, 255, 255, 0, 0, 255, 255, 255, 255, 255]);
    }

    #[test]
    fn rgba_and_bgra_have_correct_channel_order() {
        assert_eq!(snapshot(frame(&[10, 20, 30, 40]), 1, 1, 4, 1, BufferCrop::full(1, 1)).unwrap(), [30, 20, 10, 255]);
        assert_eq!(snapshot(frame(&[30, 20, 10, 40]), 1, 1, 4, 5, BufferCrop::full(1, 1)).unwrap(), [30, 20, 10, 255]);
    }

    #[test]
    fn yv12_and_nv12_are_converted_to_rgb() {
        // 2x2 limited-range YUV: top row black, bottom row white. With a luma
        // stride of 2, planar YV12 has one byte per chroma plane and row;
        // semi-planar NV12 has two interleaved bytes per chroma row.
        let yv12 = [16, 16, 235, 235, 128, 128];
        assert_eq!(snapshot(frame(&yv12), 2, 2, 2, 0x3231_5659, BufferCrop::full(2, 2)).unwrap(),
            [0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255, 255, 255, 255, 255, 255]);
        let nv12 = [16, 16, 235, 235, 128, 128];
        assert_eq!(snapshot(frame(&nv12), 2, 2, 2, 0x23, BufferCrop::full(2, 2)).unwrap(),
            [0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255, 255, 255, 255, 255, 255]);
    }

    #[test]
    fn yuv_limited_range_uses_bt601_coefficients() {
        // Pure red in BT.601 limited range: Y=81, Cb=90, Cr=240.
        let yv12 = [81u8, 81, 81, 81, 240, 90];
        let rgb = snapshot(frame(&yv12), 2, 2, 2, 0x3231_5659, BufferCrop::full(2, 2)).unwrap();
        // Output is XRGB8888 bytes: [B, G, R, unused].
        assert!(rgb[2] > 240 && rgb[1] < 30 && rgb[0] < 30, "red was {rgb:?}");
    }

    #[test]
    fn yv12_crop_excludes_decoder_alignment_padding() {
        let (stride, height, visible) = (512usize, 240usize, 426usize);
        let luma = stride * height;
        let chroma = (stride / 2) * (height / 2);
        let mut bytes = vec![0u8; luma + 2 * chroma];
        for y in 0..height {
            bytes[y * stride..y * stride + visible].fill(81);
        }
        for y in 0..height / 2 {
            let row = y * (stride / 2);
            bytes[luma + row..luma + row + visible / 2].fill(240);
            bytes[luma + chroma + row..luma + chroma + row + visible / 2].fill(90);
        }
        let pixels = snapshot(frame(&bytes), 512, 240, 512, 0x3231_5659,
            BufferCrop::full(426, 240)).unwrap();
        assert_eq!(pixels.len(), visible * height * 4);
        assert!(pixels.chunks_exact(4).all(|pixel| pixel == [0, 0, 255, 255]));
    }

    #[test]
    fn rgb_nonzero_origin_and_changed_crops_use_the_same_allocation() {
        // Four RGBA pixels plus four padding bytes per allocation row.
        let mut bytes = vec![0xee; 20 * 3];
        for y in 0..3 {
            for x in 0..4 {
                bytes[y * 20 + x * 4..y * 20 + x * 4 + 4]
                    .copy_from_slice(&[x as u8, y as u8, 10, 255]);
            }
        }
        let fd = frame(&bytes);
        let crop = BufferCrop { left: 1, top: 1, right: 3, bottom: 3 };
        let first = snapshot(fd.try_clone().unwrap(), 4, 3, 20, 1, crop).unwrap();
        assert_eq!(first, [10, 1, 1, 255, 10, 1, 2, 255, 10, 2, 1, 255, 10, 2, 2, 255]);
        let second = snapshot(fd.try_clone().unwrap(), 4, 3, 20, 1,
            BufferCrop { left: 3, top: 0, right: 4, bottom: 1 }).unwrap();
        assert_eq!(second, [10, 0, 3, 255]);
        assert_eq!(snapshot(fd, 4, 3, 20, 1, crop).unwrap(), first,
            "an intervening crop must not change the allocation's metadata");
    }

    #[test]
    fn yuv_crop_keeps_allocation_plane_bases_and_absolute_chroma_coordinates() {
        // Allocation: 6x6, luma stride 8. Crop: 4x3 at odd origin (1,1).
        // Neither the luma plane base nor chroma row selection can be computed
        // from that smaller crop or from crop-relative x/y.
        let crop = BufferCrop { left: 1, top: 1, right: 5, bottom: 4 };
        let u = [90, 128, 240, 50, 200, 100, 160, 80, 220];
        let v = [240, 128, 90, 200, 50, 160, 80, 220, 100];
        let mut expected = Vec::new();
        for y in 1..4usize {
            for x in 1..5usize {
                let chroma = (y / 2) * 3 + x / 2;
                let (r, g, b) = yuv_to_rgb((40 + y * 10 + x) as u8, u[chroma], v[chroma]);
                expected.extend_from_slice(&[b, g, r, 255]);
            }
        }
        for format in [0x3231_5659, 0x23] {
            let mut bytes = vec![0xee; 48 + 24];
            for y in 0..6usize {
                for x in 0..6usize { bytes[y * 8 + x] = (40 + y * 10 + x) as u8; }
            }
            for y in 0..3usize {
                for x in 0..3usize {
                    let sample = y * 3 + x;
                    if format == 0x3231_5659 {
                        bytes[48 + y * 4 + x] = v[sample];
                        bytes[60 + y * 4 + x] = u[sample];
                    } else {
                        bytes[48 + y * 8 + x * 2] = u[sample];
                        bytes[48 + y * 8 + x * 2 + 1] = v[sample];
                    }
                }
            }
            let pixels = snapshot(frame(&bytes), 6, 6, 8, format, crop).unwrap();
            assert_eq!(pixels, expected, "format {format:#x} used crop dimensions as storage dimensions");
            let scaled = resize(pixels, 4, 3, 8, 6).unwrap();
            assert_eq!(scaled.len(), 8 * 6 * 4);
            assert_eq!(&scaled[..8], &[expected[0], expected[1], expected[2], 255,
                expected[0], expected[1], expected[2], 255]);
        }
        assert!(snapshot(frame(&[0; 24]), 6, 6, 8, 0x3231_5659, crop)
            .unwrap_err().contains("shorter"), "a crop does not shorten the allocation's plane offsets");
    }

    #[test]
    fn malformed_crops_are_rejected_instead_of_clamped_or_made_full_frame() {
        for crop in [
            BufferCrop { left: -1, top: 0, right: 4, bottom: 4 },
            BufferCrop { left: 0, top: -1, right: 4, bottom: 4 },
            BufferCrop { left: 4, top: 0, right: 4, bottom: 4 },
            BufferCrop { left: 0, top: 4, right: 4, bottom: 4 },
            BufferCrop { left: 3, top: 0, right: 2, bottom: 4 },
            BufferCrop { left: 0, top: 3, right: 4, bottom: 2 },
            BufferCrop::full(5, 4), BufferCrop::full(4, 5),
            BufferCrop { left: i32::MIN, top: 0, right: i32::MAX, bottom: 4 },
            BufferCrop::full(i32::MAX, i32::MAX),
        ] {
            assert!(snapshot(frame(&[0; 64]), 4, 4, 16, 1, crop).unwrap_err().contains("crop"),
                "accepted {crop:?}");
        }
    }

    #[test]
    fn invalid_layouts_are_rejected_before_reading() {
        for (w, h, stride, format) in [(0, 1, 4, 1), (2, 1, 2, 4), (1, 2, 4, 1),
            (1, 1, 4, 99), (8193, 1, u32::MAX, 1), (1, 2, u32::MAX, 1)] {
            assert!(snapshot(frame(&[0; 4]), w, h, stride, format, BufferCrop::full(w as i32, h as i32)).is_err());
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
