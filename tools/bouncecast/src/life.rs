//! Conway's Game of Life layered between the fractal and the logo. The grid wraps at the edges
//! (a torus). Each live cell carries the colour the logo had when the cell was born, and dead
//! cells fade out over about a second, leaving trails. Logo bounces splash new cells at the
//! contact point, gliders are fired in from the edges now and then, and a sparse grid is
//! reseeded, so the board never dies out or freezes for long.

pub struct Life {
    pub cols: usize,
    pub rows: usize,
    /// Cell edge in output pixels.
    pub cell: usize,
    alive: Vec<bool>,
    next: Vec<bool>,
    /// Rendered brightness, 0..=1. Rises quickly when born, decays after death.
    pub glow: Vec<f32>,
    /// Colour each cell was born with, 0..=255 RGB.
    pub color: Vec<[f32; 3]>,
    rng: u64,
    frames: u64,
    /// Frames between generations.
    pub step_every: u64,
    fps: u32,
}

/// Fraction of the grid below which a fresh patch of soup is dropped in.
const SPARSE: f32 = 0.035;
/// Per-frame brightness retention of a dead cell at 30 fps.
const FADE_30: f32 = 0.955;

impl Life {
    pub fn new(width: u32, height: u32, fps: u32, seed: u64, color: [f32; 3]) -> Self {
        let cell = (height as usize / 60).max(4);
        let cols = width as usize / cell + 1;
        let rows = height as usize / cell + 1;
        let n = cols * rows;
        let mut life = Self {
            cols,
            rows,
            cell,
            alive: vec![false; n],
            next: vec![false; n],
            glow: vec![0.0; n],
            color: vec![color; n],
            rng: seed.rotate_left(17) | 1,
            frames: 0,
            // About six generations per second regardless of frame rate.
            step_every: (fps as u64 / 6).max(1),
            fps,
        };
        for _ in 0..6 {
            life.soup(color);
        }
        life
    }

    pub fn population(&self) -> usize {
        self.alive.iter().filter(|&&a| a).count()
    }

    /// Advances one video frame: maybe a generation, maybe a glider, and the fade animation.
    pub fn tick(&mut self, color: [f32; 3]) {
        self.frames += 1;
        if self.frames.is_multiple_of(self.step_every) {
            self.generation(color);
        }
        // A glider from a random edge every ~3 s.
        if self.frames.is_multiple_of(self.fps as u64 * 3) {
            self.glider(color);
        }
        if self.frames.is_multiple_of(self.fps as u64)
            && (self.population() as f32) < SPARSE * self.alive.len() as f32
        {
            self.soup(color);
        }
        let fade = FADE_30.powf(30.0 / self.fps as f32);
        for (g, &a) in self.glow.iter_mut().zip(&self.alive) {
            if a {
                *g += (1.0 - *g) * 0.4;
            } else {
                *g *= fade;
            }
        }
    }

    fn generation(&mut self, color: [f32; 3]) {
        let (w, h) = (self.cols, self.rows);
        for y in 0..h {
            let (up, down) = ((y + h - 1) % h, (y + 1) % h);
            for x in 0..w {
                let (left, right) = ((x + w - 1) % w, (x + 1) % w);
                let n = [
                    (left, up),
                    (x, up),
                    (right, up),
                    (left, y),
                    (right, y),
                    (left, down),
                    (x, down),
                    (right, down),
                ]
                .iter()
                .filter(|&&(cx, cy)| self.alive[cy * w + cx])
                .count();
                let i = y * w + x;
                let was = self.alive[i];
                let now = n == 3 || (was && n == 2);
                if now && !was {
                    self.color[i] = color;
                }
                self.next[i] = now;
            }
        }
        std::mem::swap(&mut self.alive, &mut self.next);
    }

    fn set(&mut self, x: i64, y: i64, color: [f32; 3]) {
        let x = x.rem_euclid(self.cols as i64) as usize;
        let y = y.rem_euclid(self.rows as i64) as usize;
        let i = y * self.cols + x;
        if !self.alive[i] {
            self.color[i] = color;
        }
        self.alive[i] = true;
    }

    /// A random 40%-density patch somewhere on the board.
    fn soup(&mut self, color: [f32; 3]) {
        let (pw, ph) = (16, 12);
        let ox = (self.rand() % self.cols as u64) as i64;
        let oy = (self.rand() % self.rows as u64) as i64;
        for y in 0..ph {
            for x in 0..pw {
                if self.rand() % 10 < 4 {
                    self.set(ox + x, oy + y, color);
                }
            }
        }
    }

    /// Bursts random cells around a point given in output pixels, e.g. where the logo hit.
    pub fn splash(&mut self, px: f32, py: f32, color: [f32; 3]) {
        let cx = (px / self.cell as f32) as i64;
        let cy = (py / self.cell as f32) as i64;
        let r = 5i64;
        for dy in -r..=r {
            for dx in -r..=r {
                let inside = dx * dx + dy * dy <= r * r;
                let (x, y) = (cx + dx, cy + dy);
                let on_board =
                    (0..self.cols as i64).contains(&x) && (0..self.rows as i64).contains(&y);
                if inside && on_board && self.rand().is_multiple_of(2) {
                    self.set(x, y, color);
                }
            }
        }
    }

    /// A glider entering from a random edge, headed into the board.
    fn glider(&mut self, color: [f32; 3]) {
        // Canonical glider travelling +x, +y; mirrored per axis for the other diagonals.
        const SHAPE: [(i64, i64); 5] = [(1, 0), (2, 1), (0, 2), (1, 2), (2, 2)];
        let r = self.rand();
        let (fx, fy) = (r & 1 == 0, r & 2 == 0);
        let (w, h) = (self.cols as i64, self.rows as i64);
        let along_top = r & 4 == 0;
        let pos = self.rand() as i64;
        let (ox, oy) = if along_top {
            (pos.rem_euclid(w), if fy { 0 } else { h - 3 })
        } else {
            (if fx { 0 } else { w - 3 }, pos.rem_euclid(h))
        };
        for (dx, dy) in SHAPE {
            let x = if fx { dx } else { 2 - dx };
            let y = if fy { dy } else { 2 - dy };
            self.set(ox + x, oy + y, color);
        }
    }

    fn rand(&mut self) -> u64 {
        let s = &mut self.rng;
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        *s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty(cols: usize, rows: usize) -> Life {
        let mut l = Life::new(64, 64, 30, 1, [255.0; 3]);
        l.cols = cols;
        l.rows = rows;
        l.alive = vec![false; cols * rows];
        l.next = l.alive.clone();
        l.glow = vec![0.0; cols * rows];
        l.color = vec![[0.0; 3]; cols * rows];
        l
    }

    #[test]
    fn blinker_oscillates_and_wraps() {
        let mut l = empty(5, 5);
        // Horizontal blinker straddling the left/right seam.
        for x in [4, 0, 1] {
            l.set(x, 2, [1.0; 3]);
        }
        l.generation([2.0; 3]);
        let col0: Vec<_> = (0..5).map(|y| l.alive[y * 5]).collect();
        assert_eq!(col0, [false, true, true, true, false]);
        assert_eq!(l.population(), 3);
        // Newly born cells take the current colour; the survivor keeps its own.
        assert_eq!(l.color[5], [2.0; 3]);
        assert_eq!(l.color[2 * 5], [1.0; 3]);
        l.generation([3.0; 3]);
        assert!(l.alive[2 * 5 + 4] && l.alive[2 * 5] && l.alive[2 * 5 + 1]);
    }

    #[test]
    fn never_dies_out() {
        let mut l = Life::new(1920, 1080, 30, 99, [255.0; 3]);
        let mut min = usize::MAX;
        for f in 0..30 * 300 {
            l.tick([255.0; 3]);
            if f > 30 * 10 {
                min = min.min(l.population());
            }
        }
        assert!(min > 0, "board died");
    }
}
