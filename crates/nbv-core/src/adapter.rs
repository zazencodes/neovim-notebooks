//! Language projection adapters (§8) and the marker grammar (§7.1).

use crate::key::CellKey;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CellKind {
    Code,
    Markdown,
    Raw,
}

impl CellKind {
    pub fn as_nbformat(self) -> &'static str {
        match self {
            CellKind::Code => "code",
            CellKind::Markdown => "markdown",
            CellKind::Raw => "raw",
        }
    }

    pub fn from_nbformat(s: &str) -> Option<CellKind> {
        match s {
            "code" => Some(CellKind::Code),
            "markdown" => Some(CellKind::Markdown),
            "raw" => Some(CellKind::Raw),
            _ => None,
        }
    }
}

/// A parsed marker line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Marker {
    pub kind: CellKind,
    pub key: Option<CellKey>,
}

/// §8's interface. `from_buffer` keeps the spec's name despite taking `self`.
#[allow(clippy::wrong_self_convention)]
pub trait LanguageProjection {
    fn filetype(&self) -> &str;
    fn buffer_suffix(&self) -> &str;
    fn format_marker(&self, kind: CellKind, key: &CellKey) -> String;
    fn parse_marker(&self, line: &str) -> Option<Marker>;
    fn to_buffer(&self, kind: CellKind, source: &str) -> Vec<String>;
    fn from_buffer(&self, kind: CellKind, lines: &[String]) -> String;
}

/// The v1 adapter: jupytext percent format for Python.
#[derive(Clone, Copy, Debug, Default)]
pub struct PythonProjection;

const COMMENT: &str = "# ";

impl LanguageProjection for PythonProjection {
    fn filetype(&self) -> &str {
        "python"
    }

    fn buffer_suffix(&self) -> &str {
        ".py"
    }

    fn format_marker(&self, kind: CellKind, key: &CellKey) -> String {
        let id = serde_json::to_string(key.as_str()).expect("strings serialise");
        match kind {
            CellKind::Code => format!("# %% id={id}"),
            CellKind::Markdown => format!("# %% [markdown] id={id}"),
            CellKind::Raw => format!("# %% [raw] id={id}"),
        }
    }

    fn parse_marker(&self, line: &str) -> Option<Marker> {
        parse_marker_tail(line.strip_prefix("# %%")?)
    }

    fn to_buffer(&self, kind: CellKind, source: &str) -> Vec<String> {
        source
            .split('\n')
            .map(|line| {
                let line = match kind {
                    CellKind::Code => escape_magic(line),
                    CellKind::Markdown | CellKind::Raw if line.is_empty() => "#".to_string(),
                    CellKind::Markdown | CellKind::Raw => format!("{COMMENT}{line}"),
                };
                escape_marker(line)
            })
            .collect()
    }

    fn from_buffer(&self, kind: CellKind, lines: &[String]) -> String {
        let lines: Vec<String> = lines
            .iter()
            .map(|line| {
                let line = unescape_marker(line);
                match kind {
                    CellKind::Code => unescape_magic(line),
                    CellKind::Markdown | CellKind::Raw if line == "#" => String::new(),
                    CellKind::Markdown | CellKind::Raw => match line.strip_prefix(COMMENT) {
                        Some(rest) => rest.to_string(),
                        // Text the user typed without a comment prefix is taken as-is.
                        None => line.to_string(),
                    },
                }
            })
            .collect();
        lines.join("\n")
    }
}

/// Parses what follows `# %%` against `[ " [" kind "]" ] [ " id=" json-string ]`.
fn parse_marker_tail(rest: &str) -> Option<Marker> {
    let (kind, rest) = if let Some(r) = rest.strip_prefix(" [markdown]") {
        (CellKind::Markdown, r)
    } else if let Some(r) = rest.strip_prefix(" [raw]") {
        (CellKind::Raw, r)
    } else {
        (CellKind::Code, rest)
    };
    if rest.is_empty() {
        return Some(Marker { kind, key: None });
    }
    let lit = rest.strip_prefix(" id=")?;
    // serde_json tolerates surrounding whitespace; the grammar does not.
    if lit.len() < 2 || !lit.starts_with('"') || !lit.ends_with('"') {
        return None;
    }
    let key: String = serde_json::from_str(lit).ok()?;
    Some(Marker { kind, key: Some(CellKey::new(key)) })
}

/// The number of leading `"# "` prefixes on a line of the form `("# ")^j "%%" tail`,
/// where `tail` is a valid marker tail. Depth 1 is a marker; depth ≥ 2 is an escaped one.
fn marker_depth(line: &str) -> usize {
    let mut rest = line;
    let mut depth = 0;
    while let Some(r) = rest.strip_prefix(COMMENT) {
        rest = r;
        depth += 1;
    }
    match rest.strip_prefix("%%") {
        Some(tail) if depth > 0 && parse_marker_tail(tail).is_some() => depth,
        _ => 0,
    }
}

fn escape_marker(line: String) -> String {
    if marker_depth(&line) >= 1 { format!("{COMMENT}{line}") } else { line }
}

fn unescape_marker(line: &str) -> &str {
    if marker_depth(line) >= 2 { &line[COMMENT.len()..] } else { line }
}

/// Splits `indent ("# ")^k body`, returning `(indent_len, k, body)`.
fn split_comment_stack(line: &str) -> (usize, usize, &str) {
    let indent = line.len() - line.trim_start_matches([' ', '\t']).len();
    let mut rest = &line[indent..];
    let mut k = 0;
    while let Some(r) = rest.strip_prefix(COMMENT) {
        rest = r;
        k += 1;
    }
    (indent, k, rest)
}

/// Whether `body` begins an IPython magic, shell escape, or assignment from one:
/// `%name`, `%%name`, `!cmd`, `x = %name`, `x = !cmd`.
fn is_magic_body(body: &str) -> bool {
    fn starts_magic(s: &str) -> bool {
        let b = s.as_bytes();
        match b {
            [b'%', b'%', c, ..] | [b'%', c, ..] if c.is_ascii_alphabetic() || *c == b'_' => true,
            [b'!', c, ..] => !c.is_ascii_whitespace() && *c != b'=',
            _ => false,
        }
    }
    if starts_magic(body) {
        return true;
    }
    // Assignment form: identifier (with dots/commas/spaces) `=` magic.
    let Some(eq) = body.find('=') else { return false };
    let lhs = body[..eq].trim_end();
    let first = lhs.as_bytes().first();
    let lhs_ok = matches!(first, Some(c) if c.is_ascii_alphabetic() || *c == b'_')
        && lhs.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b',' | b' '));
    let rhs = &body[eq + 1..];
    lhs_ok && !rhs.starts_with('=') && starts_magic(rhs.trim_start())
}

/// jupytext-style: a magic line, commented or not, gains one `# ` after its indentation.
fn escape_magic(line: &str) -> String {
    let (indent, _, body) = split_comment_stack(line);
    if is_magic_body(body) { format!("{}{COMMENT}{}", &line[..indent], &line[indent..]) } else { line.to_string() }
}

fn unescape_magic(line: &str) -> String {
    let (indent, k, body) = split_comment_stack(line);
    if k >= 1 && is_magic_body(body) {
        format!("{}{}", &line[..indent], &line[indent + COMMENT.len()..])
    } else {
        line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn py() -> PythonProjection {
        PythonProjection
    }

    #[test]
    fn marker_grammar() {
        let p = py();
        assert_eq!(p.parse_marker("# %%"), Some(Marker { kind: CellKind::Code, key: None }));
        assert_eq!(
            p.parse_marker("# %% [markdown] id=\"ab\""),
            Some(Marker { kind: CellKind::Markdown, key: Some(CellKey::new("ab")) })
        );
        assert_eq!(p.parse_marker("# %% [raw]").unwrap().kind, CellKind::Raw);
        for not in [
            "# %% Load data",
            "# %% id=\"x\" tags=[\"a\"]",
            "# %% ",
            " # %%",
            "# %% id= \"x\"",
            "# %% id=\"x\" ",
            "# %% [code]",
            "#%%",
            "# %% id=x",
        ] {
            assert_eq!(p.parse_marker(not), None, "{not:?}");
        }
        let k = CellKey::new("we\"ird\\id");
        let m = p.format_marker(CellKind::Raw, &k);
        assert_eq!(p.parse_marker(&m), Some(Marker { kind: CellKind::Raw, key: Some(k) }));
    }

    #[test]
    fn magics_round_trip() {
        let p = py();
        let src = "%matplotlib inline\n  !ls -la\nx = !pwd\n# %time\n%%bash\nprint(1)\na != b";
        let buf = p.to_buffer(CellKind::Code, src);
        assert_eq!(buf[0], "# %matplotlib inline");
        assert_eq!(buf[1], "  # !ls -la");
        assert_eq!(buf[2], "# x = !pwd");
        assert_eq!(buf[3], "# # %time");
        assert_eq!(buf[5], "print(1)");
        assert_eq!(buf[6], "a != b");
        assert_eq!(p.from_buffer(CellKind::Code, &buf), src);
    }

    #[test]
    fn marker_like_lines_are_escaped() {
        let p = py();
        for kind in [CellKind::Code, CellKind::Markdown, CellKind::Raw] {
            let src = "# %%\n# # %% id=\"k\"\n%%\n%% [raw]";
            let buf = p.to_buffer(kind, src);
            assert!(buf.iter().all(|l| p.parse_marker(l).is_none()), "{kind:?} {buf:?}");
            assert_eq!(p.from_buffer(kind, &buf), src);
        }
    }

    #[test]
    fn markdown_accepts_uncommented_text() {
        let p = py();
        let lines = vec!["# Title".to_string(), "#".into(), "typed raw".into()];
        assert_eq!(p.from_buffer(CellKind::Markdown, &lines), "Title\n\ntyped raw");
    }
}
