// SPDX-License-Identifier: Apache-2.0

//! Grid-view layout mathematics and selection state. No GPU, no window — fully unit-testable.
//!
//! The grid displays thumbnails in a scrollable tile grid. Each cell is CELL_W × CELL_H pixels;
//! the selection is shown by drawing a slightly larger colored quad behind the thumbnail.

/// Thumbnail cell dimensions in logical pixels. Actual thumbnails may be smaller (portrait/square
/// images); they are centered within the cell.
pub const CELL_W: f32 = 168.0; // THUMB_W + 8px padding
pub const CELL_H: f32 = 128.0; // THUMB_H + 8px padding
/// Gap between cells.
pub const GAP: f32 = 8.0;
/// Margin from window edge.
pub const MARGIN: f32 = 14.0;
/// Selection ring outset (pixels on each side beyond the cell).
pub const SEL_OUTSET: f32 = 3.0;

/// Grid selection and scroll state.
#[derive(Debug, Clone)]
pub struct GridState {
    /// Currently selected tile index (into the playlist).
    pub sel: usize,
    /// Vertical scroll offset in physical pixels.
    pub scroll_y: f32,
}

impl GridState {
    pub fn new(sel: usize) -> Self {
        GridState { sel, scroll_y: 0.0 }
    }

    /// Number of columns that fit in `win_w`.
    pub fn col_count(win_w: f32) -> usize {
        ((win_w - MARGIN * 2.0 + GAP) / (CELL_W + GAP))
            .floor()
            .max(1.0) as usize
    }

    /// Total grid height (all rows), not accounting for scroll.
    pub fn total_height(n: usize, win_w: f32) -> f32 {
        let cols = Self::col_count(win_w);
        let rows = n.div_ceil(cols);
        MARGIN * 2.0 + rows as f32 * (CELL_H + GAP) - GAP
    }

    /// Top-left corner of cell `i` in screen space (y-down, with scroll applied).
    pub fn cell_origin(&self, i: usize, win_w: f32) -> (f32, f32) {
        let cols = Self::col_count(win_w);
        let col = i % cols;
        let row = i / cols;
        let x = MARGIN + col as f32 * (CELL_W + GAP);
        let y = MARGIN + row as f32 * (CELL_H + GAP) - self.scroll_y;
        (x, y)
    }

    /// Whether cell `i` is at least partially visible in a window of height `win_h`.
    pub fn is_visible(&self, i: usize, win_w: f32, win_h: f32) -> bool {
        let (_, y) = self.cell_origin(i, win_w);
        y + CELL_H > 0.0 && y < win_h
    }

    /// Adjust `scroll_y` so the selected tile is fully visible.
    pub fn scroll_to_sel(&mut self, n: usize, win_w: f32, win_h: f32) {
        if n == 0 {
            return;
        }
        let cols = Self::col_count(win_w);
        let row = self.sel / cols;
        let abs_top = MARGIN + row as f32 * (CELL_H + GAP);
        let abs_bot = abs_top + CELL_H;
        let screen_top = abs_top - self.scroll_y;
        let screen_bot = abs_bot - self.scroll_y;
        if screen_top < MARGIN {
            self.scroll_y = abs_top - MARGIN;
        } else if screen_bot > win_h - MARGIN {
            self.scroll_y = abs_bot - (win_h - MARGIN);
        }
        let max_scroll = (Self::total_height(n, win_w) - win_h).max(0.0);
        self.scroll_y = self.scroll_y.clamp(0.0, max_scroll);
    }

    /// Move selection by `(dc, dr)` columns/rows. Returns `true` if the selection changed.
    pub fn move_sel(&mut self, dc: isize, dr: isize, n: usize, win_w: f32) -> bool {
        if n == 0 {
            return false;
        }
        let cols = Self::col_count(win_w) as isize;
        let new_sel = (self.sel as isize + dc + dr * cols).clamp(0, n as isize - 1) as usize;
        let changed = new_sel != self.sel;
        self.sel = new_sel;
        changed
    }

    /// Which tile is under screen position `(mx, my)`, or `None` if outside the grid.
    pub fn hit_test(&self, mx: f32, my: f32, win_w: f32, n: usize) -> Option<usize> {
        let cols = Self::col_count(win_w);
        let mx_rel = mx - MARGIN;
        let my_rel = my + self.scroll_y - MARGIN;
        if mx_rel < 0.0 || my_rel < 0.0 {
            return None;
        }
        let col = (mx_rel / (CELL_W + GAP)) as usize;
        let row = (my_rel / (CELL_H + GAP)) as usize;
        // Click must be within the cell body, not in the gap.
        let in_cell_x = mx_rel % (CELL_W + GAP) < CELL_W;
        let in_cell_y = my_rel % (CELL_H + GAP) < CELL_H;
        if !in_cell_x || !in_cell_y || col >= cols {
            return None;
        }
        let idx = row * cols + col;
        if idx < n {
            Some(idx)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn col_count_varies_with_width() {
        let one = GridState::col_count(CELL_W + MARGIN * 2.0);
        assert_eq!(one, 1);
        let two = GridState::col_count(CELL_W * 2.0 + GAP + MARGIN * 2.0);
        assert_eq!(two, 2);
    }

    #[test]
    fn move_sel_clamps_to_bounds() {
        let mut g = GridState::new(0);
        let cols = GridState::col_count(1000.0);
        // Try to go left from 0: stays at 0.
        g.move_sel(-1, 0, 10, 1000.0);
        assert_eq!(g.sel, 0);
        // Go right by 3 columns.
        g.move_sel(3, 0, 10, 1000.0);
        assert_eq!(g.sel, 3);
        // Go right by 100: clamps to 9.
        g.move_sel(100, 0, 10, 1000.0);
        assert_eq!(g.sel, 9);
        // Go up from row 0: stays on row 0.
        g.sel = 1;
        g.move_sel(0, -1, 10, 1000.0);
        assert_eq!(g.sel, (1isize - cols as isize).max(0) as usize);
    }

    #[test]
    fn hit_test_finds_tile() {
        let g = GridState::new(0);
        // First tile should be at (MARGIN, MARGIN).
        let hit = g.hit_test(MARGIN + CELL_W / 2.0, MARGIN + CELL_H / 2.0, 1000.0, 5);
        assert_eq!(hit, Some(0));
    }

    #[test]
    fn hit_test_gap_returns_none() {
        let g = GridState::new(0);
        // Click in the gap between tile 0 and tile 1.
        let hit = g.hit_test(
            MARGIN + CELL_W + GAP / 2.0,
            MARGIN + CELL_H / 2.0,
            1000.0,
            5,
        );
        assert_eq!(hit, None);
    }
}
