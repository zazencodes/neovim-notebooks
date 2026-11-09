//! The output layer's content: what a cell's outputs look like and how many rows they need.
//! Heights depend on terminal geometry, so they are computed here rather than in core (§5.2).

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::Cursor;
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
    Image { hash: u64, data: Arc<[u8]>, cols: u16, rows: u16 },
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

impl OutputView {
    /// Collapse only the display; the notebook retains all output data.
    pub fn collapse(&mut self) {
        if self.height > 0 {
            self.blocks = vec![Block::Text(vec![Line::styled("[outputs collapsed · za expands]", dim())])];
            self.height = 1;
        }
    }
}

struct MeasuredImage {
    hash: u64,
    data: Arc<[u8]>,
    dimensions: (u32, u32),
}

/// Raster cache budget. Output views retain compressed bytes, never decoded raster Arcs.
const DECODED_BYTES: usize = 32 * 1024 * 1024;

/// Decoded rasters are retained by bytes, with least-recently-used eviction.
#[derive(Default)]
pub struct Images {
    decoded: HashMap<u64, (Arc<DynamicImage>, u64)>,
    bytes: usize,
    clock: u64,
}

impl Images {
    /// Header-only measurement: layout never expands offscreen images into pixels.
    fn measure(&mut self, b64: &str) -> Result<MeasuredImage, String> {
        let clean: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
        let bytes = base64::engine::general_purpose::STANDARD.decode(clean).map_err(|e| e.to_string())?;
        let mut h = std::collections::hash_map::DefaultHasher::new();
        bytes.hash(&mut h);
        let dimensions = image::ImageReader::new(Cursor::new(&bytes))
            .with_guessed_format()
            .map_err(|e| e.to_string())?
            .into_dimensions()
            .map_err(|e| e.to_string())?;
        Ok(MeasuredImage { hash: h.finish(), data: bytes.into(), dimensions })
    }

    pub fn decode(&mut self, hash: u64, data: &[u8]) -> Result<Arc<DynamicImage>, String> {
        self.decode_with_budget(hash, data, DECODED_BYTES)
    }

    fn decode_with_budget(&mut self, hash: u64, data: &[u8], budget: usize) -> Result<Arc<DynamicImage>, String> {
        self.clock += 1;
        if let Some((image, used)) = self.decoded.get_mut(&hash) {
            *used = self.clock;
            return Ok(image.clone());
        }
        let image = Arc::new(image::load_from_memory(data).map_err(|e| e.to_string())?);
        let bytes = image.as_bytes().len();
        while self.bytes.saturating_add(bytes) > budget && !self.decoded.is_empty() {
            let oldest = *self.decoded.iter().min_by_key(|(_, (_, used))| used).expect("nonempty cache").0;
            let (old, _) = self.decoded.remove(&oldest).expect("cached image");
            self.bytes -= old.as_bytes().len();
        }
        // A single oversized raster may be rendered transiently but is never cached.
        if bytes <= budget {
            self.bytes += bytes;
            self.decoded.insert(hash, (image.clone(), self.clock));
        }
        Ok(image)
    }

    /// Remove obsolete rasters, including images whose outputs were replaced or cleared.
    pub fn retain(&mut self, mut keep: impl FnMut(u64) -> bool) {
        self.decoded.retain(|hash, (image, _)| {
            if keep(*hash) {
                true
            } else {
                self.bytes -= image.as_bytes().len();
                false
            }
        });
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
fn image_cells(dimensions: (u32, u32), font: FontSize, max_cols: u16) -> (u16, u16) {
    let fw = font.width.max(1) as f64;
    let fh = font.height.max(1) as f64;
    let (w, h) = (dimensions.0 as f64 / fw, dimensions.1 as f64 / fh);
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
                let data = out.get("data").unwrap_or(&Value::Null);
                let image = ["image/png", "image/jpeg", "image/gif"]
                    .iter()
                    .find_map(|m| data.get(*m).map(|v| (m, images.measure(&multiline(Some(v))))));
                if let Some((mime, image)) = image {
                    match image {
                        Ok(MeasuredImage { hash, data, dimensions }) => {
                            let (cols, rows) = image_cells(dimensions, font, width.saturating_sub(2).max(1));
                            blocks.push(Block::Image { hash, data, cols, rows });
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

    fn png_data(side: u32, value: u8) -> Vec<u8> {
        let mut bytes = Cursor::new(vec![]);
        let image = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(side, side, image::Rgba([value, 0, 0, 255])));
        image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        bytes.into_inner()
    }

    #[test]
    fn offscreen_measurement_retains_no_raster_and_eviction_releases_view_images() {
        let data = png_data(32, 7);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
        let c = cell(json!([{"output_type":"display_data", "data":{"image/png":b64}}]));
        let mut images = Images::default();
        let view = build(&c, 80, FontSize::new(10, 20), &mut images);
        assert!(images.decoded.is_empty(), "measuring output height must not decode pixels");
        let Block::Image { hash, data, .. } = &view.blocks[0] else { panic!("image") };
        let decoded = images.decode(*hash, data).unwrap();
        let weak = Arc::downgrade(&decoded);
        drop(decoded);
        images.retain(|_| false);
        assert_eq!(images.bytes, 0);
        assert!(weak.upgrade().is_none(), "a still-cached OutputView must not own decoded pixels");
        assert_eq!(view.blocks.len(), 1);
        assert_eq!(c.outputs()[0]["data"]["image/png"], b64, "canonical notebook image remains intact");
    }

    #[test]
    fn raster_cache_plateaus_by_bytes_and_releases_oversized_images() {
        let mut images = Images::default();
        let budget = 2 * 32 * 32 * 4;
        for id in 0..100 {
            drop(images.decode_with_budget(id, &png_data(32, id as u8), budget).unwrap());
            assert!(images.bytes <= budget);
            assert!(images.decoded.len() <= 2);
        }
        assert!(images.decoded.contains_key(&99));
        // Touch the oldest entry, then prove LRU rather than insertion-order eviction.
        images.decode_with_budget(98, &[], budget).unwrap();
        images.decode_with_budget(100, &png_data(32, 100), budget).unwrap();
        assert!(images.decoded.contains_key(&98));
        assert!(!images.decoded.contains_key(&99));
        let oversized = images.decode_with_budget(101, &png_data(64, 101), budget).unwrap();
        assert_eq!(oversized.width(), 64);
        assert!(!images.decoded.contains_key(&101));
        assert_eq!(images.bytes, 0);
    }

    #[test]
    #[ignore = "release performance scenario: cargo test -p nbv-tui --release plotting_cache_scenario -- --ignored --nocapture"]
    fn plotting_cache_scenario() {
        use std::time::Instant;
        let data = png_data(800, 7);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
        let mut images = Images::default();
        let start = Instant::now();
        for _ in 0..200 {
            std::hint::black_box(image::load_from_memory(&data).unwrap());
        }
        let full_decode = start.elapsed();
        let start = Instant::now();
        for _ in 0..200 {
            std::hint::black_box(images.measure(&b64).unwrap());
        }
        let header_measure = start.elapsed();
        let start = Instant::now();
        let mut peak = 0;
        for id in 0..200 {
            images.retain(|_| false);
            drop(images.decode(id, &data).unwrap());
            peak = peak.max(images.bytes);
            assert!(images.bytes <= DECODED_BYTES);
        }
        eprintln!(
            "200 800x800 plots: full raster decode={full_decode:?}, header-only layout={header_measure:?}; repeated visible plotting={:?}, retained raster peak={peak} bytes",
            start.elapsed()
        );
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
