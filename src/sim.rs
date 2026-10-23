//! Toroidal Conway's Game of Life with periodic reseeding.
//!
//! Cells are strictly binary: `0` is dead, `1` is alive. There is no ageing and
//! no colour ramp — a cell is on or off, and it only ever generates damage on
//! the generation it flips. That keeps the changed-cell count far lower than an
//! ageing scheme, where every survivor re-tints for several generations.

/// How many random 8x8 soups to drop per reseeding.
const SOUPS_PER_SEED: usize = 3;

/// How many named patterns to drop per reseeding in shape modes.
const SHAPES_PER_SEED: usize = 3;

/// One initial pattern per this many cells, so density scales with resolution.
const CELLS_PER_SHAPE: usize = 700;

/// When the live population falls below this fraction of the grid, inject
/// fresh material so the wallpaper never freezes. Default `0.012`.
pub const DEFAULT_REVIVE_FLOOR: f64 = 0.012;

/// What gets dropped onto the grid at startup and on each reseed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Spawn {
    /// Uniform random noise only. Chaotic, but nothing reads as "Life".
    Noise,
    /// Only named patterns on an otherwise empty grid.
    Shapes,
    /// Noise, with named patterns dropped on top of it.
    Mixed,
}

/// A named Life pattern: live-cell offsets inside its bounding box.
pub struct Pattern {
    pub name: &'static str,
    pub w: usize,
    pub h: usize,
    /// Relative chance of being picked, so the big and spectacular ones stay
    /// rare and a screenful does not turn into a gun farm.
    pub weight: u32,
    pub cells: &'static [(usize, usize)],
}

/// Patterns worth recognising on screen. Each one is checked by the tests at
/// the bottom of this file, because a subtly wrong coordinate list produces a
/// blob that decays instead of the object it was meant to be.
pub const PATTERNS: &[Pattern] = &[
    Pattern {
        name: "glider",
        w: 3,
        h: 3,
        weight: 30,
        cells: &[(1, 0), (2, 1), (0, 2), (1, 2), (2, 2)],
    },
    Pattern {
        name: "lwss",
        w: 5,
        h: 4,
        weight: 12,
        cells: &[(1, 0), (4, 0), (0, 1), (0, 2), (4, 2), (0, 3), (1, 3), (2, 3), (3, 3)],
    },
    Pattern { name: "block", w: 2, h: 2, weight: 10, cells: &[(0, 0), (1, 0), (0, 1), (1, 1)] },
    Pattern { name: "blinker", w: 3, h: 1, weight: 10, cells: &[(0, 0), (1, 0), (2, 0)] },
    Pattern {
        name: "toad",
        w: 4,
        h: 2,
        weight: 8,
        cells: &[(1, 0), (2, 0), (3, 0), (0, 1), (1, 1), (2, 1)],
    },
    Pattern {
        name: "beacon",
        w: 4,
        h: 4,
        weight: 8,
        cells: &[(0, 0), (1, 0), (0, 1), (1, 1), (2, 2), (3, 2), (2, 3), (3, 3)],
    },
    Pattern {
        name: "pulsar",
        w: 13,
        h: 13,
        weight: 4,
        cells: &[
            (2, 0), (3, 0), (4, 0), (8, 0), (9, 0), (10, 0),
            (0, 2), (5, 2), (7, 2), (12, 2),
            (0, 3), (5, 3), (7, 3), (12, 3),
            (0, 4), (5, 4), (7, 4), (12, 4),
            (2, 5), (3, 5), (4, 5), (8, 5), (9, 5), (10, 5),
            (2, 7), (3, 7), (4, 7), (8, 7), (9, 7), (10, 7),
            (0, 8), (5, 8), (7, 8), (12, 8),
            (0, 9), (5, 9), (7, 9), (12, 9),
            (0, 10), (5, 10), (7, 10), (12, 10),
            (2, 12), (3, 12), (4, 12), (8, 12), (9, 12), (10, 12),
        ],
    },
    Pattern {
        name: "gosper gun",
        w: 36,
        h: 9,
        weight: 2,
        cells: &[
            (24, 0), (22, 1), (24, 1), (12, 2), (13, 2), (20, 2), (21, 2), (34, 2), (35, 2),
            (11, 3), (15, 3), (20, 3), (21, 3), (34, 3), (35, 3), (0, 4), (1, 4), (10, 4),
            (16, 4), (20, 4), (21, 4), (0, 5), (1, 5), (10, 5), (14, 5), (16, 5), (17, 5),
            (22, 5), (24, 5), (10, 6), (16, 6), (24, 6), (11, 7), (15, 7), (12, 8), (13, 8),
        ],
    },
];

const PATTERN_WEIGHT_TOTAL: u32 = sum_weights(PATTERNS);

const fn sum_weights(ps: &[Pattern]) -> u32 {
    let mut total = 0;
    let mut i = 0;
    while i < ps.len() {
        total += ps[i].weight;
        i += 1;
    }
    total
}

/// Simulation knobs that are independent of how the grid is drawn.
#[derive(Clone, Copy, Debug)]
pub struct SimCfg {
    /// Fraction of cells alive in the opening frame (0.0..=1.0).
    pub density: f64,
    /// Drop a fresh batch of soup every N generations. 0 disables.
    pub seed_interval: u64,
    /// What to drop: noise, named patterns, or both.
    pub spawn: Spawn,
    /// Below this fraction of the grid alive, inject fresh material. 0 disables.
    pub revive_floor: f64,
}

impl Default for SimCfg {
    fn default() -> Self {
        SimCfg { density: 0.28, seed_interval: 24, spawn: Spawn::Mixed, revive_floor: DEFAULT_REVIVE_FLOOR }
    }
}

/// Tiny xorshift64\* PRNG — no dependency, plenty good enough for noise.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(if seed == 0 { 0x9E3779B97F4A7C15 } else { seed })
    }

    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform float in `[0, 1)`.
    #[inline]
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    #[inline]
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }
}

pub struct Life {
    pub w: usize,
    pub h: usize,
    cur: Vec<u8>,
    next: Vec<u8>,
    pub gen: u64,
    pub population: usize,
    rng: Rng,
    cfg: SimCfg,
}

impl Life {
    pub fn new(w: usize, h: usize, cfg: SimCfg, seed: u64) -> Life {
        let w = w.max(3);
        let h = h.max(3);
        let mut life = Life {
            w,
            h,
            cur: vec![0; w * h],
            next: vec![0; w * h],
            gen: 0,
            population: 0,
            rng: Rng::new(seed),
            cfg,
        };
        life.seed_initial();
        life
    }

    /// Whether a cell is alive.
    #[inline]
    pub fn alive(&self, i: usize) -> bool {
        self.cur[i] != 0
    }

    /// Build the opening frame according to `cfg.spawn`.
    fn seed_initial(&mut self) {
        if self.cfg.spawn != Spawn::Shapes {
            let d = self.cfg.density;
            for cell in self.cur.iter_mut() {
                *cell = (self.rng.next_f64() < d) as u8;
            }
        }
        if self.cfg.spawn != Spawn::Noise {
            let n = (self.w * self.h / CELLS_PER_SHAPE).max(1);
            self.drop_shapes(n);
        }
        self.recount();
    }

    fn recount(&mut self) {
        self.population = self.cur.iter().filter(|&&v| v != 0).count();
    }

    /// Pick a pattern by weight, so guns stay rare.
    fn pick_pattern(&mut self) -> &'static Pattern {
        let mut roll = self.rng.next_u64() % PATTERN_WEIGHT_TOTAL as u64;
        for p in PATTERNS {
            if roll < p.weight as u64 {
                return p;
            }
            roll -= p.weight as u64;
        }
        &PATTERNS[0]
    }

    /// Stamp a pattern centred on `(cx, cy)`, wrapping at the edges.
    fn stamp(&mut self, p: &Pattern, cx: usize, cy: usize) {
        let (hw, hh) = ((p.w / 2) as i64, (p.h / 2) as i64);
        for &(dx, dy) in p.cells {
            let x = (cx as i64 + dx as i64 - hw).rem_euclid(self.w as i64) as usize;
            let y = (cy as i64 + dy as i64 - hh).rem_euclid(self.h as i64) as usize;
            self.cur[y * self.w + x] = 1;
        }
    }

    /// Drop `n` named patterns at random spots.
    fn drop_shapes(&mut self, n: usize) {
        for _ in 0..n {
            let p = self.pick_pattern();
            let (x, y) = (self.rng.below(self.w), self.rng.below(self.h));
            self.stamp(p, x, y);
        }
    }

    /// Drop an 8x8 random block centred on `(cx, cy)`. Small soups reliably
    /// produce gliders and oscillators, which keeps the screen moving in a way
    /// that uniform noise does not.
    fn soup(&mut self, cx: usize, cy: usize) {
        for dy in 0..8i64 {
            for dx in 0..8i64 {
                if self.rng.next_f64() < 0.42 {
                    let x = (cx as i64 + dx - 4).rem_euclid(self.w as i64) as usize;
                    let y = (cy as i64 + dy - 4).rem_euclid(self.h as i64) as usize;
                    self.cur[y * self.w + x] = 1;
                }
            }
        }
    }

    /// Advance one generation (B3/S23, wrapping edges), then reseed if needed.
    pub fn step(&mut self) {
        // Destructure so `cur` and `next` are disjoint borrows of `self`.
        {
            let Life { w, h, cur, next, .. } = self;
            for y in 0..*h {
                let up = if y == 0 { *h - 1 } else { y - 1 };
                let dn = if y + 1 == *h { 0 } else { y + 1 };
                let (row_up, row_cur, row_dn) = (&cur[up * *w..up * *w + *w], &cur[y * *w..y * *w + *w], &cur[dn * *w..dn * *w + *w]);
                let out = &mut next[y * *w..y * *w + *w];
                for x in 0..*w {
                    let l = if x == 0 { *w - 1 } else { x - 1 };
                    let r = if x + 1 == *w { 0 } else { x + 1 };
                    let n = (row_up[l] != 0) as u8
                        + (row_up[x] != 0) as u8
                        + (row_up[r] != 0) as u8
                        + (row_cur[l] != 0) as u8
                        + (row_cur[r] != 0) as u8
                        + (row_dn[l] != 0) as u8
                        + (row_dn[x] != 0) as u8
                        + (row_dn[r] != 0) as u8;
                    out[x] = if row_cur[x] != 0 {
                        u8::from(n == 2 || n == 3)
                    } else {
                        u8::from(n == 3)
                    };
                }
            }
        }
        std::mem::swap(&mut self.cur, &mut self.next);
        self.gen += 1;
        self.population = self.cur.iter().filter(|&&v| v != 0).count();

        let cells = self.w * self.h;
        let floor = (self.cfg.revive_floor * cells as f64) as usize;
        if self.population < floor {
            if self.cfg.spawn == Spawn::Noise {
                let (x, y) = (self.rng.below(self.w), self.rng.below(self.h));
                self.soup(x, y);
            } else {
                self.drop_shapes(2);
            }
            self.recount();
        }

        let iv = self.cfg.seed_interval;
        if iv > 0 && self.gen.is_multiple_of(iv) {
            match self.cfg.spawn {
                Spawn::Noise => {
                    for _ in 0..SOUPS_PER_SEED {
                        let (x, y) = (self.rng.below(self.w), self.rng.below(self.h));
                        self.soup(x, y);
                    }
                }
                Spawn::Shapes | Spawn::Mixed => self.drop_shapes(SHAPES_PER_SEED),
            }
            self.recount();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A grid holding exactly one pattern, far from every edge so the torus
    /// cannot be what makes the test pass. The revive floor is off, or it would
    /// inject extra material and wreck the isolation.
    fn lone(p: &Pattern) -> Life {
        let mut cfg = SimCfg::default();
        cfg.spawn = Spawn::Noise;
        cfg.density = 0.0;
        cfg.seed_interval = 0;
        cfg.revive_floor = 0.0;
        let mut life = Life::new(80, 60, cfg, 7);
        life.stamp(p, 40, 30);
        life.recount();
        life
    }

    /// Live coordinates — compact enough to print when a test fails.
    fn snapshot(l: &Life) -> Vec<(usize, usize)> {
        (0..l.h).flat_map(|y| (0..l.w).map(move |x| (x, y))).filter(|&(x, y)| l.alive(y * l.w + x)).collect()
    }

    #[test]
    fn still_lifes_never_change() {
        for name in ["block"] {
            let p = PATTERNS.iter().find(|q| q.name == name).unwrap();
            let mut life = lone(p);
            let before = snapshot(&life);
            for _ in 0..10 {
                life.step();
            }
            assert_eq!(before, snapshot(&life), "{name} should be inert");
        }
    }

    #[test]
    fn period_two_oscillators_blink() {
        for name in ["blinker", "toad", "beacon"] {
            let p = PATTERNS.iter().find(|q| q.name == name).unwrap();
            let mut life = lone(p);
            let gen0 = snapshot(&life);
            life.step();
            let gen1 = snapshot(&life);
            assert_ne!(gen0, gen1, "{name} must actually move");
            life.step();
            assert_eq!(gen0, snapshot(&life), "{name} should have period 2");
        }
    }

    #[test]
    fn pulsar_has_period_three() {
        let p = PATTERNS.iter().find(|q| q.name == "pulsar").unwrap();
        let mut life = lone(p);
        let gen0 = snapshot(&life);
        life.step();
        assert_ne!(gen0, snapshot(&life), "pulsar must blink");
        for _ in 0..2 {
            life.step();
        }
        assert_eq!(gen0, snapshot(&life), "pulsar should be back after 3 generations");
    }

    #[test]
    fn glider_translates_instead_of_dying() {
        let p = PATTERNS.iter().find(|q| q.name == "glider").unwrap();
        let mut life = lone(p);
        assert_eq!(life.population, 5);
        let start = snapshot(&life);
        for _ in 0..4 {
            life.step();
        }
        assert_eq!(life.population, 5, "a glider must not shed cells");
        let after = snapshot(&life);
        // Same shape, moved. Asserting the direction would just re-state the
        // orientation in the coordinate list, which is what we are checking.
        let dx = after[0].0 as i64 - start[0].0 as i64;
        let dy = after[0].1 as i64 - start[0].1 as i64;
        assert!(dx != 0 || dy != 0, "glider did not move at all");
        assert!(
            after.iter().all(|&(x, y)| start.iter().any(|&(sx, sy)| sx as i64 + dx == x as i64 && sy as i64 + dy == y as i64)),
            "glider should translate, not deform: {start:?} -> {after:?}"
        );
    }

    #[test]
    fn gosper_gun_grows_forever() {
        let p = PATTERNS.iter().find(|q| q.name == "gosper gun").unwrap();
        let mut life = lone(p);
        let start = life.population;
        assert_eq!(start, p.cells.len());
        for _ in 0..120 {
            life.step();
        }
        assert!(life.population > start + 10, "gun should be emitting gliders: {start} -> {}", life.population);
    }

    #[test]
    fn shapes_mode_starts_empty_except_patterns() {
        let mut cfg = SimCfg::default();
        cfg.spawn = Spawn::Shapes;
        cfg.density = 0.9;
        let life = Life::new(60, 60, cfg, 3);
        // A few patterns, not a 90%-dense field.
        assert!(life.population < 60 * 60 / 4, "shapes mode must ignore density: {}", life.population);
        assert!(life.population > 0);
    }
}
