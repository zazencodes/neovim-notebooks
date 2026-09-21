//! Property tests against `nbv-core` alone (§16.4): structural changes, undo and redo.

mod common;

use std::collections::HashSet;

use common::*;
use nbv_core::{CellKey, CellKind, Change, Notebook};
use proptest::prelude::*;
use serde_json::Value;

const KINDS: [CellKind; 3] = [CellKind::Code, CellKind::Markdown, CellKind::Raw];

/// An operation, with positions chosen relative to the notebook it is applied to.
#[derive(Clone, Debug)]
enum Op {
    Add(usize, usize),
    Delete(usize),
    Move(usize, usize),
    Kind(usize, usize),
    Split(usize, usize),
    Merge(usize),
    Paste(usize, usize),
    Type(usize, String),
    Undo,
    Redo,
}

fn op(structural_only: bool) -> BoxedStrategy<Op> {
    let n = 0..64usize;
    let structural = prop_oneof![
        (n.clone(), 0..3usize).prop_map(|(i, k)| Op::Add(i, k)),
        n.clone().prop_map(Op::Delete),
        (n.clone(), n.clone()).prop_map(|(i, j)| Op::Move(i, j)),
        (n.clone(), 0..3usize).prop_map(|(i, k)| Op::Kind(i, k)),
        (n.clone(), 0..6usize).prop_map(|(i, l)| Op::Split(i, l)),
        n.clone().prop_map(Op::Merge),
        (n.clone(), n.clone()).prop_map(|(i, j)| Op::Paste(i, j)),
    ];
    if structural_only {
        structural.boxed()
    } else {
        prop_oneof![
            4 => structural,
            1 => (n, "[a-z\n]{0,12}").prop_map(|(i, s)| Op::Type(i, s)),
            1 => Just(Op::Undo),
            1 => Just(Op::Redo),
        ]
        .boxed()
    }
}

fn nth(nb: &Notebook, i: usize) -> Option<CellKey> {
    let order = nb.order();
    (!order.is_empty()).then(|| order[i % order.len()].clone())
}

fn apply(nb: &mut Notebook, op: &Op) {
    let len = nb.order().len();
    match op {
        Op::Add(i, k) => {
            let key = nb.new_cell(KINDS[*k], "");
            nb.edit(vec![Change::Show { key, index: i % (len + 1) }]);
        }
        Op::Delete(i) => {
            if let Some(key) = nth(nb, *i) {
                nb.edit(vec![Change::Hide { key }]);
            }
        }
        Op::Move(i, j) => {
            if let Some(key) = nth(nb, *i) {
                nb.edit(vec![Change::Move { key, index: j % len }]);
            }
        }
        Op::Kind(i, k) => {
            if let Some(key) = nth(nb, *i) {
                nb.edit(vec![Change::Kind { key, kind: KINDS[*k] }]);
            }
        }
        Op::Split(i, line) => {
            if let Some(key) = nth(nb, *i) {
                let new = nb.new_cell(nb.cell(&key).unwrap().kind(), "");
                nb.edit(vec![Change::Split { key, line: *line, new }]);
            }
        }
        Op::Merge(i) => {
            if let Some(key) = nth(nb, *i)
                && let Some(next) = nb.order().get(nb.index_of(&key).unwrap() + 1).cloned()
            {
                nb.edit(vec![Change::Merge { key, next }]);
            }
        }
        Op::Paste(i, j) => {
            if let Some(src) = nth(nb, *i) {
                let raw = nb.cell(&src).unwrap().raw.clone();
                let key = nb.copy_cell(&raw);
                nb.edit(vec![Change::Show { key, index: j % (len + 1) }]);
            }
        }
        Op::Type(i, s) => {
            if let Some(key) = nth(nb, *i) {
                nb.set_source(&key, s);
            }
        }
        Op::Undo => {
            nb.undo();
        }
        Op::Redo => {
            nb.redo();
        }
    }
}

fn check_integrity(nb: &Notebook, originals: &[(CellKey, Value)]) -> Result<(), TestCaseError> {
    let keys: HashSet<&CellKey> = nb.order().iter().collect();
    prop_assert_eq!(keys.len(), nb.order().len(), "duplicate live keys");
    for k in nb.order() {
        prop_assert!(nb.is_live(k) && !nb.is_tombstoned(k));
    }
    // Only execution changes outputs, so a loaded cell's outputs survive everything else,
    // whether it is live, tombstoned, or of a type that stashes them.
    for (k, outputs) in originals {
        prop_assert!(nb.cell(k).is_some(), "cell {k} was destroyed");
        prop_assert_eq!(&outputs_anywhere(nb, k), outputs, "outputs of {} changed", k);
    }
    Ok(())
}

fn originals(nb: &Notebook) -> Vec<(CellKey, Value)> {
    nb.order().iter().map(|k| (k.clone(), outputs_anywhere(nb, k))).collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn integrity_under_arbitrary_editing(ops in prop::collection::vec(op(false), 0..40)) {
        let mut nb = load("v4.5-outputs.ipynb");
        let originals = originals(&nb);
        for op in &ops {
            apply(&mut nb, op);
            check_integrity(&nb, &originals)?;
        }
    }

    #[test]
    fn undo_reverses_and_redo_replays(ops in prop::collection::vec(op(true), 0..24)) {
        let mut nb = load("v4.5-outputs.ipynb");
        let start = snapshot(&nb);
        let mut states = vec![start.clone()];
        for op in &ops {
            apply(&mut nb, op);
            if snapshot(&nb) != *states.last().unwrap() {
                states.push(snapshot(&nb));
            }
        }
        let end = snapshot(&nb);
        // Each undo step returns to the state before its change group.
        while nb.undo().is_some() {}
        prop_assert_eq!(snapshot(&nb), start);
        while nb.redo().is_some() {}
        prop_assert_eq!(snapshot(&nb), end);
    }
}

#[test]
fn split_then_merge_round_trips_every_line() {
    for src in ["", "a", "a\n", "a\nb", "\n\n", "a\n\nb\n"] {
        for line in 0..=src.split('\n').count() {
            let json = serde_json::json!({"cells": [{"cell_type": "code", "execution_count": null, "id": "a",
                "metadata": {}, "outputs": [], "source": src}], "metadata": {}, "nbformat": 4, "nbformat_minor": 5});
            let mut nb = Notebook::from_bytes(
                std::path::Path::new("t.ipynb"),
                &serde_json::to_vec(&json).unwrap(),
                Default::default(),
            )
            .unwrap();
            let key = CellKey::new("a");
            let new = nb.new_cell(CellKind::Code, "");
            let valid = line > 0 && line < src.split('\n').count();
            assert_eq!(nb.edit(vec![Change::Split { key: key.clone(), line, new: new.clone() }]).is_some(), valid);
            if !valid {
                continue;
            }
            let (head, tail) = (nb.cell(&key).unwrap().source(), nb.cell(&new).unwrap().source());
            assert_eq!(format!("{head}\n{tail}"), src);
            nb.undo();
            assert_eq!(nb.cell(&key).unwrap().source(), src, "{src:?} at {line}: {head:?} + {tail:?}");
            assert_eq!(nb.order().len(), 1);
        }
    }
}
