//! The logo sprite: decoded once at start-up, box-filtered down to its on-screen size and split
//! into the coverage, two-tone and drop-shadow masks the compositor needs. Nothing here runs per
//! frame.

/// Cropped to the mark's visible bounds, so the sprite edge is where the logo visibly touches
/// the frame edge.
const LOGO_PNG: &[u8] = include_bytes!("../assets/logo.png");

/// Green channel of the two source colours; the tone mask interpolates between them.
const ORANGE_G: f32 = 89.0;
const AMBER_G: f32 = 163.0;

pub struct Sprite {
    /// Visible logo size; this is what bounces.
    pub width: u32,
    pub height: u32,
    /// Border around the logo kept for the shadow, on every side.
    pub margin: u32,
    /// Mask size: `width + 2 * margin` by `height + 2 * margin`.
    pub stride: u32,
    pub rows: u32,
    /// Logo coverage, 0..=1.
    pub alpha: Vec<f32>,
    /// 0 = primary colour (ring), 1 = secondary colour (sweep).
    pub tone: Vec<f32>,
    /// Drop-shadow coverage, 0..=1, already offset.
    pub shadow: Vec<f32>,
}

impl Sprite {
    /// Builds the sprite `height` pixels tall, keeping the logo's aspect ratio.
    pub fn new(height: u32) -> Result<Self, String> {
        let (src_w, src_h, rgba) = decode()?;
        let height = height.clamp(8, src_h);
        let width = ((src_w as f32 * height as f32 / src_h as f32).round() as u32).max(1);
        let margin = (height / 14).max(2);
        let stride = width + 2 * margin;
        let rows = height + 2 * margin;

        let mut alpha = vec![0.0; (stride * rows) as usize];
        let mut tone = vec![0.0; (stride * rows) as usize];
        // Area-average every source pixel into the destination pixel it falls in. Colour is
        // weighted by coverage so the antialiased rim keeps the right tone.
        let mut weight = vec![0.0f32; (width * height) as usize];
        let mut cover = vec![0.0f32; (width * height) as usize];
        let mut toned = vec![0.0f32; (width * height) as usize];
        for sy in 0..src_h {
            let dy = (sy * height / src_h) as usize;
            for sx in 0..src_w {
                let dx = (sx * width / src_w) as usize;
                let p = &rgba[((sy * src_w + sx) * 4) as usize..][..4];
                let a = p[3] as f32 / 255.0;
                let t = ((p[1] as f32 - ORANGE_G) / (AMBER_G - ORANGE_G)).clamp(0.0, 1.0);
                let i = dy * width as usize + dx;
                weight[i] += 1.0;
                cover[i] += a;
                toned[i] += a * t;
            }
        }
        for y in 0..height {
            for x in 0..width {
                let i = (y * width + x) as usize;
                let o = ((y + margin) * stride + x + margin) as usize;
                if cover[i] > 0.0 {
                    alpha[o] = cover[i] / weight[i];
                    tone[o] = toned[i] / cover[i];
                }
            }
        }

        let shadow = drop_shadow(&alpha, stride, rows, margin);
        Ok(Self {
            width,
            height,
            margin,
            stride,
            rows,
            alpha,
            tone,
            shadow,
        })
    }
}

/// Coverage blurred by two box passes (close to a Gaussian) and shifted down-right.
fn drop_shadow(alpha: &[f32], w: u32, h: u32, margin: u32) -> Vec<f32> {
    let radius = (margin / 2).max(1) as i64;
    let shift = (margin / 2) as i64;
    let mut a = alpha.to_vec();
    let mut tmp = vec![0.0; a.len()];
    for _ in 0..2 {
        box_blur(&a, &mut tmp, w, h, radius, true);
        box_blur(&tmp, &mut a, w, h, radius, false);
    }
    let mut out = vec![0.0; a.len()];
    for y in 0..h as i64 {
        for x in 0..w as i64 {
            let (sx, sy) = (x - shift, y - shift);
            if sx >= 0 && sy >= 0 {
                out[(y * w as i64 + x) as usize] = a[(sy * w as i64 + sx) as usize];
            }
        }
    }
    out
}

fn box_blur(src: &[f32], dst: &mut [f32], w: u32, h: u32, r: i64, horizontal: bool) {
    let (w, h) = (w as i64, h as i64);
    let norm = 1.0 / (2 * r + 1) as f32;
    for y in 0..h {
        for x in 0..w {
            let mut sum = 0.0;
            for k in -r..=r {
                let (sx, sy) = if horizontal { (x + k, y) } else { (x, y + k) };
                if (0..w).contains(&sx) && (0..h).contains(&sy) {
                    sum += src[(sy * w + sx) as usize];
                }
            }
            dst[(y * w + x) as usize] = sum * norm;
        }
    }
}

fn decode() -> Result<(u32, u32, Vec<u8>), String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(LOGO_PNG));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|e| format!("logo: {e}"))?;
    let mut buf = vec![0; reader.output_buffer_size().ok_or("logo: image too large")?];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| format!("logo: {e}"))?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return Err(format!(
            "logo: expected 8-bit RGBA, got {:?} {:?}",
            info.color_type, info.bit_depth
        ));
    }
    buf.truncate(info.buffer_size());
    Ok((info.width, info.height, buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sprite_fills_its_bounds() {
        let s = Sprite::new(160).unwrap();
        assert_eq!(s.height, 160);
        assert!(s.width > s.height, "the mark is wider than tall");
        let (m, a, st) = (s.margin, &s.alpha, s.stride);
        let row = |y: u32| (m..m + s.width).map(move |x| a[(y * st + x) as usize]);
        let col = |x: u32| (m..m + s.height).map(move |y| a[(y * st + x) as usize]);
        // The asset is cropped to the mark, so every visible edge carries some coverage.
        assert!(row(m).any(|a| a > 0.1), "top edge");
        assert!(row(m + s.height - 1).any(|a| a > 0.1), "bottom edge");
        assert!(col(m).any(|a| a > 0.1), "left edge");
        assert!(col(m + s.width - 1).any(|a| a > 0.1), "right edge");
        // Both tones are present.
        let opaque = || (0..s.alpha.len()).filter(|&i| s.alpha[i] > 0.99);
        assert!(opaque().any(|i| s.tone[i] < 0.1));
        assert!(opaque().any(|i| s.tone[i] > 0.9));
    }
}
