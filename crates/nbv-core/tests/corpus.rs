//! Round-trip acceptance (§16.2), legacy upgrade (§16.3) and the save contract (§16.5).

mod common;

use common::*;
use nbv_core::{Change, CommitError, CommitOptions, Notebook, OpenError, RenameError};
use serde_json::Value;

#[test]
fn unedited_save_is_semantically_lossless_across_corpus() {
    for path in corpus() {
        let name = path.file_name().unwrap().to_str().unwrap();
        let (_dir, copy) = scratch_copy(name);
        let before: Value = serde_json::from_slice(&std::fs::read(&copy).unwrap()).unwrap();
        let mut nb = Notebook::open(&copy).unwrap();
        nb.commit(CommitOptions::default()).unwrap();
        let after: Value = serde_json::from_slice(&std::fs::read(&copy).unwrap()).unwrap();
        assert_eq!(before, after, "{name} lost information");
    }
}

#[test]
fn jupyter_formatted_notebooks_save_byte_identical() {
    for name in ["legacy-4.4-outputs.ipynb", "v4.5-outputs.ipynb", "magics-and-markers.ipynb", "huge-output.ipynb"] {
        let (_dir, copy) = scratch_copy(name);
        let before = std::fs::read(&copy).unwrap();
        Notebook::open(&copy).unwrap().commit(CommitOptions::default()).unwrap();
        assert_eq!(before, std::fs::read(&copy).unwrap(), "{name}");
    }
}

#[test]
fn rewriting_every_source_unchanged_is_not_a_mutation() {
    for path in corpus() {
        let mut nb = Notebook::open(&path).unwrap();
        let before = snapshot(&nb);
        for k in nb.order().to_vec() {
            let src = nb.cell(&k).unwrap().source();
            assert!(!nb.set_source(&k, &src), "{path:?} {k}");
        }
        assert_eq!(snapshot(&nb), before, "{path:?}");
        assert!(!nb.is_mutated());
    }
}

#[test]
fn legacy_unedited_save_injects_nothing() {
    let (_dir, copy) = scratch_copy("legacy-4.4-outputs.ipynb");
    Notebook::open(&copy).unwrap().commit(CommitOptions::default()).unwrap();
    let v: Value = serde_json::from_slice(&std::fs::read(&copy).unwrap()).unwrap();
    assert_eq!(v["nbformat_minor"], 4);
    assert!(v["cells"].as_array().unwrap().iter().all(|c| c.get("id").is_none()));
}

#[test]
fn legacy_upgrades_on_first_mutation() {
    let (_dir, copy) = scratch_copy("legacy-4.4-outputs.ipynb");
    let before: Value = serde_json::from_slice(&std::fs::read(&copy).unwrap()).unwrap();
    let mut nb = Notebook::open(&copy).unwrap();
    // Append a line to the second cell.
    let k = nb.order()[1].clone();
    let src = nb.cell(&k).unwrap().source();
    assert!(nb.set_source(&k, &format!("{src}\nx = 1")));
    let keys: Vec<String> = nb.order().iter().map(|k| k.to_string()).collect();
    nb.commit(CommitOptions::default()).unwrap();

    let after: Value = serde_json::from_slice(&std::fs::read(&copy).unwrap()).unwrap();
    assert_eq!(after["nbformat_minor"], 5);
    let cells = after["cells"].as_array().unwrap();
    for (c, k) in cells.iter().zip(&keys) {
        assert_eq!(c["id"], k.as_str());
    }
    // No other change beyond the mutation itself.
    let mut expected = before.clone();
    expected["nbformat_minor"] = 5.into();
    for (i, c) in expected["cells"].as_array_mut().unwrap().iter_mut().enumerate() {
        c["id"] = keys[i].clone().into();
    }
    expected["cells"][1]["source"] =
        serde_json::json!(["import numpy as np\n", "print('hello')\n", "print('world')\n", "x = 1"]);
    assert_eq!(after, expected);
}

#[test]
fn bad_ids_get_synthesized_keys_and_are_rewritten_on_mutation() {
    let mut nb = load("ids-missing-duplicate-invalid.ipynb");
    let keys: Vec<String> = nb.order().iter().map(|k| k.to_string()).collect();
    assert_eq!(keys[5], "ok-id_1");
    for (i, k) in keys.iter().enumerate() {
        if i != 5 {
            assert_eq!(k.len(), 8, "{k}");
        }
    }
    assert!(nb.to_json()["cells"][0]["id"] == "dup", "unmutated document is untouched");
    let last = nb.order().last().unwrap().clone();
    nb.set_source(&last, "# more");
    let json = nb.to_json();
    for (c, k) in json["cells"].as_array().unwrap().iter().zip(&keys) {
        assert_eq!(c["id"], k.as_str());
    }
}

#[test]
fn refuses_unknown_cell_types_and_missing_metadata() {
    let err = Notebook::open(&corpus_dir().join("refused/unknown-cell-type.ipynb")).unwrap_err();
    assert!(matches!(err, OpenError::UnknownCellType(_, 1, _)), "{err}");
    assert!(err.to_string().contains("\"heading\""), "{err}");
    let json = br#"{"cells": [], "nbformat": 4, "nbformat_minor": 5}"#;
    let err = Notebook::from_bytes(std::path::Path::new("t.ipynb"), json, Default::default()).unwrap_err();
    assert!(matches!(err, OpenError::NotANotebook(..)), "{err}");
}

#[test]
fn choosing_a_kernel_records_its_kernelspec() {
    let (_dir, copy) = scratch_copy("v4.5-outputs.ipynb");
    let mut nb = Notebook::open(&copy).unwrap();
    nb.set_kernelspec("python3", "Python 3", "python");
    assert!(!nb.is_mutated(), "the same kernelspec is no change");
    nb.set_kernelspec("ir", "R", "r");
    assert!(nb.is_mutated());
    nb.commit(CommitOptions::default()).unwrap();
    let v: Value = serde_json::from_slice(&std::fs::read(&copy).unwrap()).unwrap();
    assert_eq!(v["metadata"]["kernelspec"], serde_json::json!({"display_name": "R", "language": "r", "name": "ir"}));
}

#[test]
fn refuses_non_v4() {
    let err = Notebook::open(&corpus_dir().join("refused/nbformat3.ipynb")).unwrap_err();
    assert!(matches!(err, OpenError::UnsupportedNbformat(..)), "{err}");
}

#[test]
fn refuses_to_overwrite_a_notebook_changed_on_disk() {
    let (_dir, copy) = scratch_copy("v4.5-outputs.ipynb");
    let mut nb = Notebook::open(&copy).unwrap();
    std::fs::write(&copy, b"{\"changed\": true}").unwrap();
    let err = nb.commit(CommitOptions::default()).unwrap_err();
    assert!(matches!(err, CommitError::ChangedOnDisk(_)));
    assert_eq!(std::fs::read(&copy).unwrap(), b"{\"changed\": true}");
    nb.commit(CommitOptions { force: true }).unwrap();
    // After a successful write, the new content is the baseline.
    nb.commit(CommitOptions::default()).unwrap();
}

#[test]
fn writes_through_symlinks_and_preserves_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, copy) = scratch_copy("v4.5-outputs.ipynb");
    std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o640)).unwrap();
    let link = dir.path().join("link.ipynb");
    std::os::unix::fs::symlink(&copy, &link).unwrap();
    let mut nb = Notebook::open(&link).unwrap();
    let first = nb.order()[0].clone();
    nb.set_source(&first, "#");
    nb.commit(CommitOptions::default()).unwrap();
    assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
    assert_eq!(std::fs::metadata(&copy).unwrap().permissions().mode() & 0o777, 0o640);
    let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(names.len(), 2, "no stray temporary files: {names:?}");
}

#[test]
fn reload_discards_tombstones_and_rebuilds() {
    let (_dir, copy) = scratch_copy("v4.5-outputs.ipynb");
    let mut nb = Notebook::open(&copy).unwrap();
    let original = snapshot(&nb);
    let key = nb.order()[1].clone();
    nb.edit(vec![Change::Hide { key: key.clone() }]);
    assert!(nb.is_tombstoned(&key));
    nb.reload().unwrap();
    assert_eq!(snapshot(&nb), original);
    assert!(nb.is_live(&key));
    assert!(!nb.is_mutated());
    assert_eq!(nb.undo(), None, "history is discarded");
}

#[test]
fn rename_moves_the_file_and_later_writes_follow_it() {
    let (dir, copy) = scratch_copy("v4.5-outputs.ipynb");
    let before = std::fs::read(&copy).unwrap();
    let mut nb = Notebook::open(&copy).unwrap();
    for bad in ["", ".", "..", "sub/x.ipynb", "/tmp/x.ipynb"] {
        assert!(matches!(nb.rename(bad), Err(RenameError::NotAFileName(_))), "{bad:?}");
    }
    std::fs::write(dir.path().join("taken.ipynb"), "{}").unwrap();
    assert!(matches!(nb.rename("taken.ipynb"), Err(RenameError::Exists(_))));
    assert_eq!(nb.path(), copy);

    nb.rename("renamed.ipynb").unwrap();
    let to = dir.path().join("renamed.ipynb");
    assert_eq!(nb.path(), to);
    assert!(!copy.exists());
    assert_eq!(std::fs::read(&to).unwrap(), before);
    // The file on disk is the one loaded, so writing needs no force.
    nb.commit(CommitOptions::default()).unwrap();
    assert!(!copy.exists());
}
