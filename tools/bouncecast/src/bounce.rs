//! Logo motion and colour: constant-speed travel, mirror reflection off the frame edges and a
//! new colour on every bounce. Advanced once per frame with a fixed step, so a run is fully
//! determined by its seed and frame count, never by wall-clock jitter.

/// Two-tone logo colour in linear-ish 0..=255 sRGB. `primary` paints the ring (the original
/// orange) and `secondary` the sweep (the original amber).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tint {
    pub primary: [f32; 3],
    pub secondary: [f32; 3],
}

/// The logo's own palette, shown until the first bounce.
pub const BRAND: Tint = Tint {
    primary: [253.0, 89.0, 45.0],
    secondary: [253.0, 163.0, 32.0],
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    None,
    Edge,
    /// Both axes reflected in the same step (or one did while the other sat on its wall).
    Corner,
}

pub struct Bouncer {
    pub x: f32,
    pub y: f32,
    vx: f32,
    vy: f32,
    max_x: f32,
    max_y: f32,
    hue: f32,
    pub tint: Tint,
    pub bounces: u64,
    pub corners: u64,
    rng: u64,
}

/// Distance in pixels from the other wall that still counts a single-axis bounce as a corner.
const CORNER_SLACK: f32 = 3.0;

impl Bouncer {
    /// `field` is the frame size and `sprite` the logo's visible size, both in pixels. `speed`
    /// is in pixels per second along each axis.
    pub fn new(field: (u32, u32), sprite: (u32, u32), speed: f32, seed: u64) -> Self {
        let mut rng = seed | 1;
        let max_x = field.0.saturating_sub(sprite.0) as f32;
        let max_y = field.1.saturating_sub(sprite.1) as f32;
        let x = unit(&mut rng) * max_x;
        let y = unit(&mut rng) * max_y;
        let sx = if next(&mut rng) & 1 == 0 { 1.0 } else { -1.0 };
        let sy = if next(&mut rng) & 1 == 0 { 1.0 } else { -1.0 };
        // Unequal axis speeds keep the path from settling into a short loop.
        Self {
            x,
            y,
            vx: sx * speed,
            vy: sy * speed * 0.77,
            max_x,
            max_y,
            hue: 14.0,
            tint: BRAND,
            bounces: 0,
            corners: 0,
            rng,
        }
    }

    pub fn step(&mut self, dt: f32) -> Hit {
        self.x += self.vx * dt;
        self.y += self.vy * dt;
        let hit_x = reflect(&mut self.x, &mut self.vx, self.max_x);
        let hit_y = reflect(&mut self.y, &mut self.vy, self.max_y);
        if !(hit_x || hit_y) {
            return Hit::None;
        }
        let near = |p: f32, max: f32| p <= CORNER_SLACK || p >= max - CORNER_SLACK;
        let corner = (hit_x && hit_y)
            || (hit_x && near(self.y, self.max_y))
            || (hit_y && near(self.x, self.max_x));
        self.bounces += 1;
        self.recolor();
        if corner {
            self.corners += 1;
            Hit::Corner
        } else {
            Hit::Edge
        }
    }

    /// Jump at least 60 degrees round the colour wheel so consecutive colours never look alike.
    fn recolor(&mut self) {
        self.hue = (self.hue + 60.0 + unit(&mut self.rng) * 240.0) % 360.0;
        self.tint = Tint {
            primary: hsv(self.hue, 0.80, 1.0),
            secondary: hsv(self.hue + 26.0, 0.85, 1.0),
        };
    }
}

fn reflect(p: &mut f32, v: &mut f32, max: f32) -> bool {
    if *p < 0.0 {
        *p = (-*p).min(max);
        *v = v.abs();
        true
    } else if *p > max {
        *p = (2.0 * max - *p).max(0.0);
        *v = -v.abs();
        true
    } else {
        false
    }
}

/// HSV (hue in degrees) to 0..=255 RGB.
pub fn hsv(h: f32, s: f32, v: f32) -> [f32; 3] {
    let h = h.rem_euclid(360.0) / 60.0;
    let c = v * s;
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    [(r + m) * 255.0, (g + m) * 255.0, (b + m) * 255.0]
}

/// xorshift64: tiny, deterministic, good enough for picking colours and start positions.
fn next(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

fn unit(s: &mut u64) -> f32 {
    (next(s) >> 40) as f32 / (1u64 << 24) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stays_inside_and_recolors_on_every_bounce() {
        let mut b = Bouncer::new((1280, 720), (200, 166), 180.0, 42);
        let mut last = b.tint;
        let mut hits = 0;
        for _ in 0..30 * 600 {
            let hit = b.step(1.0 / 30.0);
            assert!((0.0..=b.max_x).contains(&b.x), "x out of range: {}", b.x);
            assert!((0.0..=b.max_y).contains(&b.y), "y out of range: {}", b.y);
            if hit == Hit::None {
                assert_eq!(b.tint, last);
            } else {
                hits += 1;
                assert_ne!(b.tint, last, "bounce must change colour");
                last = b.tint;
            }
        }
        assert_eq!(hits, b.bounces);
        assert!(hits > 100, "ten minutes should bounce often, got {hits}");
    }

    #[test]
    fn same_seed_same_path() {
        let mut a = Bouncer::new((1280, 720), (200, 166), 180.0, 7);
        let mut b = Bouncer::new((1280, 720), (200, 166), 180.0, 7);
        for _ in 0..5000 {
            assert_eq!(a.step(1.0 / 30.0), b.step(1.0 / 30.0));
        }
        assert_eq!((a.x, a.y, a.tint), (b.x, b.y, b.tint));
    }

    #[test]
    fn exact_corner_is_reported() {
        let mut b = Bouncer::new((300, 300), (100, 100), 100.0, 1);
        b.x = 199.0;
        b.y = 199.0;
        b.vx = 60.0;
        b.vy = 60.0;
        assert_eq!(b.step(1.0 / 30.0), Hit::Corner);
        assert_eq!(b.corners, 1);
    }

    #[test]
    fn hsv_primaries() {
        assert_eq!(hsv(0.0, 1.0, 1.0), [255.0, 0.0, 0.0]);
        assert_eq!(hsv(120.0, 1.0, 1.0), [0.0, 255.0, 0.0]);
        assert_eq!(hsv(240.0, 1.0, 1.0), [0.0, 0.0, 255.0]);
    }
}
