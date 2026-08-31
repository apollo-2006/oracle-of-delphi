//! Shared pixel plumbing for the capture backends.
//!
//! Downscaling and PNG encoding are identical whatever produced the pixels, so
//! they live here rather than three times over. Everything in this file is pure
//! arithmetic on buffers — no OS calls — so it is unit-testable on any platform,
//! which matters because the backends that use it are not.

use base64::Engine;
use oracle_ipc::actd::CapturedImage;

use super::PalError;

/// Box-filter downscale of an RGBA buffer so its width does not exceed
/// `max_width`. Returns the input untouched when it already fits.
///
/// A box filter rather than nearest-neighbour: the consumer is a vision model
/// reading small text, and point-sampling a 1440p window down to 768px drops
/// whole strokes off glyphs. Averaging keeps them legible as grey.
pub fn downscale_rgba(src: &[u8], width: u32, height: u32, max_width: u32) -> (Vec<u8>, u32, u32) {
    if max_width == 0 || width <= max_width || width == 0 || height == 0 {
        return (src.to_vec(), width, height);
    }
    let dst_w = max_width.max(1);
    // Preserve aspect ratio; never produce a zero-height image from a wide one.
    let dst_h = ((height as u64 * dst_w as u64) / width as u64).max(1) as u32;

    let mut out = vec![0u8; (dst_w as usize) * (dst_h as usize) * 4];
    for y in 0..dst_h {
        // Source band for this destination row.
        let y0 = (y as u64 * height as u64 / dst_h as u64) as u32;
        let y1 = (((y as u64 + 1) * height as u64) / dst_h as u64).max(y0 as u64 + 1) as u32;
        for x in 0..dst_w {
            let x0 = (x as u64 * width as u64 / dst_w as u64) as u32;
            let x1 = (((x as u64 + 1) * width as u64) / dst_w as u64).max(x0 as u64 + 1) as u32;

            let (mut r, mut g, mut b, mut a, mut n) = (0u32, 0u32, 0u32, 0u32, 0u32);
            for sy in y0..y1.min(height) {
                for sx in x0..x1.min(width) {
                    let i = ((sy as usize * width as usize) + sx as usize) * 4;
                    if i + 3 >= src.len() {
                        continue;
                    }
                    r += src[i] as u32;
                    g += src[i + 1] as u32;
                    b += src[i + 2] as u32;
                    a += src[i + 3] as u32;
                    n += 1;
                }
            }
            let o = ((y as usize * dst_w as usize) + x as usize) * 4;
            if let (Some(r), Some(g), Some(b), Some(a)) = (
                r.checked_div(n),
                g.checked_div(n),
                b.checked_div(n),
                a.checked_div(n),
            ) {
                out[o] = r as u8;
                out[o + 1] = g as u8;
                out[o + 2] = b as u8;
                out[o + 3] = a as u8;
            }
        }
    }
    (out, dst_w, dst_h)
}

/// Destination size for a capture, honouring `max_width` and never upscaling.
///
/// Used by backends that can scale during capture (Windows `StretchBlt`), which
/// is much cheaper than capturing full-size and shrinking afterwards. It lives
/// here rather than in the Windows module so the arithmetic is testable on any
/// platform -- the aspect-ratio floor is exactly the kind of thing that is
/// obviously right and occasionally zero.
pub fn scaled_dims(src_w: i32, src_h: i32, max_width: u32) -> (i32, i32) {
    if max_width == 0 || src_w <= 0 || src_h <= 0 || src_w <= max_width as i32 {
        return (src_w, src_h);
    }
    let dst_w = max_width as i32;
    let dst_h = (((src_h as i64) * (dst_w as i64)) / (src_w as i64)).max(1) as i32;
    (dst_w, dst_h)
}

/// Encode an RGBA buffer as PNG.
pub fn encode_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, PalError> {
    let expected = (width as usize) * (height as usize) * 4;
    if rgba.len() < expected {
        return Err(PalError::Backend(format!(
            "capture buffer is {} bytes, expected {expected} for {width}x{height}",
            rgba.len()
        )));
    }
    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, width, height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        // Fast, not small: this image is decoded once by a local process
        // seconds later and then thrown away. Spending CPU to shrink it would
        // trade the user's responsiveness for bytes nobody stores.
        enc.set_compression(png::Compression::Fast);
        let mut writer = enc
            .write_header()
            .map_err(|e| PalError::Backend(format!("png header: {e}")))?;
        writer
            .write_image_data(&rgba[..expected])
            .map_err(|e| PalError::Backend(format!("png data: {e}")))?;
    }
    Ok(buf)
}

/// Downscale, encode and wrap into the wire type.
pub fn finish(
    window_id: u64,
    title: String,
    rgba: &[u8],
    width: u32,
    height: u32,
    max_width: u32,
) -> Result<CapturedImage, PalError> {
    let (scaled, w, h) = downscale_rgba(rgba, width, height, max_width);
    let png = encode_png(&scaled, w, h)?;
    Ok(CapturedImage {
        window_id,
        title,
        width: w,
        height: h,
        png_b64: base64::engine::general_purpose::STANDARD.encode(png),
    })
}

/// Wrap already-encoded PNG bytes (macOS `screencapture` hands these back).
pub fn finish_png(
    window_id: u64,
    title: String,
    png: Vec<u8>,
    width: u32,
    height: u32,
) -> CapturedImage {
    CapturedImage {
        window_id,
        title,
        width,
        height,
        png_b64: base64::engine::general_purpose::STANDARD.encode(png),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, colour: [u8; 4]) -> Vec<u8> {
        colour
            .iter()
            .cycle()
            .take((w * h * 4) as usize)
            .copied()
            .collect()
    }

    #[test]
    fn an_image_already_within_the_limit_is_untouched() {
        let src = solid(4, 4, [1, 2, 3, 255]);
        let (out, w, h) = downscale_rgba(&src, 4, 4, 100);
        assert_eq!((w, h), (4, 4));
        assert_eq!(out, src);
    }

    #[test]
    fn downscaling_preserves_aspect_ratio() {
        let src = solid(1920, 1080, [10, 20, 30, 255]);
        let (_, w, h) = downscale_rgba(&src, 1920, 1080, 768);
        assert_eq!(w, 768);
        assert_eq!(h, 432, "16:9 must stay 16:9");
    }

    #[test]
    fn a_very_wide_image_never_collapses_to_zero_height() {
        // An ultrawide strip scaled hard: integer division would floor the
        // height to 0 and produce a zero-byte image the encoder would reject.
        let src = solid(2000, 3, [255, 0, 0, 255]);
        let (out, w, h) = downscale_rgba(&src, 2000, 3, 10);
        assert_eq!(w, 10);
        assert!(h >= 1, "height floored to {h}");
        assert_eq!(out.len(), (w * h * 4) as usize);
    }

    #[test]
    fn averaging_a_solid_colour_reproduces_it() {
        let src = solid(64, 64, [200, 100, 50, 255]);
        let (out, w, h) = downscale_rgba(&src, 64, 64, 8);
        assert_eq!((w, h), (8, 8));
        // Every destination pixel averages a block of one colour, so it must
        // come back exactly -- this catches indexing bugs that would blend in
        // neighbouring rows or read past the row stride.
        for px in out.chunks(4) {
            assert_eq!(px, [200, 100, 50, 255]);
        }
    }

    #[test]
    fn a_zero_max_width_disables_scaling_rather_than_dividing_by_zero() {
        let src = solid(8, 8, [1, 1, 1, 255]);
        let (_, w, h) = downscale_rgba(&src, 8, 8, 0);
        assert_eq!((w, h), (8, 8));
    }

    #[test]
    fn scaled_dims_leaves_a_small_window_alone() {
        assert_eq!(scaled_dims(800, 600, 1024), (800, 600));
        assert_eq!(
            scaled_dims(1024, 768, 1024),
            (1024, 768),
            "equal is not over"
        );
    }

    #[test]
    fn scaled_dims_shrinks_and_keeps_the_ratio() {
        assert_eq!(scaled_dims(3840, 2160, 1024), (1024, 576));
        assert_eq!(scaled_dims(1920, 1080, 768), (768, 432));
    }

    #[test]
    fn scaled_dims_never_returns_a_zero_dimension() {
        // An ultrawide or a one-pixel-tall strip: integer division floors to
        // zero, and a zero-height DIB is a GDI failure at best.
        assert_eq!(scaled_dims(4000, 1, 100).1, 1);
        assert!(scaled_dims(10000, 3, 64).1 >= 1);
    }

    #[test]
    fn scaled_dims_ignores_nonsense_input_rather_than_dividing_by_zero() {
        assert_eq!(scaled_dims(0, 0, 512), (0, 0));
        assert_eq!(scaled_dims(-5, -5, 512), (-5, -5));
        assert_eq!(
            scaled_dims(2000, 1000, 0),
            (2000, 1000),
            "0 disables scaling"
        );
    }

    #[test]
    fn encoding_produces_a_real_png() {
        let src = solid(8, 8, [255, 255, 255, 255]);
        let png = encode_png(&src, 8, 8).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "PNG magic");
    }

    #[test]
    fn a_short_buffer_is_an_error_not_a_panic() {
        // A backend that miscomputed its stride must fail loudly here rather
        // than index out of bounds inside the encoder.
        let err = encode_png(&[0, 0, 0, 255], 8, 8).unwrap_err();
        assert!(format!("{err}").contains("expected"), "{err}");
    }

    #[test]
    fn finish_round_trips_through_base64() {
        let src = solid(4, 4, [7, 8, 9, 255]);
        let img = finish(42, "a window".into(), &src, 4, 4, 100).unwrap();
        assert_eq!(img.window_id, 42);
        assert_eq!((img.width, img.height), (4, 4));
        let raw = base64::engine::general_purpose::STANDARD
            .decode(&img.png_b64)
            .unwrap();
        assert_eq!(&raw[..8], b"\x89PNG\r\n\x1a\n");
    }
}
