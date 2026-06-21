// SPDX-License-Identifier: Apache-2.0

//! Grid-view layout mathematics and selection state. No GPU, no window — fully unit-testable.
//!
//! The grid displays thumbnails in a scrollable tile grid. Each cell is CELL_W × CELL_H pixels;
//! the selection is shown by drawing a slightly larger colored quad behind the thumbnail.

use std::collections::HashSet;

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
    /// Cursor tile (keyboard focus / last clicked). Always a valid playlist index.
    pub sel: usize,
    /// Multi-selection set (the tiles a group action like delete applies to). Contains at
    /// least the cursor after any plain move; may be empty after a Ctrl/Cmd toggle.
    pub selected: HashSet<usize>,
    /// Range anchor for Shift selection (set whenever the cursor moves without Shift).
    pub anchor: usize,
    /// Vertical scroll offset in physical pixels.
    pub scroll_y: f32,
    /// Active rubber-band rectangle in screen coords `(x0, y0, x1, y1)` while drag-selecting.
    /// `None` when not dragging; the renderer draws it as a translucent box.
    pub marquee: Option<(f32, f32, f32, f32)>,
}

impl GridState {
    pub fn new(sel: usize) -> Self {
        GridState {
            sel,
            selected: HashSet::from([sel]),
            anchor: sel,
            scroll_y: 0.0,
            marquee: None,
        }
    }

    /// Indices of all tiles whose cell intersects the screen-space rectangle (any corner order).
    /// Used for rubber-band (drag) selection.
    pub fn tiles_in_rect(
        &self,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        win_w: f32,
        n: usize,
    ) -> Vec<usize> {
        let (lx, rx) = (x0.min(x1), x0.max(x1));
        let (ty, by) = (y0.min(y1), y0.max(y1));
        (0..n)
            .filter(|&i| {
                let (cx, cy) = self.cell_origin(i, win_w);
                cx < rx && cx + CELL_W > lx && cy < by && cy + CELL_H > ty
            })
            .collect()
    }

    /// Single-select tile `i`: it becomes the only selection, the cursor, and the anchor.
    pub fn select_single(&mut self, i: usize) {
        self.selected.clear();
        self.selected.insert(i);
        self.sel = i;
        self.anchor = i;
    }

    /// Toggle tile `i` in the selection (Ctrl/Cmd). Moves the cursor + anchor to `i`.
    pub fn toggle(&mut self, i: usize) {
        if !self.selected.insert(i) {
            self.selected.remove(&i);
        }
        self.sel = i;
        self.anchor = i;
    }

    /// Select the contiguous range between the anchor and `to` (Shift). Cursor moves to `to`;
    /// the anchor is preserved so the range can be re-extended.
    pub fn select_range(&mut self, to: usize) {
        let (lo, hi) = if self.anchor <= to {
            (self.anchor, to)
        } else {
            (to, self.anchor)
        };
        self.selected = (lo..=hi).collect();
        self.sel = to;
    }

    /// Select every tile in the playlist (Ctrl/Cmd+A).
    pub fn select_all(&mut self, n: usize) {
        self.selected = (0..n).collect();
    }

    /// Collapse the selection back to just the cursor (first Esc).
    pub fn clear_to_cursor(&mut self) {
        self.selected.clear();
        self.selected.insert(self.sel);
        self.anchor = self.sel;
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
    fn selection_single_toggle_range() {
        let mut g = GridState::new(2);
        assert_eq!(g.selected, HashSet::from([2]));
        // Ctrl-toggle adds/removes.
        g.toggle(5);
        assert_eq!(g.selected, HashSet::from([2, 5]));
        g.toggle(5);
        assert_eq!(g.selected, HashSet::from([2]));
        // Single-select collapses to one.
        g.select_single(3);
        assert_eq!(g.selected, HashSet::from([3]));
        assert_eq!(g.anchor, 3);
        // Shift-range from anchor 3 to 6 (inclusive), cursor follows.
        g.select_range(6);
        assert_eq!(g.selected, HashSet::from([3, 4, 5, 6]));
        assert_eq!(g.sel, 6);
        assert_eq!(g.anchor, 3); // anchor preserved
        // Range the other direction re-extends from the same anchor.
        g.select_range(1);
        assert_eq!(g.selected, HashSet::from([1, 2, 3]));
    }

    #[test]
    fn tiles_in_rect_covers_dragged_cells() {
        let g = GridState::new(0);
        // A rectangle from inside tile 0 to inside tile 1 (same row) covers both.
        let hits = g.tiles_in_rect(
            MARGIN + CELL_W / 2.0,
            MARGIN + CELL_H / 2.0,
            MARGIN + CELL_W + GAP + CELL_W / 2.0,
            MARGIN + CELL_H / 2.0,
            1000.0,
            5,
        );
        assert!(hits.contains(&0) && hits.contains(&1));
        // A tiny rect inside tile 0 only hits tile 0.
        let one = g.tiles_in_rect(MARGIN + 2.0, MARGIN + 2.0, MARGIN + 4.0, MARGIN + 4.0, 1000.0, 5);
        assert_eq!(one, vec![0]);
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
