#![allow(dead_code)]

use std::path::{Path, PathBuf};

use nbv_core::{CellKey, CellKind, Notebook};
use serde_json::Value;

pub fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus")
}

pub fn corpus() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(corpus_dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "ipynb"))
        .collect();
    v.sort();
    v
}

pub fn load(name: &str) -> Notebook {
    Notebook::open(&corpus_dir().join(name)).unwrap()
}

/// Copies a corpus notebook into a fresh temporary directory.
pub fn scratch_copy(name: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    std::fs::copy(corpus_dir().join(name), &path).unwrap();
    (dir, path)
}

/// Keys, kinds, sources and outputs of the live cells, in order.
#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot(pub Vec<(CellKey, CellKind, String, Value)>);

pub fn snapshot(nb: &Notebook) -> Snapshot {
    Snapshot(
        nb.order()
            .iter()
            .map(|k| {
                let c = nb.cell(k).unwrap();
                (k.clone(), c.kind(), c.source(), outputs_anywhere(nb, k))
            })
            .collect(),
    )
}

/// A cell's outputs, whether in its JSON or stashed by a type change.
pub fn outputs_anywhere(nb: &Notebook, k: &CellKey) -> Value {
    let c = nb.cell(k).unwrap();
    c.raw.get("outputs").or_else(|| c.stash.get("outputs")).cloned().unwrap_or(Value::Array(vec![]))
}
