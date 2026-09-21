//! Property tests against `nbv-core` alone (§16.4). Spike 4's kill gate.

mod common;

use std::collections::{HashMap, HashSet};

use common::*;
use nbv_core::{CellKey, CellKind, LanguageProjection, LineEdit, Notebook, PythonProjection};
use proptest::prelude::*;
use serde_json::Value;

const KINDS: [CellKind; 3] = [CellKind::Code, CellKind::Markdown, CellKind::Raw];

/// Source text built from fragments that stress the escaping rules.
fn source() -> impl Strategy<Value = String> {
    let frag = prop_oneof![
        Just("# ".to_string()),
        Just("#".to_string()),
        Just("%%".to_string()),
        Just("%".to_string()),
        Just("%time".to_string()),
        Just("!".to_string()),
        Just("!ls".to_string()),
        Just(" id=\"k\"".to_string()),
        Just(" [markdown]".to_string()),
        Just(" [raw]".to_string()),
        Just("x = ".to_string()),
        Just("\n".to_string()),
        Just("\r".to_string()),
        Just(" ".to_string()),
        Just("\t".to_string()),
        "[a-z]{1,3}",
        Just("é名".to_string()),
    ];
    prop::collection::vec(frag, 0..24).prop_map(|v| v.concat())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4000))]

    #[test]
    fn adapter_inverse(kind in prop::sample::select(KINDS.to_vec()), s in source()) {
        let p = PythonProjection;
        let buf = p.to_buffer(kind, &s);
        prop_assert!(buf.iter().all(|l| p.parse_marker(l).is_none()), "{buf:?}");
        prop_assert!(buf.iter().all(|l| !l.contains('\n')));
        prop_assert_eq!(p.from_buffer(kind, &buf), s);
    }
}

#[test]
fn adapter_inverse_over_corpus_cells() {
    let p = PythonProjection;
    for path in corpus() {
        let nb = Notebook::open(&path).unwrap();
        for k in nb.order() {
            let c = nb.cell(k).unwrap();
            let src = c.source();
            assert_eq!(p.from_buffer(c.kind(), &p.to_buffer(c.kind(), &src)), src, "{path:?} {k}");
        }
    }
}

/// A line to insert, chosen relative to the current state.
#[derive(Clone, Debug)]
enum LineGen {
    Text(String),
    /// A marker with no key.
    Bare(usize),
    /// A marker showing a key the session has seen (live or tombstoned).
    Seen(usize, usize),
    /// A marker showing an unknown key.
    Unknown(usize, u8),
    /// A copy of an existing line.
    Copy(usize),
}

#[derive(Clone, Debug)]
enum Op {
    Insert { at: usize, lines: Vec<LineGen> },
    Replace { at: usize, len: usize, lines: Vec<LineGen> },
    Delete { at: usize, len: usize },
    /// Yank a range and paste it elsewhere.
    Duplicate { at: usize, len: usize, to: usize },
    /// Delete a range and paste it elsewhere: two edits.
    Move { at: usize, len: usize, to: usize },
    /// Whole-buffer replacement that keeps marker lines and rewrites bodies.
    Rewrite { suffix: String },
    /// Return to an earlier normalised state as a single edit, as undo does.
    Undo { back: usize },
    /// Return to an earlier normalised state as a whole-buffer resync.
    Resync { back: usize },
}

fn line_gen() -> impl Strategy<Value = LineGen> + Clone {
    prop_oneof![
        4 => "[a-z =%!#]{0,8}".prop_map(LineGen::Text),
        1 => (0..3usize).prop_map(LineGen::Bare),
        2 => (0..3usize, any::<usize>()).prop_map(|(k, i)| LineGen::Seen(k, i)),
        1 => (0..3usize, 0..4u8).prop_map(|(k, i)| LineGen::Unknown(k, i)),
        2 => any::<usize>().prop_map(LineGen::Copy),
    ]
}

fn op() -> impl Strategy<Value = Op> {
    let lines = prop::collection::vec(line_gen(), 0..5);
    prop_oneof![
        3 => (any::<usize>(), lines.clone()).prop_map(|(at, lines)| Op::Insert { at, lines }),
        3 => (any::<usize>(), 0..4usize, lines).prop_map(|(at, len, lines)| Op::Replace { at, len, lines }),
        2 => (any::<usize>(), 1..8usize).prop_map(|(at, len)| Op::Delete { at, len }),
        2 => (any::<usize>(), 1..8usize, any::<usize>()).prop_map(|(at, len, to)| Op::Duplicate { at, len, to }),
        2 => (any::<usize>(), 1..8usize, any::<usize>()).prop_map(|(at, len, to)| Op::Move { at, len, to }),
        1 => "[ x#]{0,3}".prop_map(|suffix| Op::Rewrite { suffix }),
        2 => (1..6usize).prop_map(|back| Op::Undo { back }),
        1 => (1..6usize).prop_map(|back| Op::Resync { back }),
    ]
}

struct Harness {
    nb: Notebook,
    /// Every key the session has seen, in first-seen order.
    seen: Vec<CellKey>,
    /// Outputs each cell had at load. No execution happens, so they must never change.
    outputs: HashMap<CellKey, Value>,
    /// Normalised states: text and the document it corresponds to.
    history: Vec<(Vec<String>, Snapshot)>,
}

impl Harness {
    fn new(nb: Notebook) -> Harness {
        let seen = nb.order().to_vec();
        let outputs = seen.iter().map(|k| (k.clone(), outputs_anywhere(&nb, k))).collect();
        let history = vec![(nb.mirror().to_vec(), snapshot(&nb))];
        Harness { nb, seen, outputs, history }
    }

    fn render(&self, g: &LineGen) -> String {
        let p = PythonProjection;
        let text = self.nb.mirror();
        match g {
            LineGen::Text(s) => s.clone(),
            LineGen::Bare(k) => match KINDS[*k] {
                CellKind::Code => "# %%".into(),
                CellKind::Markdown => "# %% [markdown]".into(),
                CellKind::Raw => "# %% [raw]".into(),
            },
            LineGen::Seen(k, i) if !self.seen.is_empty() => p.format_marker(KINDS[*k], &self.seen[i % self.seen.len()]),
            LineGen::Seen(k, _) => self.render(&LineGen::Bare(*k)),
            LineGen::Unknown(k, i) => p.format_marker(KINDS[*k], &CellKey::new(format!("unknown{i}"))),
            LineGen::Copy(i) if !text.is_empty() => text[i % text.len()].clone(),
            LineGen::Copy(_) => String::new(),
        }
    }

    fn edit(&mut self, e: LineEdit) {
        let mut expect = self.nb.mirror().to_vec();
        expect.splice(e.first..e.last, e.lines.clone());
        let r = self.nb.apply_edit(e).unwrap();
        assert_eq!(self.nb.mirror(), expect.as_slice());
        normalise(&mut self.nb, r.normalise);
        for k in self.nb.order() {
            if !self.seen.contains(k) {
                self.seen.push(k.clone());
            }
        }
        self.check_integrity();
    }

    fn check_integrity(&self) {
        let nb = &self.nb;
        let p = PythonProjection;
        let order: HashSet<&CellKey> = nb.order().iter().collect();
        assert_eq!(order.len(), nb.order().len(), "two live cells share a key");
        assert!(nb.order().iter().all(|k| nb.is_live(k) && !nb.is_tombstoned(k)));
        // Normalised: every marker shows its key, in order, and there is no leading cell.
        let shown: Vec<CellKey> =
            nb.mirror().iter().filter_map(|l| p.parse_marker(l)).map(|m| m.key.expect("normalised")).collect();
        assert_eq!(shown, nb.order());
        // No output belonging to a surviving cell is ever lost.
        for (k, v) in &self.outputs {
            if nb.cell(k).is_some() {
                assert_eq!(&outputs_anywhere(nb, k), v, "outputs of {k} changed");
            }
        }
        // The document matches the text.
        for span in nb.layout() {
            let c = nb.cell(&span.key).unwrap();
            assert_eq!(c.kind(), span.kind);
            assert_eq!(c.source(), p.from_buffer(span.kind, &nb.mirror()[span.body..span.end]));
        }
    }

    fn run(&mut self, op: Op) {
        let len = self.nb.mirror().len();
        let pos = |x: usize| x % (len + 1);
        match op {
            Op::Insert { at, lines } => {
                let lines = lines.iter().map(|g| self.render(g)).collect();
                self.edit(LineEdit { first: pos(at), last: pos(at), lines });
            }
            Op::Replace { at, len: n, lines } => {
                let first = pos(at);
                let lines = lines.iter().map(|g| self.render(g)).collect();
                self.edit(LineEdit { first, last: (first + n).min(len), lines });
            }
            Op::Delete { at, len: n } => {
                let first = pos(at);
                self.edit(LineEdit { first, last: (first + n).min(len), lines: vec![] });
            }
            Op::Duplicate { at, len: n, to } => {
                let first = pos(at);
                let yank = self.nb.mirror()[first..(first + n).min(len)].to_vec();
                let to = pos(to);
                self.edit(LineEdit { first: to, last: to, lines: yank });
            }
            Op::Move { at, len: n, to } => {
                let first = pos(at);
                let last = (first + n).min(len);
                let yank = self.nb.mirror()[first..last].to_vec();
                self.edit(LineEdit { first, last, lines: vec![] });
                let to = to % (self.nb.mirror().len() + 1);
                self.edit(LineEdit { first: to, last: to, lines: yank });
            }
            Op::Rewrite { suffix } => {
                let p = PythonProjection;
                let before: Vec<CellKey> = self.nb.order().to_vec();
                // Lines above the first marker stay as they are: filling them would add a cell.
                let first = self.nb.mirror().iter().position(|l| p.parse_marker(l).is_some());
                let text: Vec<String> = self
                    .nb
                    .mirror()
                    .iter()
                    .enumerate()
                    .map(|(i, l)| {
                        if first.is_none_or(|f| i <= f) || p.parse_marker(l).is_some() {
                            l.clone()
                        } else {
                            format!("{l}{suffix}")
                        }
                    })
                    .collect();
                // Suffixing can turn a body line into a marker; only marker-preserving rewrites count.
                let markers = |t: &[String]| t.iter().filter(|l| p.parse_marker(l).is_some()).count();
                if markers(&text) == markers(self.nb.mirror()) {
                    let r = self.nb.resync(text);
                    normalise(&mut self.nb, r.normalise);
                    assert_eq!(self.nb.order(), before.as_slice(), "rewrite lost identity");
                    self.check_integrity();
                }
            }
            Op::Undo { back } | Op::Resync { back } => {
                let idx = self.history.len().saturating_sub(back + 1);
                let (text, snap) = self.history[idx].clone();
                if matches!(op, Op::Undo { .. }) {
                    let e = diff_edit(self.nb.mirror(), &text);
                    self.edit(e);
                } else {
                    let r = self.nb.resync(text);
                    normalise(&mut self.nb, r.normalise);
                    self.check_integrity();
                }
                assert_eq!(snapshot(&self.nb), snap, "returning the text did not return the document");
            }
        }
        self.history.push((self.nb.mirror().to_vec(), snapshot(&self.nb)));
    }
}

fn corpus_names() -> Vec<String> {
    corpus()
        .into_iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .filter(|n| n != "huge-output.ipynb")
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(3000))]

    #[test]
    fn random_edit_sequences_preserve_integrity_and_reversibility(
        name in prop::sample::select(corpus_names()),
        ops in prop::collection::vec(op(), 1..30),
    ) {
        let mut h = Harness::new(load(&name));
        h.check_integrity();
        for op in ops {
            h.run(op);
        }
    }
}
