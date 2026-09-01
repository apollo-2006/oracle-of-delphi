//! Deciding whether a captured frame is worth a model call.
//!
//! The sampler runs on a timer, but a screen does not change on a timer. Most
//! captures are of something the assistant already looked at: the same document
//! with the cursor two lines lower, the same page with a different blink state.
//! Interpreting those costs a model call each and writes near-duplicate rows
//! that crowd out real memories.
//!
//! So each frame is reduced to a 64-bit average hash and compared against the
//! last one by Hamming distance. aHash is crude — it will call two different
//! all-white pages identical — but that is the correct bias here: the cost of
//! wrongly skipping a frame is that the *next* one catches it seconds later,
//! while the cost of wrongly interpreting one is a model call and a junk memory.

/// Side length of the hash grid. 8x8 = 64 bits, the standard aHash size.
const GRID: u32 = 8;

/// Decode a PNG and reduce it to an average hash.
///
/// Returns None for anything that will not decode — a truncated capture is not
/// worth a model call either, and this is the cheapest place to find out.
pub fn ahash_png(png: &[u8]) -> Option<u64> {
    let decoder = png::Decoder::new(png);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let bytes = &buf[..info.buffer_size()];

    let (w, h) = (info.width, info.height);
    if w == 0 || h == 0 {
        return None;
    }
    let channels = match info.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        // Indexed images arrive expanded only if a transform was requested;
        // rather than guess at a palette, decline.
        png::ColorType::Indexed => return None,
    };
    if info.bit_depth != png::BitDepth::Eight {
        return None;
    }

    // Average each cell of an 8x8 grid into a luminance value.
    let mut cells = [0f64; (GRID * GRID) as usize];
    let mut counts = [0u32; (GRID * GRID) as usize];
    for y in 0..h {
        let cy = (y * GRID / h).min(GRID - 1);
        for x in 0..w {
            let cx = (x * GRID / w).min(GRID - 1);
            let i = ((y as usize * w as usize) + x as usize) * channels;
            if i + channels > bytes.len() {
                continue;
            }
            let lum = match channels {
                1 | 2 => bytes[i] as f64,
                // Rec. 601 luma: a colour change that leaves brightness alone
                // is rarely a change in what the screen says.
                _ => {
                    0.299 * bytes[i] as f64
                        + 0.587 * bytes[i + 1] as f64
                        + 0.114 * bytes[i + 2] as f64
                }
            };
            let c = (cy * GRID + cx) as usize;
            cells[c] += lum;
            counts[c] += 1;
        }
    }

    let mut means = [0f64; (GRID * GRID) as usize];
    for i in 0..means.len() {
        means[i] = if counts[i] > 0 {
            cells[i] / counts[i] as f64
        } else {
            0.0
        };
    }
    let overall: f64 = means.iter().sum::<f64>() / means.len() as f64;

    let mut hash = 0u64;
    for (i, m) in means.iter().enumerate() {
        if *m >= overall {
            hash |= 1 << i;
        }
    }
    Some(hash)
}

/// Decode a PNG, and if it is wider than `max_width`, downscale and re-encode.
///
/// Returns `None` when the image already fits (nothing to do) and `Err` only
/// when it will not decode.
///
/// This is the caller half of `CaptureWindow`'s `max_width` contract. Backends
/// that can scale during capture honour the hint themselves — Windows does it
/// inside `StretchBlt`, for free. Backends that cannot return native size and
/// say so, and until this existed nothing resized them: on a Retina Mac,
/// `screencapture` hands back a frame at twice the requested point size, so a
/// 1440pt window arrived as a 2880px PNG and went to the vision model whole.
/// The symptom would not be an error — it is a slow, oversized request whose
/// image alone can exceed the context the model was launched with.
pub fn fit_to_width(png: &[u8], max_width: u32) -> anyhow::Result<Option<Vec<u8>>> {
    if max_width == 0 {
        return Ok(None);
    }
    let (rgba, w, h) = decode_rgba(png)?;
    if w <= max_width {
        return Ok(None);
    }
    let dst_w = max_width.max(1);
    let dst_h = ((h as u64 * dst_w as u64) / w as u64).max(1) as u32;

    // Box filter, matching the actd side: point-sampling a 4K screen down to
    // 1024px drops strokes off glyphs, and small text is the whole payload.
    let mut out = vec![0u8; (dst_w as usize) * (dst_h as usize) * 4];
    for y in 0..dst_h {
        let y0 = (y as u64 * h as u64 / dst_h as u64) as u32;
        let y1 = (((y as u64 + 1) * h as u64) / dst_h as u64).max(y0 as u64 + 1) as u32;
        for x in 0..dst_w {
            let x0 = (x as u64 * w as u64 / dst_w as u64) as u32;
            let x1 = (((x as u64 + 1) * w as u64) / dst_w as u64).max(x0 as u64 + 1) as u32;
            let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
            for sy in y0..y1.min(h) {
                for sx in x0..x1.min(w) {
                    let i = ((sy as usize * w as usize) + sx as usize) * 4;
                    if i + 3 >= rgba.len() {
                        continue;
                    }
                    r += rgba[i] as u32;
                    g += rgba[i + 1] as u32;
                    b += rgba[i + 2] as u32;
                    n += 1;
                }
            }
            let o = ((y as usize * dst_w as usize) + x as usize) * 4;
            if let (Some(r), Some(g), Some(b)) =
                (r.checked_div(n), g.checked_div(n), b.checked_div(n))
            {
                out[o] = r as u8;
                out[o + 1] = g as u8;
                out[o + 2] = b as u8;
                out[o + 3] = 255;
            }
        }
    }

    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, dst_w, dst_h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut wr = enc.write_header()?;
        wr.write_image_data(&out)?;
    }
    Ok(Some(buf))
}

/// Decode a PNG to RGBA8, expanding whatever colour type it arrived in.
fn decode_rgba(png_bytes: &[u8]) -> anyhow::Result<(Vec<u8>, u32, u32)> {
    let mut reader = png::Decoder::new(png_bytes).read_info()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf)?;
    if info.bit_depth != png::BitDepth::Eight {
        anyhow::bail!("unsupported PNG bit depth {:?}", info.bit_depth);
    }
    let ch = match info.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Indexed => anyhow::bail!("indexed PNGs are not supported"),
    };
    let (w, h) = (info.width, info.height);
    let src = &buf[..info.buffer_size()];
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for px in src.chunks_exact(ch) {
        match ch {
            1 => rgba.extend_from_slice(&[px[0], px[0], px[0], 255]),
            2 => rgba.extend_from_slice(&[px[0], px[0], px[0], px[1]]),
            3 => rgba.extend_from_slice(&[px[0], px[1], px[2], 255]),
            _ => rgba.extend_from_slice(&[px[0], px[1], px[2], px[3]]),
        }
    }
    Ok((rgba, w, h))
}

/// Number of differing bits between two hashes.
pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// Whether a frame is different enough from the last one to be worth
/// interpreting. `None` for `last` means there is no previous frame.
pub fn is_new_scene(last: Option<u64>, current: u64, threshold: u32) -> bool {
    match last {
        None => true,
        Some(prev) => hamming(prev, current) > threshold,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode an RGBA buffer to PNG so tests exercise the real decode path.
    fn png_of(w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 4]) -> Vec<u8> {
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                rgba.extend_from_slice(&f(x, y));
            }
        }
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, w, h);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut wr = enc.write_header().unwrap();
            wr.write_image_data(&rgba).unwrap();
        }
        out
    }

    #[test]
    fn the_same_image_hashes_identically() {
        let a = png_of(32, 32, |x, y| [(x * 8) as u8, (y * 8) as u8, 0, 255]);
        assert_eq!(ahash_png(&a), ahash_png(&a));
    }

    #[test]
    fn a_gradient_and_its_inverse_hash_differently() {
        let a = png_of(32, 32, |x, _| {
            [(x * 8) as u8, (x * 8) as u8, (x * 8) as u8, 255]
        });
        let b = png_of(32, 32, |x, _| {
            let v = 255 - (x * 8) as u8;
            [v, v, v, 255]
        });
        let (ha, hb) = (ahash_png(&a).unwrap(), ahash_png(&b).unwrap());
        assert!(hamming(ha, hb) > 8, "distance was {}", hamming(ha, hb));
    }

    #[test]
    fn a_tiny_local_change_is_not_a_new_scene() {
        // The case this exists for: a text cursor blinking, or one line
        // scrolling. Interpreting that would burn a model call per blink.
        let base = png_of(64, 64, |x, y| {
            let v = ((x / 8 + y / 8) % 2 * 200) as u8;
            [v, v, v, 255]
        });
        let nudged = png_of(64, 64, |x, y| {
            let mut v = ((x / 8 + y / 8) % 2 * 200) as u8;
            if x == 3 && y == 3 {
                v = 255;
            }
            [v, v, v, 255]
        });
        let (a, b) = (ahash_png(&base).unwrap(), ahash_png(&nudged).unwrap());
        assert!(!is_new_scene(Some(a), b, 6), "distance {}", hamming(a, b));
    }

    #[test]
    fn the_first_frame_is_always_a_new_scene() {
        assert!(is_new_scene(None, 0, 6));
        assert!(is_new_scene(None, u64::MAX, 6));
    }

    #[test]
    fn the_threshold_is_exclusive_so_zero_means_any_change_counts() {
        assert!(!is_new_scene(Some(0b1), 0b1, 0), "identical is never new");
        assert!(is_new_scene(Some(0b1), 0b11, 0));
    }

    #[test]
    fn a_greyscale_png_decodes() {
        // Not every backend produces RGBA; a grey capture must not be silently
        // treated as undecodable and re-interpreted every single cycle.
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, 16, 16);
            enc.set_color(png::ColorType::Grayscale);
            enc.set_depth(png::BitDepth::Eight);
            let mut wr = enc.write_header().unwrap();
            wr.write_image_data(&(0..256).map(|i| i as u8).collect::<Vec<_>>())
                .unwrap();
        }
        assert!(ahash_png(&out).is_some());
    }

    #[test]
    fn garbage_does_not_decode_and_does_not_panic() {
        assert_eq!(ahash_png(b"not a png"), None);
        assert_eq!(ahash_png(&[]), None);
        // A truncated PNG: valid magic, nothing behind it.
        assert_eq!(ahash_png(b"\x89PNG\r\n\x1a\n"), None);
    }

    #[test]
    fn an_image_within_the_limit_is_left_alone() {
        let src = png_of(64, 48, |x, _| [(x * 4) as u8, 0, 0, 255]);
        assert!(
            fit_to_width(&src, 1024).unwrap().is_none(),
            "no re-encode needed"
        );
    }

    #[test]
    fn an_oversized_image_is_downscaled_keeping_its_aspect_ratio() {
        // The Retina case: screencapture returns twice the requested points, so
        // the frame arrives far wider than the model was sized for.
        let src = png_of(2048, 1152, |x, y| {
            [(x % 256) as u8, (y % 256) as u8, 0, 255]
        });
        let out = fit_to_width(&src, 1024).unwrap().expect("must resize");
        let (_, w, h) = decode_rgba(&out).unwrap();
        assert_eq!(w, 1024);
        assert_eq!(h, 576, "16:9 preserved");
    }

    #[test]
    fn a_resized_frame_is_still_a_decodable_png() {
        let src = png_of(2048, 512, |x, _| [(x % 256) as u8, 0, 0, 255]);
        let out = fit_to_width(&src, 256).unwrap().unwrap();
        assert_eq!(&out[..8], b"\x89PNG\r\n\x1a\n");
        assert!(ahash_png(&out).is_some(), "it must still hash");
    }

    #[test]
    fn resizing_a_very_wide_strip_never_yields_zero_height() {
        let src = png_of(4000, 3, |x, _| [(x % 256) as u8, 0, 0, 255]);
        let out = fit_to_width(&src, 64).unwrap().unwrap();
        let (_, w, h) = decode_rgba(&out).unwrap();
        assert_eq!(w, 64);
        assert!(h >= 1);
    }

    #[test]
    fn a_zero_max_width_disables_resizing() {
        let src = png_of(2048, 512, |x, _| [(x % 256) as u8, 0, 0, 255]);
        assert!(fit_to_width(&src, 0).unwrap().is_none());
    }

    #[test]
    fn undecodable_input_is_an_error_not_a_silent_pass_through() {
        // Passing junk through unchanged would send it to the model as if it
        // were a frame.
        assert!(fit_to_width(b"not a png", 1024).is_err());
    }

    #[test]
    fn a_greyscale_png_expands_to_rgba_before_resizing() {
        let mut src = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut src, 128, 64);
            enc.set_color(png::ColorType::Grayscale);
            enc.set_depth(png::BitDepth::Eight);
            let mut wr = enc.write_header().unwrap();
            wr.write_image_data(&(0..128 * 64).map(|i| (i % 256) as u8).collect::<Vec<_>>())
                .unwrap();
        }
        let out = fit_to_width(&src, 32).unwrap().expect("must resize");
        let (rgba, w, _) = decode_rgba(&out).unwrap();
        assert_eq!(w, 32);
        assert_eq!(rgba.len() % 4, 0);
    }

    #[test]
    fn hamming_is_symmetric_and_zero_on_equality() {
        assert_eq!(hamming(0xDEAD_BEEF, 0xDEAD_BEEF), 0);
        assert_eq!(hamming(0, u64::MAX), 64);
        assert_eq!(hamming(0b1010, 0b0101), hamming(0b0101, 0b1010));
    }
}
