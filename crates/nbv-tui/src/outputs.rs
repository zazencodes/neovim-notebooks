//! The output layer's content: what a cell's outputs look like and how many rows they need.
//! Heights depend on terminal geometry, so they are computed here rather than in core (§5.2).

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use base64::Engine;
use image::DynamicImage;
use nbv_core::Cell;
use nbv_core::document::multiline;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui_image::FontSize;
use serde_json::Value;
use unicode_width::UnicodeWidthChar;

use crate::ansi;

/// Text rows shown per cell before the head is elided.
pub const MAX_TEXT_ROWS: usize = 40;
/// Image height cap, in rows.
pub const MAX_IMAGE_ROWS: u16 = 24;

#[derive(Clone)]
pub enum Block {
    Text(Vec<Line<'static>>),
    Image { hash: u64, image: Arc<DynamicImage>, cols: u16, rows: u16 },
}

impl Block {
    pub fn height(&self) -> usize {
        match self {
            Block::Text(lines) => lines.len(),
            Block::Image { rows, .. } => *rows as usize,
        }
    }
}

/// A cell's rendered outputs.
#[derive(Clone, Default)]
pub struct OutputView {
    pub blocks: Vec<Block>,
    pub height: usize,
    /// The width the view was built for.
    pub width: u16,
}

/// Decoded images by content hash, so re-rendering does not re-decode.
#[derive(Default)]
pub struct Images {
    decoded: HashMap<u64, Result<Arc<DynamicImage>, String>>,
}

impl Images {
    fn get(&mut self, b64: &str) -> Result<(u64, Arc<DynamicImage>), String> {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        b64.hash(&mut h);
        let hash = h.finish();
        let img = self
            .decoded
            .entry(hash)
            .or_insert_with(|| {
                let clean: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
                let bytes = base64::engine::general_purpose::STANDARD.decode(clean).map_err(|e| e.to_string())?;
                image::load_from_memory(&bytes).map(Arc::new).map_err(|e| e.to_string())
            })
            .clone()?;
        Ok((hash, img))
    }
}

fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

fn text_block(text: &str, base: Style) -> Vec<Line<'static>> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    ansi::lines(text, base)
}

/// Wraps lines at `width` columns, as Neovim's `wrap` does without `linebreak`.
fn wrap(lines: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    let width = width.max(1) as usize;
    let mut out = vec![];
    for line in lines {
        if line.width() <= width {
            out.push(line);
            continue;
        }
        let mut row: Vec<Span<'static>> = vec![];
        let mut col = 0;
        for span in &line.spans {
            let mut run = String::new();
            for ch in span.content.chars() {
                let w = ch.width().unwrap_or(0);
                if col + w > width && col > 0 {
                    row.push(Span::styled(std::mem::take(&mut run), span.style));
                    out.push(Line::from(std::mem::take(&mut row)).style(line.style));
                    col = 0;
                }
                run.push(ch);
                col += w;
            }
            if !run.is_empty() {
                row.push(Span::styled(run, span.style));
            }
        }
        out.push(Line::from(row).style(line.style));
    }
    out
}

/// Image size in cells: natural size, shrunk to fit `max_cols` × `MAX_IMAGE_ROWS`.
fn image_cells(img: &DynamicImage, font: FontSize, max_cols: u16) -> (u16, u16) {
    let fw = font.width.max(1) as f64;
    let fh = font.height.max(1) as f64;
    let (w, h) = (img.width() as f64 / fw, img.height() as f64 / fh);
    let scale = (max_cols as f64 / w).min(MAX_IMAGE_ROWS as f64 / h).min(1.0);
    (((w * scale).ceil() as u16).max(1), ((h * scale).ceil() as u16).max(1))
}

/// Builds the view of a cell's outputs for a given width.
pub fn build(cell: &Cell, width: u16, font: FontSize, images: &mut Images) -> OutputView {
    let mut blocks = vec![];
    for out in cell.outputs() {
        let kind = out.get("output_type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "stream" => {
                let err = out.get("name").and_then(Value::as_str) == Some("stderr");
                let base = if err { Style::default().fg(Color::LightRed) } else { Style::default() };
                blocks.push(Block::Text(text_block(&multiline(out.get("text")), base)));
            }
            "error" => {
                let tb: Vec<String> = out
                    .get("traceback")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect();
                let text = if tb.is_empty() {
                    format!(
                        "{}: {}",
                        out.get("ename").and_then(Value::as_str).unwrap_or("Error"),
                        out.get("evalue").and_then(Value::as_str).unwrap_or("")
                    )
                } else {
                    tb.join("\n")
                };
                blocks.push(Block::Text(text_block(&text, Style::default())));
            }
            "execute_result" | "display_data" => {
                let data = out.get("data").cloned().unwrap_or(Value::Null);
                let image = ["image/png", "image/jpeg", "image/gif"]
                    .iter()
                    .find_map(|m| data.get(*m).map(|v| (m, images.get(&multiline(Some(v))))));
                if let Some((mime, image)) = image {
                    match image {
                        Ok((hash, image)) => {
                            let (cols, rows) = image_cells(&image, font, width.saturating_sub(2).max(1));
                            blocks.push(Block::Image { hash, image, cols, rows });
                        }
                        Err(e) => blocks.push(Block::Text(vec![Line::styled(
                            format!("{mime} could not be decoded: {e}"),
                            Style::default().fg(Color::LightRed),
                        )])),
                    }
                } else if let Some(t) = data.get("text/plain") {
                    blocks.push(Block::Text(text_block(&multiline(Some(t)), Style::default())));
                } else if let Some(mime) = data.as_object().and_then(|m| m.keys().next()) {
                    blocks.push(Block::Text(vec![Line::styled(format!("[{mime} output]"), dim())]));
                }
            }
            other => blocks.push(Block::Text(vec![Line::styled(format!("[unsupported output: {other}]"), dim())])),
        }
    }
    for b in &mut blocks {
        if let Block::Text(lines) = b {
            *lines = wrap(std::mem::take(lines), width);
        }
    }
    cap_text(&mut blocks);
    // Why the last run could not happen: after the outputs, never elided.
    if let Some(e) = &cell.runtime.error {
        blocks.push(Block::Text(wrap(text_block(e, Style::default().fg(Color::LightRed)), width)));
    }
    let height = blocks.iter().map(Block::height).sum();
    OutputView { blocks, height, width }
}

/// Keeps the last `MAX_TEXT_ROWS` text (display) rows of a cell, eliding the head: for long-running
/// output the latest lines matter most.
fn cap_text(blocks: &mut [Block]) {
    let total: usize = blocks.iter().filter(|b| matches!(b, Block::Text(_))).map(Block::height).sum();
    if total <= MAX_TEXT_ROWS {
        return;
    }
    let mut drop = total - MAX_TEXT_ROWS + 1;
    let hidden = drop;
    let mut marked = false;
    for b in blocks.iter_mut() {
        if let Block::Text(lines) = b {
            let n = drop.min(lines.len());
            lines.drain(..n);
            drop -= n;
            if !marked {
                lines.insert(0, Line::from(Span::styled(format!("⋯ {hidden} earlier rows"), dim())));
                marked = true;
            }
            if drop == 0 {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cell(outputs: Value) -> Cell {
        let raw = json!({"cell_type": "code", "execution_count": 1, "metadata": {}, "outputs": outputs, "source": ""});
        Cell::from_raw(raw.as_object().unwrap().clone())
    }

    #[test]
    fn text_outputs_and_heights() {
        let c = cell(json!([
            {"output_type": "stream", "name": "stdout", "text": ["a\n", "b\n"]},
            {"output_type": "execute_result", "execution_count": 1, "data": {"text/plain": ["42"]}, "metadata": {}},
            {"output_type": "error", "ename": "E", "evalue": "v", "traceback": ["line1\nline2"]},
            {"output_type": "display_data", "data": {"text/html": "<b>"}, "metadata": {}},
        ]));
        let v = build(&c, 80, FontSize::new(10, 20), &mut Images::default());
        assert_eq!(v.height, 2 + 1 + 2 + 1);
    }

    #[test]
    fn long_lines_wrap_at_the_width() {
        let c = cell(json!([{"output_type": "stream", "name": "stdout", "text": ["abcdefghij\n", "xy\n"]}]));
        let v = build(&c, 4, FontSize::new(10, 20), &mut Images::default());
        let Block::Text(lines) = &v.blocks[0] else { panic!() };
        let rows: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        assert_eq!(rows, ["abcd", "efgh", "ij", "xy"]);
        assert_eq!(v.height, 4);
    }

    #[test]
    fn long_output_keeps_the_tail() {
        let text: Vec<String> = (0..100).map(|i| format!("{i}\n")).collect();
        let c = cell(json!([{"output_type": "stream", "name": "stdout", "text": text}]));
        let v = build(&c, 80, FontSize::new(10, 20), &mut Images::default());
        assert_eq!(v.height, MAX_TEXT_ROWS);
        let Block::Text(lines) = &v.blocks[0] else { panic!() };
        assert!(lines[0].spans[0].content.contains("61 earlier rows"));
        assert_eq!(lines.last().unwrap().spans[0].content, "99");
    }

    #[test]
    fn an_undecodable_image_says_so() {
        let c = cell(json!([{"output_type": "display_data",
            "data": {"image/png": "iVBORw0KGgo=", "text/plain": ["<Figure>"]}, "metadata": {}}]));
        let v = build(&c, 80, FontSize::new(10, 20), &mut Images::default());
        let Block::Text(lines) = &v.blocks[0] else { panic!("expected the error") };
        assert!(lines[0].spans[0].content.starts_with("image/png could not be decoded"), "{lines:?}");
    }

    #[test]
    fn a_failed_run_shows_its_reason_under_the_outputs() {
        let mut c = cell(json!([{"output_type": "stream", "name": "stdout", "text": ["old\n"]}]));
        c.runtime.error = Some("no kernel".into());
        let v = build(&c, 80, FontSize::new(10, 20), &mut Images::default());
        let Block::Text(lines) = v.blocks.last().unwrap() else { panic!() };
        assert_eq!(lines[0].spans[0].content, "no kernel");
    }

    #[test]
    fn images_fit_width_and_row_cap() {
        let mut png = vec![];
        let img = DynamicImage::new_rgb8(400, 200);
        img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let c = cell(
            json!([{"output_type": "display_data", "data": {"image/png": b64, "text/plain": ["<Figure>"]}, "metadata": {}}]),
        );
        let v = build(&c, 22, FontSize::new(10, 20), &mut Images::default());
        let Block::Image { cols, rows, .. } = v.blocks[0] else { panic!("expected an image") };
        assert_eq!((cols, rows), (20, 5));
    }
}
