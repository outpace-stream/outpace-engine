//! Frame rendering: an animated Julia set drawn at half resolution, bilinearly upscaled, with the
//! logo composited on top and the result written straight into a planar YUV 4:2:0 (BT.709,
//! limited range) buffer. Every buffer is allocated once; a frame allocates nothing.

use crate::bounce::Tint;
use crate::life::Life;
use crate::logo::Sprite;
use std::f32::consts::{LN_2, TAU};

const MAX_ITER: u32 = 64;
const LUT_SIZE: usize = 1024;
/// Colour of points inside the set.
const INTERIOR: [f32; 3] = [9.0, 5.0, 20.0];

pub struct Overlay<'a> {
    pub life: &'a Life,
    pub sprite: &'a Sprite,
    /// Top-left of the visible logo, in output pixels.
    pub x: f32,
    pub y: f32,
    pub tint: Tint,
    /// 0..=1 corner-hit glow.
    pub flash: f32,
}

pub struct Renderer {
    width: usize,
    height: usize,
    bg_w: usize,
    bg_h: usize,
    bg: Vec<[f32; 3]>,
    /// Per output column / row: the two background samples and the weight of the second.
    cols: Vec<(u32, u32, f32)>,
    rows: Vec<(u32, u32, f32)>,
    lut: Vec<[f32; 3]>,
    /// Per output column / row: the life cell it falls in, or `u32::MAX` in the gap between
    /// cells.
    cell_cols: Vec<u32>,
    cell_rows: Vec<u32>,
    threads: usize,
}

/// Bytes in one YUV 4:2:0 frame.
pub fn frame_len(width: u32, height: u32) -> usize {
    let luma = width as usize * height as usize;
    luma + luma / 2
}

impl Renderer {
    /// `width` and `height` must be even; `cell` is the life grid's cell size in pixels.
    pub fn new(width: u32, height: u32, cell: usize, threads: usize) -> Self {
        assert!(
            width.is_multiple_of(2) && height.is_multiple_of(2),
            "frame size must be even"
        );
        let (width, height) = (width as usize, height as usize);
        let (bg_w, bg_h) = (width / 2, height / 2);
        Self {
            width,
            height,
            bg_w,
            bg_h,
            bg: vec![[0.0; 3]; bg_w * bg_h],
            cols: upscale_taps(width, bg_w),
            rows: upscale_taps(height, bg_h),
            lut: palette(),
            cell_cols: cell_taps(width, cell),
            cell_rows: cell_taps(height, cell),
            threads: threads.max(1),
        }
    }

    pub fn render(&mut self, t: f32, overlay: &Overlay, out: &mut [u8]) {
        assert_eq!(out.len(), self.width * self.height * 3 / 2);
        self.draw_background(t);
        self.compose(overlay, out);
    }

    fn draw_background(&mut self, t: f32) {
        let f = Fractal::at(t);
        let scale = f.half_height / (self.bg_h as f32 / 2.0);
        let (cx, cy, phase) = (f.cx, f.cy, f.phase);
        let (rs, rc) = (f.rotation.sin() * scale, f.rotation.cos() * scale);
        let (w, h) = (self.bg_w, self.bg_h);
        let lut = &self.lut;
        let band = h.div_ceil(self.threads);
        std::thread::scope(|s| {
            for (i, chunk) in self.bg.chunks_mut(band * w).enumerate() {
                s.spawn(move || {
                    for (r, row) in chunk.chunks_mut(w).enumerate() {
                        let py = (i * band + r) as f32 - h as f32 / 2.0;
                        for (x, px) in row.iter_mut().enumerate() {
                            let qx = x as f32 - w as f32 / 2.0;
                            let zx = qx * rc - py * rs;
                            let zy = qx * rs + py * rc;
                            *px = julia(zx, zy, cx, cy, phase, lut);
                        }
                    }
                });
            }
        });
    }

    fn compose(&self, ov: &Overlay, out: &mut [u8]) {
        let (w, h) = (self.width, self.height);
        let (luma, chroma) = out.split_at_mut(w * h);
        let (u_plane, v_plane) = chroma.split_at_mut(w * h / 4);
        let band = h.div_ceil(self.threads).next_multiple_of(2);
        let sprite_x = ov.x.round() as i64 - ov.sprite.margin as i64;
        let sprite_y = ov.y.round() as i64 - ov.sprite.margin as i64;
        std::thread::scope(|s| {
            let bands = luma
                .chunks_mut(band * w)
                .zip(u_plane.chunks_mut(band * w / 4))
                .zip(v_plane.chunks_mut(band * w / 4))
                .enumerate();
            for (i, ((ys, us), vs)) in bands {
                s.spawn(move || {
                    let mut quad = [[0.0f32; 3]; 4];
                    for pair in 0..ys.len() / (2 * w) {
                        let y = i * band + pair * 2;
                        for cx in 0..w / 2 {
                            let x = cx * 2;
                            for (k, px) in quad.iter_mut().enumerate() {
                                let (px_x, px_y) = (x + (k & 1), y + (k >> 1));
                                *px = self.pixel(px_x, px_y, ov, sprite_x, sprite_y);
                                let row = pair * 2 + (k >> 1);
                                ys[row * w + px_x] = luma_of(*px);
                            }
                            let avg = [0, 1, 2].map(|c| {
                                (quad[0][c] + quad[1][c] + quad[2][c] + quad[3][c]) * 0.25
                            });
                            let (cb, cr) = chroma_of(avg);
                            us[pair * (w / 2) + cx] = cb;
                            vs[pair * (w / 2) + cx] = cr;
                        }
                    }
                });
            }
        });
    }

    #[inline]
    fn pixel(&self, x: usize, y: usize, ov: &Overlay, sx: i64, sy: i64) -> [f32; 3] {
        let (x0, x1, wx) = self.cols[x];
        let (y0, y1, wy) = self.rows[y];
        let bw = self.bg_w;
        let at = |xx: u32, yy: u32| self.bg[yy as usize * bw + xx as usize];
        let (a, b, c, d) = (at(x0, y0), at(x1, y0), at(x0, y1), at(x1, y1));
        let mut rgb = [0.0; 3];
        for i in 0..3 {
            let top = a[i] + (b[i] - a[i]) * wx;
            let bottom = c[i] + (d[i] - c[i]) * wx;
            rgb[i] = top + (bottom - top) * wy;
        }

        let (cc, cr) = (self.cell_cols[x], self.cell_rows[y]);
        if cc != u32::MAX && cr != u32::MAX {
            let life = ov.life;
            let i = cr as usize * life.cols + cc as usize;
            let g = life.glow[i];
            if g > 0.003 {
                // Additive, so cells read as light over the fractal and trails fade into it.
                let col = life.color[i];
                for c in 0..3 {
                    rgb[c] = (rgb[c] + (col[c] * 0.75 + 40.0) * g * 0.6).min(255.0);
                }
            }
        }

        let (lx, ly) = (x as i64 - sx, y as i64 - sy);
        let sp = ov.sprite;
        if lx < 0 || ly < 0 || lx >= sp.stride as i64 || ly >= sp.rows as i64 {
            return rgb;
        }
        let i = (ly * sp.stride as i64 + lx) as usize;
        let shadow = sp.shadow[i];
        let alpha = sp.alpha[i];
        let tone = sp.tone[i];
        let (p, q) = (ov.tint.primary, ov.tint.secondary);
        for c in 0..3 {
            // Darken under the shadow; a corner hit turns it into a glow in the logo's colour.
            let mut v = rgb[c] * (1.0 - 0.6 * shadow * (1.0 - ov.flash));
            v += p[c] * shadow * ov.flash * 0.9;
            let mut logo = p[c] + (q[c] - p[c]) * tone;
            logo += (255.0 - logo) * ov.flash * 0.55;
            rgb[c] = v + (logo - v) * alpha;
        }
        rgb
    }
}

/// The fractal's animation state at scene time `t`, shared by the CPU and GPU renderers.
pub struct Fractal {
    /// Julia constant.
    pub cx: f32,
    pub cy: f32,
    /// Complex-plane distance from the frame centre to its top edge.
    pub half_height: f32,
    pub rotation: f32,
    /// Palette cycling offset, in palette turns.
    pub phase: f32,
}

impl Fractal {
    /// Slow drift through the classic |c| = 0.7885 family of Julia sets, with breathing zoom,
    /// gentle rotation and palette cycling. The periods are long and mutually incommensurate,
    /// so the picture does not visibly repeat.
    pub fn at(t: f32) -> Self {
        let angle = 0.9 + t * 0.041;
        Self {
            cx: 0.7885 * angle.cos(),
            cy: 0.7885 * angle.sin(),
            half_height: 1.18 + 0.16 * (t * 0.053).sin(),
            rotation: t * 0.019,
            phase: t * 0.021,
        }
    }
}

#[inline]
fn julia(mut zx: f32, mut zy: f32, cx: f32, cy: f32, phase: f32, lut: &[[f32; 3]]) -> [f32; 3] {
    let mut i = 0;
    let mut r2 = zx * zx + zy * zy;
    while i < MAX_ITER && r2 <= 256.0 {
        let nx = zx * zx - zy * zy + cx;
        zy = 2.0 * zx * zy + cy;
        zx = nx;
        r2 = zx * zx + zy * zy;
        i += 1;
    }
    if i == MAX_ITER {
        return INTERIOR;
    }
    // Continuous escape count, so colour bands are smooth instead of stepped.
    let mu = (i as f32 + 1.0 - (0.5 * r2.ln()).ln() / LN_2).max(0.0);
    let idx = ((mu * 0.045 + phase).fract() * LUT_SIZE as f32) as usize & (LUT_SIZE - 1);
    // Fade points that escape quickly so the set's filaments glow out of a dark field.
    let g = (mu / 16.0).min(1.0);
    let glow = 0.24 + 0.76 * g * g * (3.0 - 2.0 * g);
    let c = lut[idx];
    [c[0] * glow, c[1] * glow, c[2] * glow]
}

/// A cyclic cosine gradient (deep blue, violet, magenta, teal), dimmed so the logo stays the
/// brightest thing on screen.
pub fn palette() -> Vec<[f32; 3]> {
    const A: [f32; 3] = [0.45, 0.35, 0.55];
    const B: [f32; 3] = [0.45, 0.35, 0.45];
    const C: [f32; 3] = [1.0, 1.0, 1.0];
    const D: [f32; 3] = [0.62, 0.80, 0.95];
    (0..LUT_SIZE)
        .map(|i| {
            let t = i as f32 / LUT_SIZE as f32;
            [0, 1, 2].map(|c| {
                let v = A[c] + B[c] * (TAU * (C[c] * t + D[c])).cos();
                v.clamp(0.0, 1.0) * 255.0 * 0.85
            })
        })
        .collect()
}

/// Cell index for each pixel along one axis, with a gap between cells drawn as `u32::MAX`.
/// Gap in pixels drawn between life cells of size `cell`.
pub fn cell_gap(cell: usize) -> usize {
    (cell / 8).max(1)
}

fn cell_taps(len: usize, cell: usize) -> Vec<u32> {
    let gap = cell_gap(cell);
    (0..len)
        .map(|p| {
            let within = p % cell;
            if within < gap || within >= cell - gap {
                u32::MAX
            } else {
                (p / cell) as u32
            }
        })
        .collect()
}

/// Bilinear taps mapping `out` output pixels onto `src` source pixels, pixel centres aligned.
fn upscale_taps(out: usize, src: usize) -> Vec<(u32, u32, f32)> {
    let ratio = src as f32 / out as f32;
    (0..out)
        .map(|o| {
            let s = ((o as f32 + 0.5) * ratio - 0.5).clamp(0.0, (src - 1) as f32);
            let s0 = s.floor() as u32;
            let s1 = (s0 + 1).min(src as u32 - 1);
            (s0, s1, s - s0 as f32)
        })
        .collect()
}

// BT.709, limited ("TV") range.
const KR: f32 = 0.2126;
const KB: f32 = 0.0722;

#[inline]
fn luma_of(rgb: [f32; 3]) -> u8 {
    let y = KR * rgb[0] + (1.0 - KR - KB) * rgb[1] + KB * rgb[2];
    (16.0 + y * (219.0 / 255.0)).round().clamp(16.0, 235.0) as u8
}

#[inline]
fn chroma_of(rgb: [f32; 3]) -> (u8, u8) {
    let y = KR * rgb[0] + (1.0 - KR - KB) * rgb[1] + KB * rgb[2];
    let cb = 128.0 + (rgb[2] - y) / (2.0 * (1.0 - KB)) * (224.0 / 255.0);
    let cr = 128.0 + (rgb[0] - y) / (2.0 * (1.0 - KR)) * (224.0 / 255.0);
    let q = |v: f32| v.round().clamp(16.0, 240.0) as u8;
    (q(cb), q(cr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bt709_reference_points() {
        assert_eq!(luma_of([0.0; 3]), 16);
        assert_eq!(luma_of([255.0; 3]), 235);
        assert_eq!(chroma_of([128.0; 3]), (128, 128));
        assert_eq!(chroma_of([0.0, 0.0, 255.0]).0, 240);
        assert_eq!(chroma_of([255.0, 0.0, 0.0]).1, 240);
    }

    #[test]
    fn taps_stay_in_bounds() {
        for (a, b, w) in upscale_taps(1280, 640) {
            assert!(a < 640 && b < 640 && (0.0..1.0).contains(&w));
        }
    }

    #[test]
    fn frame_is_deterministic_and_draws_the_logo() {
        let sprite = Sprite::new(120).unwrap();
        let life = Life::new(640, 360, 30, 5, [255.0; 3]);
        let overlay = Overlay {
            life: &life,
            sprite: &sprite,
            x: 100.0,
            y: 80.0,
            tint: crate::bounce::BRAND,
            flash: 0.0,
        };
        let mut a = vec![0; frame_len(640, 360)];
        let mut b = a.clone();
        Renderer::new(640, 360, life.cell, 3).render(12.5, &overlay, &mut a);
        Renderer::new(640, 360, life.cell, 1).render(12.5, &overlay, &mut b);
        assert!(a == b, "thread count must not change the picture");
        // Somewhere inside the logo box must be the brand orange's bright luma.
        let inside = (80..80 + sprite.height as usize)
            .flat_map(|y| (100..100 + sprite.width as usize).map(move |x| y * 640 + x));
        assert!(inside.map(|i| a[i]).any(|y| y > 120));
    }
}
