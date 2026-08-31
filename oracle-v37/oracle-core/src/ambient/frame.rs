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
    fn hamming_is_symmetric_and_zero_on_equality() {
        assert_eq!(hamming(0xDEAD_BEEF, 0xDEAD_BEEF), 0);
        assert_eq!(hamming(0, u64::MAX), 64);
        assert_eq!(hamming(0b1010, 0b0101), hamming(0b0101, 0b1010));
    }
}
