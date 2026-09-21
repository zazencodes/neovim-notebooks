//! The notebook view's geometry (§11.1): a vertical list of cell blocks, scrolled within the
//! home window. Each block is a bordered box around the cell's editor window, then its
//! outputs:
//!
//! ```text
//!  [12] ╭──────────────────── ✓ 0.12s ╮
//!       │import numpy as np            │   ← the cell's Neovim window
//!       ╰──────────────────────────────╯
//!        array([0, 1, 2])                  ← outputs, drawn by nbv
//! ```

use nbv_core::{CellKey, CellKind};
use nbv_nvim::{EditorRect, Viewport};

/// Columns left of the box, for the execution count.
pub const GUTTER: u16 = 7;
/// The narrowest editor window.
const MIN_EDITOR: u16 = 8;

/// One cell's block, in document rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub key: CellKey,
    pub kind: CellKind,
    /// First row: the top border.
    pub top: usize,
    /// Editor lines (at least one).
    pub lines: usize,
    /// Output rows.
    pub outputs: usize,
}

impl Block {
    /// Top border, lines, bottom border.
    pub fn box_height(&self) -> usize {
        self.lines + 2
    }

    /// The box, the outputs, and a blank row after outputs.
    pub fn height(&self) -> usize {
        self.box_height() + self.outputs + usize::from(self.outputs > 0)
    }
}

/// Where things go on screen for one viewport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub viewport: Viewport,
}

impl Geometry {
    /// The first screen row of the cell area (below the header row).
    pub fn area_top(&self) -> u16 {
        self.viewport.row + 1
    }

    pub fn area_height(&self) -> usize {
        self.viewport.height.saturating_sub(1) as usize
    }

    /// The box's left column and width.
    pub fn box_x(&self) -> (u16, u16) {
        let x = self.viewport.col + GUTTER.min(self.viewport.width / 4);
        let w = (self.viewport.col + self.viewport.width).saturating_sub(x + 1).max(MIN_EDITOR + 2);
        (x, w)
    }

    /// The editor window's (and the outputs') left column and width.
    pub fn inner_x(&self) -> (u16, u16) {
        let (x, w) = self.box_x();
        (x + 1, w - 2)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Layout {
    pub blocks: Vec<Block>,
    pub total: usize,
}

impl Layout {
    /// Lays out cells given as `(key, kind, lines, output rows)`, in order.
    pub fn new(cells: impl IntoIterator<Item = (CellKey, CellKind, usize, usize)>) -> Layout {
        let mut blocks = vec![];
        let mut top = 0;
        for (key, kind, lines, outputs) in cells {
            let b = Block { key, kind, top, lines: lines.max(1), outputs };
            top += b.height();
            blocks.push(b);
        }
        Layout { blocks, total: top }
    }

    pub fn index_of(&self, key: &CellKey) -> Option<usize> {
        self.blocks.iter().position(|b| &b.key == key)
    }

    /// The largest useful scroll offset: the last block's end at the bottom of the area.
    pub fn max_scroll(&self, area: usize) -> usize {
        self.total.saturating_sub(area)
    }

    /// The scroll offset that brings block `i` into view, moving as little as possible. A
    /// block taller than the area shows its top. With `whole_box`, only the box counts (an
    /// edited cell must be fully visible; its outputs need not be).
    pub fn reveal(&self, scroll: usize, area: usize, i: usize, whole_box: bool) -> usize {
        let Some(b) = self.blocks.get(i) else { return scroll };
        let len = if whole_box { b.box_height() } else { b.height() };
        let end = b.top + len.min(area);
        let scroll = if b.top < scroll {
            b.top
        } else if end > scroll + area {
            end - area
        } else {
            scroll
        };
        // The block ends by `total`, so it stays in view at the largest useful offset.
        scroll.min(self.max_scroll(area))
    }

    /// The editor windows for the blocks visible at `scroll`: each window shows the visible
    /// rows of its cell, scrolled with `topline`. The active cell's window is left to scroll
    /// itself when it is clipped, so Neovim keeps its cursor in view.
    pub fn editors(&self, g: &Geometry, scroll: usize, active: Option<&CellKey>) -> Vec<EditorRect> {
        let area = g.area_height();
        let (col, width) = g.inner_x();
        let mut out = vec![];
        for b in &self.blocks {
            let body = b.top + 1;
            let (from, to) = (body.max(scroll), (body + b.lines).min(scroll + area));
            if from >= to {
                continue;
            }
            let height = to - from;
            let topline = if Some(&b.key) == active { if height >= b.lines { 1 } else { 0 } } else { from - body + 1 };
            out.push(EditorRect {
                key: b.key.clone(),
                row: g.area_top() + (from - scroll) as u16,
                col,
                width,
                height: height as u16,
                topline,
            });
        }
        out
    }

    /// The block at a screen row.
    pub fn at_row(&self, g: &Geometry, scroll: usize, row: u16) -> Option<usize> {
        let r = (row.checked_sub(g.area_top())? as usize) + scroll;
        if row >= g.area_top() + g.area_height() as u16 {
            return None;
        }
        self.blocks.iter().position(|b| (b.top..b.top + b.height()).contains(&r))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry(height: u16) -> Geometry {
        Geometry { viewport: Viewport { row: 0, col: 0, width: 40, height } }
    }

    fn layout() -> Layout {
        // Heights: a = 3 + 0, b = 5 + 3 + 1, c = 12 + 0.
        Layout::new([
            (CellKey::new("a"), CellKind::Code, 1, 0),
            (CellKey::new("b"), CellKind::Code, 3, 3),
            (CellKey::new("c"), CellKind::Markdown, 10, 0),
        ])
    }

    #[test]
    fn blocks_stack() {
        let l = layout();
        let tops: Vec<usize> = l.blocks.iter().map(|b| b.top).collect();
        assert_eq!(tops, [0, 3, 12]);
        assert_eq!(l.total, 24);
        assert_eq!(Layout::new([(CellKey::new("e"), CellKind::Code, 0, 0)]).blocks[0].lines, 1);
    }

    #[test]
    fn editors_are_clipped_with_topline() {
        let l = layout();
        let g = geometry(11); // header + 10 rows
        // Scrolled to 5: `b`'s body rows 4..7 show rows 5, 6 → lines 2..3 of b.
        let eds = l.editors(&g, 5, None);
        let b = eds.iter().find(|e| e.key.as_str() == "b").unwrap();
        assert_eq!((b.row, b.height, b.topline), (1, 2, 2));
        // `c`'s body starts at 13 → screen row 1 + 8 = 9; two rows fit.
        let c = eds.iter().find(|e| e.key.as_str() == "c").unwrap();
        assert_eq!((c.row, c.height, c.topline), (9, 2, 1));
        assert!(!eds.iter().any(|e| e.key.as_str() == "a"));
        // The active cell, clipped, scrolls itself.
        let eds = l.editors(&g, 5, Some(&CellKey::new("b")));
        assert_eq!(eds.iter().find(|e| e.key.as_str() == "b").unwrap().topline, 0);
    }

    #[test]
    fn reveal_moves_as_little_as_possible() {
        let l = layout();
        assert_eq!(l.reveal(0, 10, 1, false), 2, "b's block ends at 12");
        assert_eq!(l.reveal(0, 10, 1, true), 0, "b's box already fits");
        assert_eq!(l.reveal(20, 10, 0, false), 0);
        assert_eq!(l.reveal(0, 10, 2, true), 12, "a box taller than the area shows its top");
        assert_eq!(l.reveal(12, 10, 2, false), 12);
    }

    #[test]
    fn rows_map_to_blocks() {
        let l = layout();
        let g = geometry(11);
        assert_eq!(l.at_row(&g, 0, 0), None, "header");
        assert_eq!(l.at_row(&g, 0, 1), Some(0));
        assert_eq!(l.at_row(&g, 0, 4), Some(1));
        assert_eq!(l.at_row(&g, 3, 1), Some(1));
        assert_eq!(l.at_row(&g, 0, 11), None, "below the area");
    }
}
