//! Structural commands as buffer edits (§7.5). Each returns edits against the current text
//! (bottom-up, like normalisation) and where the cursor should go; the frontend applies them
//! to its editor and they come back through reconciliation like any other edit.

use crate::adapter::{CellKind, LanguageProjection};
use crate::notebook::{CellSpan, LineEdit, Notebook};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub edits: Vec<LineEdit>,
    /// Cursor line after the edits apply.
    pub cursor: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
}

fn span_index(nb: &Notebook, line: usize) -> Option<usize> {
    nb.layout().iter().rposition(|s| s.marker.unwrap_or(0) <= line)
}

/// The span's first line: its marker, or its body for a leading region.
fn start(s: &CellSpan) -> usize {
    s.marker.unwrap_or(s.body)
}

/// A new, empty cell below (or above) the cell at `line`.
pub fn add(nb: &mut Notebook, line: usize, above: bool) -> Plan {
    let kind = CellKind::Code;
    let key = nb.mint_key();
    let marker = nb.adapter().format_marker(kind, &key);
    let at = match span_index(nb, line).map(|i| nb.layout()[i].clone()) {
        Some(s) if above => start(&s),
        Some(s) => s.end,
        None => nb.mirror().len(),
    };
    Plan { edits: vec![LineEdit { first: at, last: at, lines: vec![marker, String::new()] }], cursor: Some(at + 1) }
}

pub fn delete(nb: &Notebook, line: usize) -> Plan {
    let Some(i) = span_index(nb, line) else { return Plan { edits: vec![], cursor: None } };
    let s = &nb.layout()[i];
    let at = start(s);
    let remaining = nb.mirror().len() - (s.end - at);
    let cursor = (remaining > 0).then(|| at.min(remaining - 1));
    Plan { edits: vec![LineEdit { first: at, last: s.end, lines: vec![] }], cursor }
}

/// Splits the cell at `line`: the lines from `line` on become a new cell of the same kind.
/// The first half keeps the key and outputs (§7.5).
pub fn split(nb: &mut Notebook, line: usize) -> Plan {
    let Some(i) = span_index(nb, line) else { return Plan { edits: vec![], cursor: None } };
    let s = nb.layout()[i].clone();
    if line < s.body || line >= s.end.max(s.body + 1) {
        return Plan { edits: vec![], cursor: None };
    }
    let key = nb.mint_key();
    let marker = nb.adapter().format_marker(s.kind, &key);
    Plan { edits: vec![LineEdit { first: line, last: line, lines: vec![marker] }], cursor: Some(line + 1) }
}

/// Merges the next cell into the cell at `line` by removing the next marker. The next cell
/// is tombstoned with its outputs (§7.5).
pub fn merge(nb: &Notebook, line: usize) -> Plan {
    let Some(i) = span_index(nb, line) else { return Plan { edits: vec![], cursor: None } };
    let Some(next) = nb.layout().get(i + 1) else { return Plan { edits: vec![], cursor: None } };
    let m = next.marker.expect("only the first span can be a leading region");
    Plan { edits: vec![LineEdit { first: m, last: m + 1, lines: vec![] }], cursor: Some(line) }
}

/// Swaps the cell at `line` with its neighbour, as one edit so both keep their identity.
pub fn swap(nb: &Notebook, line: usize, dir: Direction) -> Plan {
    let none = Plan { edits: vec![], cursor: None };
    let Some(i) = span_index(nb, line) else { return none };
    let j = match dir {
        Direction::Up if i > 0 => i - 1,
        Direction::Down if i + 1 < nb.layout().len() => i + 1,
        _ => return none,
    };
    let (a, b) = (&nb.layout()[i.min(j)], &nb.layout()[i.max(j)]);
    if a.marker.is_none() {
        return none;
    }
    let text = nb.mirror();
    let (first, mid, last) = (start(a), start(b), b.end);
    let mut lines = text[mid..last].to_vec();
    lines.extend_from_slice(&text[first..mid]);
    let offset = line - start(&nb.layout()[i]);
    let cursor = match dir {
        Direction::Up => first + offset,
        Direction::Down => first + (last - mid) + offset,
    };
    Plan { edits: vec![LineEdit { first, last, lines }], cursor: Some(cursor) }
}

/// Changes the cell at `line` to `kind`, re-projecting its body for the new kind.
pub fn set_type(nb: &Notebook, line: usize, kind: CellKind) -> Plan {
    let none = Plan { edits: vec![], cursor: None };
    let Some(i) = span_index(nb, line) else { return none };
    let s = &nb.layout()[i];
    if s.kind == kind {
        return none;
    }
    let p = nb.adapter();
    let source = p.from_buffer(s.kind, &nb.mirror()[s.body..s.end]);
    let mut lines = vec![p.format_marker(kind, &s.key)];
    lines.extend(p.to_buffer(kind, &source));
    Plan { edits: vec![LineEdit { first: start(s), last: s.end, lines }], cursor: Some(line) }
}

/// The first body line of the cell after the one at `line`, if any.
pub fn next_cell_line(nb: &Notebook, line: usize) -> Option<usize> {
    let i = span_index(nb, line).map_or(0, |i| i + 1);
    nb.layout().get(i).map(|s| s.body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Notebook;
    use std::path::Path;

    fn nb() -> Notebook {
        let json = br##"{"cells":[
            {"cell_type":"code","execution_count":1,"id":"a","metadata":{},"outputs":[{"output_type":"stream","name":"stdout","text":["x"]}],"source":["a1\n","a2"]},
            {"cell_type":"markdown","id":"b","metadata":{},"source":["# B"]},
            {"cell_type":"code","execution_count":null,"id":"c","metadata":{},"outputs":[],"source":["c1"]}
        ],"metadata":{},"nbformat":4,"nbformat_minor":5}"##;
        Notebook::from_bytes(Path::new("t.ipynb"), json, Default::default()).unwrap()
    }

    fn apply(nb: &mut Notebook, plan: Plan) {
        for e in plan.edits {
            let r = nb.apply_edit(e).unwrap();
            assert!(r.normalise.is_empty(), "structural edits are already normalised: {:?}", r.normalise);
        }
    }

    fn keys(nb: &Notebook) -> Vec<&str> {
        nb.order().iter().map(|k| k.as_str()).collect()
    }

    #[test]
    fn add_below_and_above() {
        let mut n = nb();
        let p = add(&mut n, 1, false);
        assert_eq!(p.cursor, Some(4));
        apply(&mut n, p);
        assert_eq!(n.order().len(), 4);
        assert_eq!(n.order()[0].as_str(), "a");
        let new = n.order()[1].clone();
        assert!(n.cell(&new).unwrap().source().is_empty());
        let p = add(&mut n, 0, true);
        apply(&mut n, p);
        assert_eq!(n.order()[1].as_str(), "a");
    }

    #[test]
    fn delete_split_merge() {
        let mut n = nb();
        let p = split(&mut n, 2);
        apply(&mut n, p);
        assert_eq!(n.cell(&n.order()[0].clone()).unwrap().source(), "a1");
        assert_eq!(n.cell(&n.order()[1].clone()).unwrap().source(), "a2");
        assert_eq!(n.cell(&n.order()[0].clone()).unwrap().outputs().len(), 1, "first half keeps outputs");
        let p = merge(&n, 1);
        apply(&mut n, p);
        assert_eq!(keys(&n), ["a", "b", "c"]);
        assert_eq!(n.cell(&n.order()[0].clone()).unwrap().source(), "a1\na2");
        let p = delete(&n, 3);
        apply(&mut n, p);
        assert_eq!(keys(&n), ["a", "c"]);
    }

    #[test]
    fn move_keeps_identity_and_outputs() {
        let mut n = nb();
        let p = swap(&n, 1, Direction::Down);
        assert_eq!(p.cursor, Some(3));
        apply(&mut n, p);
        assert_eq!(keys(&n), ["b", "a", "c"]);
        assert_eq!(n.cell(&n.order()[1].clone()).unwrap().outputs().len(), 1);
        let p = swap(&n, 0, Direction::Up);
        assert!(p.edits.is_empty());
    }

    #[test]
    fn change_type_reprojects_body() {
        let mut n = nb();
        let p = set_type(&n, 0, CellKind::Markdown);
        apply(&mut n, p);
        let a = n.cell(&n.order()[0].clone()).unwrap();
        assert_eq!(a.kind(), CellKind::Markdown);
        assert_eq!(a.source(), "a1\na2");
        assert_eq!(n.mirror()[1], "# a1");
        let p = set_type(&n, 0, CellKind::Code);
        apply(&mut n, p);
        assert_eq!(n.cell(&n.order()[0].clone()).unwrap().outputs().len(), 1, "outputs restored");
    }
}
