//! The grid model: Neovim's single composed global grid (R7, §10.1).

use std::collections::{HashMap, HashSet};

use crate::redraw::{HlAttr, ModeInfo, RedrawEvent};

/// The special colour of `NbvTransparent`, the highlight the companion gives the home window.
/// Neovim reports a window-highlight remap under the original group's name, so the colour,
/// not the name, marks a transparent cell (§11.2).
pub const TRANSPARENT_SP: u32 = 0x010203;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GridCell {
    /// One grapheme, or empty for the right half of a double-width character.
    pub text: String,
    pub hl: u32,
}

impl Default for GridCell {
    fn default() -> Self {
        GridCell { text: " ".into(), hl: 0 }
    }
}

#[derive(Debug, Default)]
pub struct Grid {
    pub width: usize,
    pub height: usize,
    pub cells: Vec<GridCell>,
    pub cursor: (usize, usize),
    pub hl: HashMap<u32, HlAttr>,
    /// hl ids that resolve to `NbvTransparent`.
    transparent: HashSet<u32>,
    pub default_fg: Option<u32>,
    pub default_bg: Option<u32>,
    pub modes: Vec<ModeInfo>,
    pub mode: usize,
    pub mode_name: String,
    /// Counts mode changes, so a caller can tell a mode it saw from a later return to it.
    pub mode_changes: u64,
    pub busy: bool,
    pub title: Option<String>,
}

impl Grid {
    pub fn cell(&self, row: usize, col: usize) -> &GridCell {
        &self.cells[row * self.width + col]
    }

    /// Whether a cell belongs to the home window, through which the notebook shows (§11.2).
    pub fn is_transparent(&self, row: usize, col: usize) -> bool {
        self.transparent.contains(&self.cell(row, col).hl)
    }

    pub fn attr(&self, hl: u32) -> Option<&HlAttr> {
        self.hl.get(&hl)
    }

    pub fn row_text(&self, row: usize) -> String {
        (0..self.width).map(|c| self.cell(row, c).text.as_str()).collect()
    }

    /// Applies a batch of events. Returns true on `flush`, the only point at which the grid
    /// is consistent and may be rendered.
    pub fn apply(&mut self, ev: RedrawEvent) -> bool {
        match ev {
            RedrawEvent::GridResize { width, height, .. } => {
                self.width = width;
                self.height = height;
                self.cells = vec![GridCell::default(); width * height];
            }
            RedrawEvent::GridClear { .. } => self.cells.fill(GridCell::default()),
            RedrawEvent::GridLine { row, col, cells, .. } => {
                if row >= self.height {
                    return false;
                }
                let mut c = col;
                let mut hl = 0;
                for cell in cells {
                    if let Some(h) = cell.hl {
                        hl = h;
                    }
                    for _ in 0..cell.repeat {
                        if c >= self.width {
                            break;
                        }
                        self.cells[row * self.width + c] = GridCell { text: cell.text.clone(), hl };
                        c += 1;
                    }
                }
            }
            RedrawEvent::GridScroll { top, bot, left, right, rows, .. } => {
                let (bot, right) = (bot.min(self.height), right.min(self.width));
                let w = self.width;
                if rows > 0 {
                    for r in top..bot.saturating_sub(rows as usize) {
                        let src = r + rows as usize;
                        let (a, b) = self.cells.split_at_mut(src * w);
                        a[r * w + left..r * w + right].clone_from_slice(&b[left..right]);
                    }
                } else if rows < 0 {
                    let n = (-rows) as usize;
                    for r in (top + n..bot).rev() {
                        let src = r - n;
                        let (a, b) = self.cells.split_at_mut(r * w);
                        b[left..right].clone_from_slice(&a[src * w + left..src * w + right]);
                    }
                }
            }
            RedrawEvent::GridCursorGoto { row, col, .. } => self.cursor = (row, col),
            RedrawEvent::HlAttrDefine { id, attr } => {
                if attr.sp == Some(TRANSPARENT_SP) {
                    self.transparent.insert(id);
                } else {
                    self.transparent.remove(&id);
                }
                self.hl.insert(id, attr);
            }
            RedrawEvent::DefaultColors { fg, bg, .. } => {
                self.default_fg = fg;
                self.default_bg = bg;
            }
            RedrawEvent::ModeInfoSet { modes } => self.modes = modes,
            RedrawEvent::ModeChange { mode, index } => {
                self.mode = index;
                self.mode_name = mode;
                self.mode_changes += 1;
            }
            RedrawEvent::BusyStart => self.busy = true,
            RedrawEvent::BusyStop => self.busy = false,
            RedrawEvent::SetTitle(t) => self.title = Some(t),
            RedrawEvent::Flush => return true,
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redraw::LineCell;

    fn line(row: usize, text: &str) -> RedrawEvent {
        RedrawEvent::GridLine {
            grid: 1,
            row,
            col: 0,
            cells: text.chars().map(|c| LineCell { text: c.to_string(), hl: Some(0), repeat: 1 }).collect(),
        }
    }

    #[test]
    fn lines_and_scrolling() {
        let mut g = Grid::default();
        g.apply(RedrawEvent::GridResize { grid: 1, width: 3, height: 3 });
        for (r, t) in ["abc", "def", "ghi"].iter().enumerate() {
            g.apply(line(r, t));
        }
        g.apply(RedrawEvent::GridScroll { grid: 1, top: 0, bot: 3, left: 0, right: 3, rows: 1 });
        assert_eq!(g.row_text(0), "def");
        assert_eq!(g.row_text(1), "ghi");
        g.apply(RedrawEvent::GridScroll { grid: 1, top: 0, bot: 3, left: 0, right: 3, rows: -2 });
        assert_eq!(g.row_text(2), "def");
    }

    #[test]
    fn the_sentinel_colour_marks_transparent_cells() {
        let mut g = Grid::default();
        let attr = HlAttr { sp: Some(TRANSPARENT_SP), names: vec!["Normal".into()], ..Default::default() };
        g.apply(RedrawEvent::HlAttrDefine { id: 42, attr });
        g.apply(RedrawEvent::GridResize { grid: 1, width: 2, height: 1 });
        g.apply(RedrawEvent::GridLine {
            grid: 1,
            row: 0,
            col: 0,
            cells: vec![LineCell { text: " ".into(), hl: Some(42), repeat: 2 }],
        });
        assert!(g.is_transparent(0, 1));
        g.apply(RedrawEvent::HlAttrDefine { id: 42, attr: HlAttr::default() });
        assert!(!g.is_transparent(0, 1), "a redefined id is opaque again");
    }
}
