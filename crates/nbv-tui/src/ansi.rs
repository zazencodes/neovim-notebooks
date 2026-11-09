//! ANSI SGR text (tracebacks, colored stream output) into styled lines.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

fn basic(n: u16) -> Color {
    match n {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        7 => Color::Gray,
        8 => Color::DarkGray,
        9 => Color::LightRed,
        10 => Color::LightGreen,
        11 => Color::LightYellow,
        12 => Color::LightBlue,
        13 => Color::LightMagenta,
        14 => Color::LightCyan,
        _ => Color::White,
    }
}

fn apply_sgr(style: &mut Style, params: &[u16]) {
    let mut i = 0;
    let params = if params.is_empty() { &[0][..] } else { params };
    while i < params.len() {
        let p = params[i];
        match p {
            0 => *style = Style::default(),
            1 => *style = style.add_modifier(Modifier::BOLD),
            2 => *style = style.add_modifier(Modifier::DIM),
            3 => *style = style.add_modifier(Modifier::ITALIC),
            4 => *style = style.add_modifier(Modifier::UNDERLINED),
            7 => *style = style.add_modifier(Modifier::REVERSED),
            9 => *style = style.add_modifier(Modifier::CROSSED_OUT),
            22 => *style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => *style = style.remove_modifier(Modifier::ITALIC),
            24 => *style = style.remove_modifier(Modifier::UNDERLINED),
            27 => *style = style.remove_modifier(Modifier::REVERSED),
            30..=37 => *style = style.fg(basic(p - 30)),
            90..=97 => *style = style.fg(basic(p - 90 + 8)),
            40..=47 => *style = style.bg(basic(p - 40)),
            100..=107 => *style = style.bg(basic(p - 100 + 8)),
            39 => style.fg = None,
            49 => style.bg = None,
            38 | 48 => {
                let color = match params.get(i + 1) {
                    Some(5) => {
                        i += 2;
                        params.get(i).map(|n| Color::Indexed(*n as u8))
                    }
                    Some(2) => {
                        i += 4;
                        match (params.get(i - 2), params.get(i - 1), params.get(i)) {
                            (Some(r), Some(g), Some(b)) => Some(Color::Rgb(*r as u8, *g as u8, *b as u8)),
                            _ => None,
                        }
                    }
                    _ => None,
                };
                if let Some(c) = color {
                    *style = if p == 38 { style.fg(c) } else { style.bg(c) };
                }
            }
            _ => {}
        }
        i += 1;
    }
}

/// Incremental ANSI decoder and wrapped tail. Its retained text is bounded by the
/// visible row budget, even for a single logical line with millions of characters.
#[derive(Clone)]
pub struct Tail {
    base: Style,
    style: Style,
    escape: String,
    osc: bool,
    osc_esc: bool,
    cr: bool,
    rows: std::collections::VecDeque<Line<'static>>,
    current: Line<'static>,
    col: usize,
    logical_col: usize,
    total: usize,
    width: usize,
    ended: bool,
    line_rows: std::collections::VecDeque<Line<'static>>,
    line_total: usize,
    line_style: Style,
}

impl Tail {
    pub fn new(base: Style, width: u16) -> Self {
        Self {
            base,
            style: Style::default(),
            escape: String::new(),
            osc: false,
            osc_esc: false,
            cr: false,
            rows: Default::default(),
            current: Line::default(),
            col: 0,
            logical_col: 0,
            total: 0,
            width: width.max(1) as usize,
            ended: false,
            line_rows: Default::default(),
            line_total: 0,
            line_style: Style::default(),
        }
    }

    fn finish_row(&mut self) {
        self.rows.push_back(std::mem::take(&mut self.current));
        self.total += 1;
        if self.rows.len() > crate::outputs::MAX_TEXT_ROWS {
            self.rows.pop_front();
        }
        self.col = 0;
    }

    fn character(&mut self, c: char) {
        use unicode_width::UnicodeWidthChar;
        let w = c.width().unwrap_or(0);
        if self.col + w > self.width && self.col > 0 {
            self.finish_row();
        }
        let style = self.base.patch(self.style);
        if let Some(last) = self.current.spans.last_mut().filter(|s| s.style == style) {
            last.content.to_mut().push(c);
        } else {
            self.current.spans.push(Span::styled(c.to_string(), style));
        }
        self.col += w;
        self.logical_col += w;
    }

    pub fn push(&mut self, text: &str) {
        for c in text.chars() {
            self.ended = false;
            if self.cr && c != '\r' && c != '\n' {
                self.rows = self.line_rows.clone();
                self.total = self.line_total;
                self.style = self.line_style;
                self.current = Line::default();
                self.col = 0;
                self.logical_col = 0;
                self.cr = false;
            }
            if self.osc {
                if c == '\x07' || (self.osc_esc && c == '\\') {
                    self.osc = false;
                }
                self.osc_esc = c == '\x1b';
                continue;
            }
            if !self.escape.is_empty() {
                self.escape.push(c);
                if self.escape.len() == 2 {
                    match c {
                        '[' => continue,
                        ']' => {
                            self.escape.clear();
                            self.osc = true;
                            continue;
                        }
                        _ => {
                            self.escape.clear();
                            continue;
                        }
                    }
                }
                if ('@'..='~').contains(&c) {
                    if c == 'm' {
                        let seq = &self.escape[2..self.escape.len() - 1];
                        let params: Vec<u16> = seq.split(';').map(|p| p.parse().unwrap_or(0)).collect();
                        apply_sgr(&mut self.style, if seq.is_empty() { &[] } else { &params });
                    }
                    self.escape.clear();
                }
                continue;
            }
            match c {
                '\x1b' => self.escape.push(c),
                '\r' => self.cr = true,
                '\n' => {
                    self.finish_row();
                    self.logical_col = 0;
                    self.line_rows = self.rows.clone();
                    self.line_total = self.total;
                    self.line_style = self.style;
                    self.cr = false;
                    self.ended = true;
                }
                c => {
                    if c == '\t' {
                        for _ in 0..(8 - self.logical_col % 8) {
                            self.character(' ');
                        }
                    } else if !c.is_control() {
                        self.character(c);
                    }
                }
            }
        }
    }

    pub fn snapshot(&self) -> (Vec<Line<'static>>, usize) {
        let mut rows: Vec<_> = self.rows.iter().cloned().collect();
        let extra = usize::from(!self.ended);
        if extra != 0 {
            rows.push(self.current.clone());
        }
        let total = self.total + extra;
        if rows.len() > crate::outputs::MAX_TEXT_ROWS {
            rows.drain(..rows.len() - crate::outputs::MAX_TEXT_ROWS);
        }
        (rows, total)
    }
}

/// Parses text with ANSI escapes into lines. Carriage returns overwrite from the line start
/// and tabs expand to 8 columns; other control sequences are dropped.
pub fn lines(text: &str, base: Style) -> Vec<Line<'static>> {
    let mut out = vec![];
    let mut style = Style::default();
    for raw in text.split('\n') {
        // A carriage return restarts the line, as a terminal would show it.
        let raw = raw.rsplit('\r').find(|s| !s.is_empty()).unwrap_or("");
        let mut spans: Vec<Span<'static>> = vec![];
        let mut buf = String::new();
        let mut col = 0usize;
        let mut chars = raw.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\x1b' => {
                    match chars.peek() {
                        Some('[') => {
                            chars.next();
                            let mut seq = String::new();
                            let mut end = None;
                            for c in chars.by_ref() {
                                if ('@'..='~').contains(&c) {
                                    end = Some(c);
                                    break;
                                }
                                seq.push(c);
                            }
                            if end == Some('m') {
                                if !buf.is_empty() {
                                    spans.push(Span::styled(std::mem::take(&mut buf), base.patch(style)));
                                }
                                let params: Vec<u16> = seq.split(';').map(|p| p.parse().unwrap_or(0)).collect();
                                apply_sgr(&mut style, if seq.is_empty() { &[] } else { &params });
                            }
                        }
                        Some(']') => {
                            // OSC: up to BEL or ST.
                            chars.next();
                            while let Some(c) = chars.next() {
                                if c == '\x07' || (c == '\x1b' && chars.peek() == Some(&'\\')) {
                                    if c == '\x1b' {
                                        chars.next();
                                    }
                                    break;
                                }
                            }
                        }
                        _ => {
                            chars.next();
                        }
                    }
                }
                '\t' => {
                    let n = 8 - col % 8;
                    buf.extend(std::iter::repeat_n(' ', n));
                    col += n;
                }
                c if c.is_control() => {}
                c => {
                    buf.push(c);
                    col += 1;
                }
            }
        }
        if !buf.is_empty() {
            spans.push(Span::styled(buf, base.patch(style)));
        }
        out.push(Line::from(spans));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_and_resets() {
        let l = lines("\x1b[0;31mZeroDivisionError\x1b[0m: division", Style::default());
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].spans[0].content, "ZeroDivisionError");
        assert_eq!(l[0].spans[0].style.fg, Some(Color::Red));
        assert_eq!(l[0].spans[1].style.fg, None);
        let l = lines("a\x1b[38;2;1;2;3mb\x1b[38;5;200mc", Style::default());
        assert_eq!(l[0].spans[1].style.fg, Some(Color::Rgb(1, 2, 3)));
        assert_eq!(l[0].spans[2].style.fg, Some(Color::Indexed(200)));
    }

    #[test]
    fn carriage_returns_and_tabs() {
        let l = lines("10%\r50%\r100%\nx\ty", Style::default());
        assert_eq!(l[0].spans[0].content, "100%");
        assert_eq!(l[1].spans[0].content, "x       y");
    }
}
