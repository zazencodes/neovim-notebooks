//! Frame composition (§11.2): the notebook layer first, then Neovim's grid on top with every
//! cell of the home window transparent.

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
pub struct Region {
    pub top: u16,
    pub len: u16,
    pub x0: u16,
    pub x1: u16,
    pub offset: usize,
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

/// Draws a region's slice of an output into the buffer.
pub fn draw_output(
    buf: &mut Buffer,
    region: &Region,
    view: &OutputView,
    base: Style,
    protocols: &HashMap<(u64, u16, u16), SlicedProtocol>,
) {
    let area = Rect { x: region.x0, y: region.top, width: region.x1.saturating_sub(region.x0), height: region.len };
    buf.set_style(area, base);
    let mut y = 0usize; // row within the output
    for block in &view.blocks {
        let h = block.height();
        let (from, to) = (y, y + h);
        y = to;
        // Rows of this block that fall in the visible window [offset, offset + len).
        let vis_from = from.max(region.offset);
        let vis_to = to.min(region.offset + region.len as usize);
        if vis_from >= vis_to {
            continue;
        }
        match block {
            Block::Text(lines) => {
                for r in vis_from..vis_to {
                    let screen_y = region.top + (r - region.offset) as u16;
                    let line = &lines[r - from];
                    buf.set_line(area.x, screen_y, line, area.width);
                }
            }
            Block::Image { hash, cols, rows, .. } => {
                let Some(proto) = protocols.get(&(*hash, *cols, *rows)) else { continue };
                // The block's top, relative to the run's first row (negative when scrolled off).
                let top = from as i32 - region.offset as i32;
                let img_area = Rect { x: area.x, y: area.y, width: (*cols).min(area.width), height: area.height };
                SlicedImage::new(proto, SignedPosition::from((0, top as i16))).render_ref(img_area, buf);
            }
        }
    }
}

/// Draws Neovim's grid over the buffer. The home window's cells are transparent (§11.2).
pub fn draw_grid(buf: &mut Buffer, grid: &Grid) {
    let area = buf.area;
    for row in 0..grid.height.min(area.height as usize) {
        for col in 0..grid.width.min(area.width as usize) {
            if grid.is_transparent(row, col) {
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
    use nbv_nvim::grid::TRANSPARENT_SP;
    use nbv_nvim::redraw::{LineCell, RedrawEvent};

    #[test]
    fn transparent_cells_let_the_notebook_through() {
        let mut g = Grid::default();
        g.apply(RedrawEvent::GridResize { grid: 1, width: 4, height: 1 });
        let clear = HlAttr { sp: Some(TRANSPARENT_SP), ..Default::default() };
        g.apply(RedrawEvent::HlAttrDefine { id: 5, attr: clear });
        let cells = vec![
            LineCell { text: " ".into(), hl: Some(5), repeat: 2 },
            LineCell { text: "x".into(), hl: Some(0), repeat: 2 },
        ];
        g.apply(RedrawEvent::GridLine { grid: 1, row: 0, col: 0, cells });
        let mut buf = Buffer::with_lines(["abcd"]);
        draw_grid(&mut buf, &g);
        assert_eq!(buf, Buffer::with_lines(["abxx"]));
    }
}
