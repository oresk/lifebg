//! Cell colour lookup and damage-rect construction.
//!
//! The whole point of this program is that a Game of Life grid is *mostly
//! unchanged* between generations, so we can hand the compositor a short list
//! of dirty rectangles instead of a full-frame damage. This module turns a set
//! of changed cells into a compact list of `wl_surface.damage` rectangles.

/// A dirty rectangle in surface-local (logical) pixel coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// Colours are `0xAARRGGBB` as laid out in memory by `WL_SHM_FORMAT_ARGB8888`
/// on a little-endian machine.
pub type Rgb = u32;

#[derive(Clone, Copy, Debug)]
pub struct Palette {
    /// Colour of a dead cell.
    pub base: Rgb,
    /// Colour of a live cell. There is no ramp: a cell is on or off.
    pub alive: Rgb,
}

impl Default for Palette {
    fn default() -> Self {
        Palette { base: 0xff0b_0e13, alive: 0xff3d_7f6e }
    }
}

/// Map a cell's state onto the palette.
#[inline]
pub fn color_of(alive: bool, p: &Palette) -> Rgb {
    if alive {
        p.alive
    } else {
        p.base
    }
}

/// Accumulates changed cells into merged rectangles.
///
/// Cells must be offered in row-major order. Horizontally adjacent changes
/// collapse into one run; a run that sits directly below a run of identical
/// geometry extends that rectangle instead of adding a new one. This keeps the
/// damage list small enough that the compositor's region maths stays cheap.
pub struct DamageBuilder {
    rects: Vec<Rect>,
    /// Runs of the row currently being fed, as `(start_gx, end_gx_exclusive)`.
    runs: Vec<(usize, usize)>,
    /// Runs of the previous row that are still extendable, keyed by geometry.
    open: Vec<(usize, usize, usize)>, // (rect index, start_gx, end_gx)
    cur_row: Option<usize>,
    cell: usize,
    /// Grid origin in surface coordinates, so damage lines up with the outline.
    ox: i32,
    oy: i32,
}

impl DamageBuilder {
    pub fn new(cell: usize, ox: i32, oy: i32) -> Self {
        DamageBuilder {
            rects: Vec::with_capacity(64),
            runs: Vec::with_capacity(32),
            open: Vec::with_capacity(32),
            cur_row: None,
            cell,
            ox,
            oy,
        }
    }

    /// Record that grid cell `(gx, gy)` changed. Rows must be offered in
    /// increasing order.
    pub fn cell_changed(&mut self, gx: usize, gy: usize) {
        if self.cur_row == Some(gy) {
            if let Some(&(_, end)) = self.runs.last() {
                if end == gx {
                    let n = self.runs.len() - 1;
                    self.runs[n].1 = gx + 1;
                    return;
                }
            }
            self.runs.push((gx, gx + 1));
            return;
        }

        let prev = self.cur_row;
        self.flush_row();
        // A row with no changes at all breaks vertical continuity: nothing from
        // the previous row may be extended any more.
        if prev.is_none_or(|p| gy != p + 1) {
            self.open.clear();
        }
        self.cur_row = Some(gy);
        self.runs.push((gx, gx + 1));
    }

    /// Turn the pending runs of the row just fed into rectangles, merging them
    /// with extendable rectangles from the row above.
    fn flush_row(&mut self) {
        if self.runs.is_empty() {
            self.open.clear();
            return;
        }
        let gy = self.cur_row.expect("row flushed before first cell");
        // Damage covers whole grid slots, gap included, offset by the grid
        // origin. Deliberately a superset of what is painted: it absorbs the
        // rounding differences between logical and buffer coordinates, and the
        // few extra pixels of gap cost nothing.
        let y = (gy * self.cell) as i32 + self.oy;
        let size = self.cell as i32;

        let mut next_open = Vec::with_capacity(self.runs.len());
        for &(start, end) in self.runs.iter() {
            let w = ((end - start) * self.cell) as i32;
            let x = (start * self.cell) as i32 + self.ox;

            if let Some(pos) = self.open.iter().position(|&(_, s, e)| s == start && e == end) {
                let (idx, _, _) = self.open.remove(pos);
                self.rects[idx].h += self.cell as i32;
                next_open.push((idx, start, end));
                continue;
            }

            self.rects.push(Rect { x, y, w, h: size });
            next_open.push((self.rects.len() - 1, start, end));
        }
        self.open = next_open;
        self.runs.clear();
    }

    /// Finish and return the rectangle list. `None` means "damage the whole
    /// surface instead", which is the right call when either:
    ///
    /// - the merged area is a large fraction of the surface, so clipping to it
    ///   saves the compositor almost nothing, or
    /// - the list is longer than `rect_cap`, so building the region costs more
    ///   than the clipping saves.
    ///
    /// Both bounds are relative to the surface rather than absolute counts, so
    /// they mean the same thing at any resolution or cell size. Runs are
    /// disjoint and vertical merges only extend an existing rect, so the sum of
    /// rect areas is the union.
    pub fn finish(mut self, surface_area: u64, max_pct: usize, rect_cap: usize) -> Option<Vec<Rect>> {
        self.flush_row();
        self.rects.retain(|r| r.w > 0 && r.h > 0);
        if self.rects.len() > rect_cap {
            return None;
        }
        let area: u64 = self.rects.iter().map(|r| r.w.max(0) as u64 * r.h.max(0) as u64).sum();
        if surface_area > 0 && area.saturating_mul(100) >= surface_area.saturating_mul(max_pct as u64) {
            return None;
        }
        Some(self.rects)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed a set of `(gx, gy)` cells in the row-major order the builder
    /// requires, then finish with bounds loose enough not to interfere.
    fn build(cells: &[(usize, usize)], cell: usize) -> Option<Vec<Rect>> {
        let mut db = DamageBuilder::new(cell, 0, 0);
        let mut cells = cells.to_vec();
        cells.sort_by_key(|&(x, y)| (y, x));
        for &(x, y) in &cells {
            db.cell_changed(x, y);
        }
        db.finish(100_000_000, 55, 4096)
    }

    #[test]
    fn no_changes_is_an_empty_list_not_a_full_damage() {
        assert_eq!(build(&[], 10), Some(Vec::new()));
    }

    #[test]
    fn a_contiguous_run_collapses_to_one_rect() {
        let rects = build(&[(0, 0), (1, 0), (2, 0)], 10).unwrap();
        assert_eq!(rects, vec![Rect { x: 0, y: 0, w: 30, h: 10 }]);
    }

    #[test]
    fn a_hole_in_the_row_splits_the_run() {
        let rects = build(&[(0, 0), (1, 0), (3, 0)], 10).unwrap();
        assert_eq!(rects, vec![Rect { x: 0, y: 0, w: 20, h: 10 }, Rect { x: 30, y: 0, w: 10, h: 10 }]);
    }

    #[test]
    fn identical_runs_in_adjacent_rows_merge_vertically() {
        let rects = build(&[(1, 0), (2, 0), (1, 1), (2, 1)], 10).unwrap();
        assert_eq!(rects, vec![Rect { x: 10, y: 0, w: 20, h: 20 }]);
    }

    #[test]
    fn runs_must_match_exactly_to_merge_vertically() {
        // Row 1 is one cell narrower, so it cannot extend row 0's rect.
        let rects = build(&[(1, 0), (2, 0), (1, 1)], 10).unwrap();
        assert_eq!(rects.len(), 2, "mismatched runs must not merge");
    }

    #[test]
    fn a_skipped_row_breaks_vertical_continuity() {
        let rects = build(&[(0, 0), (0, 2)], 10).unwrap();
        assert_eq!(rects, vec![Rect { x: 0, y: 0, w: 10, h: 10 }, Rect { x: 0, y: 20, w: 10, h: 10 }]);
    }

    #[test]
    fn origin_offsets_every_rect() {
        // This is what keeps damage aligned when --outline moves the grid.
        let mut db = DamageBuilder::new(10, 7, 5);
        db.cell_changed(0, 0);
        db.cell_changed(1, 0);
        assert_eq!(db.finish(100_000_000, 55, 4096).unwrap(), vec![Rect { x: 7, y: 5, w: 20, h: 10 }]);
    }

    #[test]
    fn damage_over_the_area_budget_falls_back_to_full() {
        // 4 cells x 100px^2 = 400 of a 500px surface = 80% > 55%.
        let mut db = DamageBuilder::new(10, 0, 0);
        for c in 0..4 {
            db.cell_changed(c, 0);
        }
        assert_eq!(db.finish(500, 55, 4096), None);
        // Same list against a big enough surface is fine.
        let mut db = DamageBuilder::new(10, 0, 0);
        for c in 0..4 {
            db.cell_changed(c, 0);
        }
        assert!(db.finish(100_000, 55, 4096).is_some());
    }

    #[test]
    fn too_many_rects_falls_back_to_full() {
        let mut cells = Vec::new();
        for y in 0..20 {
            for x in 0..20 {
                // Every other cell, so nothing merges and we get many rects.
                if (x + y) % 2 == 0 {
                    cells.push((x, y));
                }
            }
        }
        let mut db = DamageBuilder::new(10, 0, 0);
        for (x, y) in cells {
            db.cell_changed(x, y);
        }
        assert_eq!(db.finish(100_000_000, 55, 10), None, "cap of 10 should be exceeded");
    }
}
