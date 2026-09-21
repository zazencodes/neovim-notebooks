//! Frame composition (§11.2): the output layer first, then Neovim's grid on top with every
//! placeholder cell transparent. Placement is read from the render stream: a vertical run of
//! one slot's cells is that output's visible extent.

use std::collections::HashMap;

use nbv_nvim::Grid;
use nbv_nvim::redraw::HlAttr;
use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui_image::sliced::{SignedPosition, SlicedImage, SlicedProtocol};

use crate::outputs::{Block, OutputView};

/// A visible stretch of one output: rows `[top, top + len)` of the screen, columns `[x0, x1)`,
/// showing rows `[offset, offset + len)` of the output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    pub slot: u16,
    pub top: u16,
    pub len: u16,
    pub x0: u16,
    pub x1: u16,
    pub offset: usize,
}

/// Placeholder rows carry their row index as `#<n>#`, invisible because placeholder cells are
/// transparent. Reading it back gives the exact offset of a clipped output.
pub fn row_tag(row: usize) -> String {
    format!("#{row}#")
}

fn parse_tag(text: &str) -> Option<usize> {
    let rest = text.strip_prefix('#')?;
    let end = rest.find('#')?;
    rest[..end].parse().ok()
}

/// A run being assembled: slot, top row, column span, length, and the first tag seen as
/// (tag, row within the run).
struct Open {
    slot: u16,
    top: u16,
    x0: u16,
    x1: u16,
    len: u16,
    tag: Option<(usize, u16)>,
}

/// Finds every output run in the grid. `height_of(slot)` is the output's full height.
///
/// A row's segment continues a run of the same slot on the row above when their columns
/// overlap; a float covering part of an output splits segments but not the run.
pub fn find_runs(grid: &Grid, height_of: impl Fn(u16) -> Option<usize>) -> Vec<Run> {
    let mut open: Vec<Open> = vec![];
    let mut done: Vec<Open> = vec![];
    for row in 0..grid.height {
        let mut next: Vec<Open> = vec![];
        let mut col = 0;
        while col < grid.width {
            let Some(slot) = grid.slot_at(row, col) else {
                col += 1;
                continue;
            };
            let start = col;
            let mut text = String::new();
            while col < grid.width && grid.slot_at(row, col) == Some(slot) {
                text.push_str(&grid.cell(row, col).text);
                col += 1;
            }
            let (x0, x1, tag) = (start as u16, col as u16, parse_tag(&text));
            let overlaps = |o: &Open| o.slot == slot && o.x0 < x1 && x0 < o.x1;
            if let Some(o) = next.iter_mut().find(|o| overlaps(o)) {
                // Another segment of a run already continued on this row.
                o.x0 = o.x0.min(x0);
                o.x1 = o.x1.max(x1);
                if o.tag.is_none() {
                    o.tag = tag.map(|t| (t, o.len - 1));
                }
            } else if let Some(i) = open.iter().position(|o| overlaps(o)) {
                let mut o = open.remove(i);
                o.len += 1;
                o.x0 = o.x0.min(x0);
                o.x1 = o.x1.max(x1);
                if o.tag.is_none() {
                    o.tag = tag.map(|t| (t, o.len - 1));
                }
                next.push(o);
            } else {
                next.push(Open { slot, top: row as u16, x0, x1, len: 1, tag: tag.map(|t| (t, 0)) });
            }
        }
        done.append(&mut open);
        open = next;
    }
    done.append(&mut open);
    done.into_iter()
        .map(|o| {
            let height = height_of(o.slot).unwrap_or(o.len as usize);
            let offset = match o.tag {
                Some((tag, at)) => tag.saturating_sub(at as usize),
                // Every tag hidden: the spec's clipping rule (§11.3).
                None if (o.len as usize) < height && o.top == 0 => height - o.len as usize,
                None => 0,
            };
            Run { slot: o.slot, top: o.top, len: o.len, x0: o.x0, x1: o.x1, offset }
        })
        .collect()
}

fn rgb(c: Option<u32>) -> Option<Color> {
    c.map(|c| Color::Rgb((c >> 16) as u8, (c >> 8) as u8, c as u8))
}

pub fn style_of(attr: Option<&HlAttr>, default_fg: Option<u32>, default_bg: Option<u32>) -> Style {
    let a = attr.cloned().unwrap_or_default();
    let mut fg = rgb(a.fg.or(default_fg)).unwrap_or(Color::Reset);
    let mut bg = rgb(a.bg.or(default_bg)).unwrap_or(Color::Reset);
    if a.reverse {
        std::mem::swap(&mut fg, &mut bg);
    }
    let mut m = Modifier::empty();
    if a.bold {
        m |= Modifier::BOLD;
    }
    if a.italic {
        m |= Modifier::ITALIC;
    }
    if a.underline || a.undercurl {
        m |= Modifier::UNDERLINED;
    }
    if a.strikethrough {
        m |= Modifier::CROSSED_OUT;
    }
    let mut s = Style::default().fg(fg).bg(bg).add_modifier(m);
    if a.undercurl
        && let Some(sp) = rgb(a.sp)
    {
        s = s.underline_color(sp);
    }
    s
}

/// Draws one run's slice of an output into the buffer.
pub fn draw_output(
    buf: &mut Buffer,
    run: &Run,
    view: &OutputView,
    base: Style,
    protocols: &HashMap<(u64, u16, u16), SlicedProtocol>,
) {
    let area = Rect { x: run.x0, y: run.top, width: run.x1.saturating_sub(run.x0), height: run.len };
    buf.set_style(area, base);
    let mut y = 0usize; // row within the output
    for block in &view.blocks {
        let h = block.height();
        let (from, to) = (y, y + h);
        y = to;
        // Rows of this block that fall in the visible window [offset, offset + len).
        let vis_from = from.max(run.offset);
        let vis_to = to.min(run.offset + run.len as usize);
        if vis_from >= vis_to {
            continue;
        }
        match block {
            Block::Text(lines) => {
                for r in vis_from..vis_to {
                    let screen_y = run.top + (r - run.offset) as u16;
                    let line = &lines[r - from];
                    buf.set_line(area.x, screen_y, line, area.width);
                }
            }
            Block::Image { hash, cols, rows, .. } => {
                let Some(proto) = protocols.get(&(*hash, *cols, *rows)) else { continue };
                // The block's top, relative to the run's first row (negative when scrolled off).
                let top = from as i32 - run.offset as i32;
                let img_area = Rect { x: area.x, y: area.y, width: (*cols).min(area.width), height: area.height };
                SlicedImage::new(proto, SignedPosition::from((0, top as i16))).render_ref(img_area, buf);
            }
        }
    }
}

/// Draws Neovim's grid over the buffer. Placeholder cells are transparent (§11.2).
pub fn draw_grid(buf: &mut Buffer, grid: &Grid) {
    let area = buf.area;
    for row in 0..grid.height.min(area.height as usize) {
        for col in 0..grid.width.min(area.width as usize) {
            if grid.slot_at(row, col).is_some() {
                continue;
            }
            let cell = grid.cell(row, col);
            let Some(bc) = buf.cell_mut((col as u16, row as u16)) else { continue };
            // Neovim draws over whatever the output layer put here, images included.
            bc.reset();
            bc.set_diff_option(CellDiffOption::None);
            if cell.text.is_empty() {
                // Right half of a double-width character: the diff skips it after the wide cell.
                continue;
            }
            bc.set_symbol(&cell.text);
            bc.set_style(style_of(grid.attr(cell.hl), grid.default_fg, grid.default_bg));
        }
    }
}

trait RenderRef {
    fn render_ref(self, area: Rect, buf: &mut Buffer);
}

impl RenderRef for SlicedImage<'_> {
    fn render_ref(self, area: Rect, buf: &mut Buffer) {
        ratatui::widgets::Widget::render(self, area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nbv_nvim::redraw::{LineCell, RedrawEvent};

    fn grid_with(rows: &[(usize, &str, u32)], width: usize, height: usize) -> Grid {
        let mut g = Grid::default();
        g.apply(RedrawEvent::GridResize { grid: 1, width, height });
        for (slot, id) in [(3u16, 10u32), (4, 11)] {
            let attr = HlAttr { names: vec![format!("NbvOutputSlot{slot}")], ..Default::default() };
            g.apply(RedrawEvent::HlAttrDefine { id, attr });
        }
        for (row, text, hl) in rows {
            let cells = text.chars().map(|c| LineCell { text: c.to_string(), hl: Some(*hl), repeat: 1 }).collect();
            g.apply(RedrawEvent::GridLine { grid: 1, row: *row, col: 2, cells });
        }
        g
    }

    #[test]
    fn runs_read_offsets_from_row_tags() {
        // Output of slot 3 (height 5) scrolled so rows 2..5 are visible at screen rows 0..3.
        let g = grid_with(&[(0, "#2#   ", 10), (1, "#3#   ", 10), (2, "#4#   ", 10), (4, "#0#  ", 11)], 10, 6);
        let runs = find_runs(&g, |s| Some(if s == 3 { 5 } else { 3 }));
        assert_eq!(runs.len(), 2);
        let r3 = runs.iter().find(|r| r.slot == 3).unwrap();
        assert_eq!((r3.top, r3.len, r3.x0, r3.x1, r3.offset), (0, 3, 2, 8, 2));
        let r4 = runs.iter().find(|r| r.slot == 4).unwrap();
        assert_eq!((r4.top, r4.len, r4.offset), (4, 1, 0));
    }

    #[test]
    fn a_float_hiding_the_tag_falls_back_to_other_rows() {
        // Row 1's tag is covered by a float (not placeholder cells), row 2's is intact.
        let mut g = grid_with(&[(0, "#5#   ", 10), (1, "#6#   ", 10), (2, "#7#   ", 10)], 10, 3);
        g.apply(RedrawEvent::GridLine {
            grid: 1,
            row: 0,
            col: 2,
            cells: vec![LineCell { text: "x".into(), hl: Some(0), repeat: 3 }],
        });
        let runs = find_runs(&g, |_| Some(10));
        // Row 0 lost its tag under the float but still belongs to the run; row 1's tag places it.
        assert_eq!(runs.len(), 1);
        assert_eq!((runs[0].top, runs[0].len, runs[0].x0, runs[0].offset), (0, 3, 2, 5));
    }
}
