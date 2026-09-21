//! The §7.2 table of common operations, one test per row.

mod common;

use common::*;
use nbv_core::{CellKind, LineEdit};

#[test]
fn typing_inside_a_cell_edits_source_and_keeps_outputs() {
    let mut nb = load("v4.5-outputs.ipynb");
    let span = nb.layout()[1].clone();
    let before = outputs_anywhere(&nb, &span.key);
    let r = nb
        .apply_edit(LineEdit { first: span.body, last: span.body + 1, lines: lines(&["import numpy as numpy"]) })
        .unwrap();
    assert!(r.changed && r.normalise.is_empty());
    assert_eq!(nb.cell(&span.key).unwrap().source(), "import numpy as numpy\nnp.arange(3)");
    assert_eq!(outputs_anywhere(&nb, &span.key), before);
    assert!(nb.cell(&span.key).unwrap().is_stale());
}

#[test]
fn whole_buffer_rewrite_preserving_comments_keeps_every_key() {
    let mut nb = load("v4.5-outputs.ipynb");
    let before = snapshot(&nb);
    let rewritten: Vec<String> =
        nb.project().into_iter().map(|l| if l.starts_with("# %%") { l } else { format!("{l}  ") }).collect();
    let r = nb.resync(rewritten);
    assert!(r.changed && r.normalise.is_empty());
    let after = snapshot(&nb);
    for (b, a) in before.0.iter().zip(&after.0) {
        assert_eq!((&b.0, &b.1, &b.3), (&a.0, &a.1, &a.3));
    }
}

#[test]
fn yank_and_paste_makes_a_fresh_copy() {
    let mut nb = load("v4.5-outputs.ipynb");
    let span = nb.layout()[1].clone();
    let cell_text = nb.mirror()[span.marker.unwrap()..span.end].to_vec();
    let end = nb.mirror().len();
    edit_normalised(&mut nb, LineEdit { first: end, last: end, lines: cell_text });
    assert_eq!(nb.order().len(), 5);
    assert_eq!(nb.order()[1], span.key, "original keeps its key");
    let copy = nb.order()[4].clone();
    assert_ne!(copy, span.key);
    assert_eq!(nb.cell(&copy).unwrap().source(), nb.cell(&span.key).unwrap().source());
    assert!(nb.cell(&copy).unwrap().outputs().is_empty());
    assert!(nb.mirror().iter().any(|l| l.contains(copy.as_str())), "copy's marker normalised");
}

#[test]
fn yank_and_paste_above_the_original_keeps_the_original() {
    let mut nb = load("v4.5-outputs.ipynb");
    let span = nb.layout()[1].clone();
    let cell_text = nb.mirror()[span.marker.unwrap()..span.end].to_vec();
    edit_normalised(&mut nb, LineEdit { first: 0, last: 0, lines: cell_text });
    assert_eq!(nb.order()[2], span.key);
    assert_ne!(nb.order()[0], span.key);
}

#[test]
fn delete_then_paste_elsewhere_moves_with_outputs() {
    let mut nb = load("v4.5-outputs.ipynb");
    let span = nb.layout()[1].clone();
    let outputs = outputs_anywhere(&nb, &span.key);
    let cell_text = nb.mirror()[span.marker.unwrap()..span.end].to_vec();
    edit_normalised(&mut nb, LineEdit { first: span.marker.unwrap(), last: span.end, lines: vec![] });
    assert!(nb.is_tombstoned(&span.key));
    let end = nb.mirror().len();
    edit_normalised(&mut nb, LineEdit { first: end, last: end, lines: cell_text });
    assert_eq!(nb.order().last(), Some(&span.key));
    assert_eq!(outputs_anywhere(&nb, &span.key), outputs);
}

#[test]
fn delete_then_undo_resurrects_with_outputs() {
    let mut nb = load("v4.5-outputs.ipynb");
    let before = snapshot(&nb);
    let text = nb.mirror().to_vec();
    let span = nb.layout()[2].clone();
    edit_normalised(&mut nb, LineEdit { first: span.marker.unwrap(), last: span.end, lines: vec![] });
    let undo = diff_edit(nb.mirror(), &text);
    edit_normalised(&mut nb, undo);
    assert_eq!(snapshot(&nb), before);
}

#[test]
fn deleting_a_marker_line_joins_body_to_previous_cell() {
    let mut nb = load("v4.5-outputs.ipynb");
    let prev = nb.layout()[1].clone();
    let gone = nb.layout()[2].clone();
    let m = gone.marker.unwrap();
    edit_normalised(&mut nb, LineEdit { first: m, last: m + 1, lines: vec![] });
    assert!(nb.is_tombstoned(&gone.key));
    assert_eq!(nb.cell(&prev.key).unwrap().source(), "import numpy as np\nnp.arange(3)\nplot()");
}

#[test]
fn typing_a_bare_marker_creates_a_cell_and_normalises_it() {
    let mut nb = load("v4.5-outputs.ipynb");
    let span = nb.layout()[1].clone();
    let r = nb.apply_edit(LineEdit { first: span.body + 1, last: span.body + 1, lines: lines(&["# %%"]) }).unwrap();
    assert!(r.changed);
    assert_eq!(nb.order().len(), 5);
    let new_key = nb.order()[2].clone();
    assert_eq!(r.normalise.len(), 1);
    assert_eq!(r.normalise[0].lines[0], format!("# %% id=\"{new_key}\""));

    // Until normalised (insert mode), edits elsewhere keep the pending key stable.
    let r2 = nb.apply_edit(LineEdit { first: span.body + 2, last: span.body + 2, lines: lines(&["y = 2"]) }).unwrap();
    assert_eq!(nb.order()[2], new_key);
    normalise(&mut nb, r2.normalise);
    assert_eq!(nb.order()[2], new_key);
    assert_eq!(nb.cell(&new_key).unwrap().source(), "y = 2\nnp.arange(3)");
}

#[test]
fn hand_editing_a_marker_to_a_live_key_gets_a_fresh_key() {
    let mut nb = load("v4.5-outputs.ipynb");
    let a = nb.layout()[1].clone();
    let b = nb.layout()[2].clone();
    let b_outputs = outputs_anywhere(&nb, &b.key);
    let m = b.marker.unwrap();
    edit_normalised(&mut nb, LineEdit { first: m, last: m + 1, lines: vec![format!("# %% id=\"{}\"", a.key)] });
    assert_eq!(nb.order()[1], a.key);
    assert!(nb.is_tombstoned(&b.key));
    let fresh = nb.order()[2].clone();
    assert_ne!(fresh, a.key);
    assert_ne!(fresh, b.key);
    assert!(nb.cell(&fresh).unwrap().outputs().is_empty());
    assert_eq!(outputs_anywhere(&nb, &b.key), b_outputs, "tombstone keeps outputs");
}

#[test]
fn changing_marker_kind_changes_cell_type_and_back_restores() {
    let mut nb = load("v4.5-outputs.ipynb");
    let before = nb.to_json();
    let span = nb.layout()[1].clone();
    let m = span.marker.unwrap();
    let md = vec![format!("# %% [markdown] id=\"{}\"", span.key)];
    edit_normalised(&mut nb, LineEdit { first: m, last: m + 1, lines: md });
    let cell = nb.cell(&span.key).unwrap();
    assert_eq!(cell.kind(), CellKind::Markdown);
    assert!(cell.raw.get("outputs").is_none() && cell.raw.get("execution_count").is_none());
    let code = vec![format!("# %% id=\"{}\"", span.key)];
    edit_normalised(&mut nb, LineEdit { first: m, last: m + 1, lines: code });
    let mut after = nb.to_json();
    // Mutation adds ids; the source document already had them.
    after["nbformat_minor"] = before["nbformat_minor"].clone();
    assert_eq!(after, before);
}

#[test]
fn leading_region_becomes_a_code_cell() {
    let mut nb = load("v4.5-outputs.ipynb");
    let r = nb.apply_edit(LineEdit { first: 0, last: 0, lines: lines(&["import os"]) }).unwrap();
    assert!(r.changed);
    let lead = nb.order()[0].clone();
    assert_eq!(nb.cell(&lead).unwrap().source(), "import os");
    // Still pending: more typing keeps the key.
    let r = nb.apply_edit(LineEdit { first: 0, last: 1, lines: lines(&["import sys"]) }).unwrap();
    assert_eq!(nb.order()[0], lead);
    normalise(&mut nb, r.normalise);
    assert_eq!(nb.mirror()[0], format!("# %% id=\"{lead}\""));
    assert_eq!(nb.cell(&lead).unwrap().source(), "import sys");
}

#[test]
fn whitespace_leading_region_is_ignored_and_empty_buffer_has_no_cells() {
    let mut nb = load("v4.5-outputs.ipynb");
    let r = nb.apply_edit(LineEdit { first: 0, last: 0, lines: lines(&["", "   "]) }).unwrap();
    assert!(r.normalise.is_empty());
    assert_eq!(nb.order().len(), 4);
    let r = nb.resync(vec![]);
    assert!(r.changed);
    assert!(nb.order().is_empty());
}

#[test]
fn out_of_range_edit_is_rejected() {
    let mut nb = load("v4.5-outputs.ipynb");
    let len = nb.mirror().len();
    assert!(nb.apply_edit(LineEdit { first: len, last: len + 1, lines: vec![] }).is_err());
}

#[test]
fn two_step_filter_insert_then_delete_keeps_every_key() {
    // Neovim's `:%!cmd` inserts the filtered text, then deletes the original, and the
    // normalisation computed in between is refused by the changedtick guard.
    let mut nb = load("v4.5-outputs.ipynb");
    let before = snapshot(&nb);
    let text = nb.mirror().to_vec();
    let n = text.len();
    let filtered: Vec<String> =
        text.iter().map(|l| if l.starts_with("# %%") { l.clone() } else { format!("{l} ") }).collect();
    let r = nb.apply_edit(LineEdit { first: n, last: n, lines: filtered }).unwrap();
    assert!(!r.normalise.is_empty(), "copies are fresh cells until the originals go");
    let r = nb.apply_edit(LineEdit { first: 0, last: n, lines: vec![] }).unwrap();
    assert!(r.normalise.is_empty(), "{:?}", r.normalise);
    let after = snapshot(&nb);
    for (b, a) in before.0.iter().zip(&after.0) {
        assert_eq!((&b.0, &b.1, &b.3), (&a.0, &a.1, &a.3));
    }
}
