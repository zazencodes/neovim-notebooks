//! Crash recovery, separate from saving the notebook. One atomic manifest per absolute
//! notebook path; unchanged output blobs are shared between successive snapshots so typing
//! beside a large image does not repeatedly clone or JSON-encode that image.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::{CellKey, Notebook};

type Result<T> = std::result::Result<T, RecoveryError>;

#[derive(Debug, thiserror::Error)]
#[error("recovery for {notebook} at {file}: {reason}")]
pub struct RecoveryError {
    notebook: PathBuf,
    file: PathBuf,
    reason: String,
}

#[derive(Clone)]
pub struct RecoveryStore {
    dir: PathBuf,
    notebook: PathBuf,
    opened_path: PathBuf,
    _lock: Arc<fs::File>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    notebook: PathBuf,
    baseline: Option<[u8; 32]>,
    trailing_newline: bool,
    top: Map<String, Value>,
    cells: Vec<SavedCell>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedCell {
    raw: Map<String, Value>,
    outputs: Option<String>,
}

struct Outputs {
    value: Value,
    hash: OnceLock<String>,
}

/// Cache belongs to the editor thread. Capture only copies sources and small cell fields;
/// the worker owns shared immutable output values and performs encoding and durable I/O.
#[derive(Default)]
pub struct SnapshotCache {
    outputs: HashMap<CellKey, Arc<Outputs>>,
}

pub struct Snapshot {
    manifest: Manifest,
    outputs: Vec<Option<Arc<Outputs>>>,
}

impl SnapshotCache {
    pub fn capture(&mut self, nb: &Notebook) -> Snapshot {
        let mut outputs = Vec::with_capacity(nb.order().len());
        let mut cells = Vec::with_capacity(nb.order().len());
        self.outputs.retain(|key, _| nb.is_live(key));
        for key in nb.order() {
            let cell = nb.cell(key).expect("live cell");
            let output =
                cell.raw.get("outputs").filter(|v| !matches!(v, Value::Array(a) if a.is_empty())).map(|value| {
                    let cached = self
                        .outputs
                        .entry(key.clone())
                        .or_insert_with(|| Arc::new(Outputs { value: value.clone(), hash: OnceLock::new() }));
                    if cached.value != *value {
                        *cached = Arc::new(Outputs { value: value.clone(), hash: OnceLock::new() });
                    }
                    cached.clone()
                });
            if output.is_none() {
                self.outputs.remove(key);
            }
            let mut raw: Map<String, Value> = cell
                .raw
                .iter()
                .filter(|(k, _)| k.as_str() != "outputs" || output.is_none())
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            // Match canonical persistence, including stable ids for older notebooks.
            raw.insert("id".into(), key.as_str().into());
            cells.push(SavedCell { raw, outputs: None });
            outputs.push(output);
        }
        let mut top = nb.top.clone();
        if top.get("nbformat_minor").and_then(Value::as_u64).is_none_or(|m| m < 5) {
            top.insert("nbformat_minor".into(), 5.into());
        }
        Snapshot {
            manifest: Manifest {
                version: 1,
                notebook: nb.path().into(),
                baseline: nb.disk_hash,
                trailing_newline: nb.trailing_newline,
                top,
                cells,
            },
            outputs,
        }
    }
}

impl RecoveryStore {
    pub fn open(notebook: &Path) -> Result<Self> {
        let var = |key| std::env::var_os(key).filter(|s| !s.is_empty()).map(PathBuf::from);
        let state = var("XDG_STATE_HOME").or_else(|| var("HOME").map(|p| p.join(".local/state")));
        match state {
            Some(state) => Self::at(&state.join("nbv/recovery"), notebook),
            None => Err(RecoveryError {
                notebook: notebook.into(),
                file: PathBuf::from("$XDG_STATE_HOME/nbv/recovery"),
                reason: "neither XDG_STATE_HOME nor HOME is set".into(),
            }),
        }
    }

    /// `root` is injectable so tests never share state directories.
    pub fn at(root: &Path, notebook: &Path) -> Result<Self> {
        let opened_path = std::path::absolute(notebook).map_err(|e| RecoveryError {
            notebook: notebook.into(),
            file: root.into(),
            reason: e.to_string(),
        })?;
        let notebook = canonical_notebook_path(notebook).map_err(|e| RecoveryError {
            notebook: notebook.into(),
            file: root.into(),
            reason: e.to_string(),
        })?;
        if notebook.to_str().is_none() {
            return Err(RecoveryError {
                notebook,
                file: root.into(),
                reason: "notebook path is not valid UTF-8".into(),
            });
        }
        let hash = digest(notebook.as_os_str().as_encoded_bytes());
        let lock_path = root.join(format!("{hash}.lock"));
        let fail = |e: std::io::Error| RecoveryError {
            notebook: notebook.clone(),
            file: lock_path.clone(),
            reason: e.to_string(),
        };
        create_state_dir(root).map_err(fail)?;
        let lock = fs::OpenOptions::new().create(true).truncate(false).write(true).open(&lock_path).map_err(fail)?;
        lock.try_lock().map_err(|e| RecoveryError {
            notebook: notebook.clone(),
            file: lock_path,
            reason: format!("cannot acquire recovery lock (is this notebook already open?): {e}"),
        })?;
        let store = Self { dir: root.join(hash), notebook, opened_path, _lock: Arc::new(lock) };
        let discarded = store.dir.with_extension("discarded");
        match fs::remove_dir_all(&discarded) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(store.error(&discarded, e)),
        }
        Ok(store)
    }

    /// Moving an opened file symlink changes its operational name, while the canonical
    /// target and held lock stay the same. No second acquisition or snapshot deletion.
    pub fn follow_file_link_rename(&mut self, opened_path: &Path) -> Result<()> {
        let identity = canonical_notebook_path(opened_path).map_err(|e| self.error(opened_path, e))?;
        if identity != self.notebook {
            return Err(self.error(opened_path, "renamed file link no longer refers to the held notebook"));
        }
        self.opened_path = std::path::absolute(opened_path).map_err(|e| self.error(opened_path, e))?;
        Ok(())
    }

    pub fn conflict_error(&self) -> RecoveryError {
        self.error(
            &self.manifest_path(),
            "an unsaved snapshot already exists; open that notebook to recover or discard it before renaming",
        )
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.dir.join("snapshot.json")
    }

    fn error(&self, file: &Path, reason: impl std::fmt::Display) -> RecoveryError {
        RecoveryError { notebook: self.opened_path.clone(), file: file.into(), reason: reason.to_string() }
    }

    pub fn load(&self) -> Result<Option<Notebook>> {
        let file = self.manifest_path();
        let bytes = match fs::read(&file) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(self.error(&file, e)),
        };
        let mut manifest: Manifest = serde_json::from_slice(&bytes).map_err(|e| self.error(&file, e))?;
        if manifest.version != 1 || manifest.notebook != self.notebook {
            return Err(self.error(&file, "unsupported version or mismatched notebook path"));
        }
        let mut cells = Vec::with_capacity(manifest.cells.len());
        for mut cell in manifest.cells {
            if let Some(hash) = cell.outputs {
                if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(self.error(&file, "invalid output blob reference"));
                }
                let blob = self.dir.join(format!("{hash}.json"));
                let bytes = fs::read(&blob).map_err(|e| self.error(&blob, e))?;
                if digest(&bytes) != hash {
                    return Err(self.error(&blob, "output blob checksum mismatch"));
                }
                let value = serde_json::from_slice(&bytes).map_err(|e| self.error(&blob, e))?;
                cell.raw.insert("outputs".into(), value);
            }
            cells.push(Value::Object(cell.raw));
        }
        manifest.top.insert("cells".into(), cells.into());
        let mut bytes = serde_json::to_vec(&manifest.top).expect("JSON value serializes");
        if manifest.trailing_newline {
            bytes.push(b'\n');
        }
        Notebook::recover(&self.opened_path, &bytes, manifest.baseline).map(Some).map_err(|e| self.error(&file, e))
    }

    /// Call on a worker thread. Publish output blobs first, then atomically replace the
    /// manifest, and only then collect blobs the new manifest no longer references.
    pub fn persist(&self, mut snapshot: Snapshot) -> Result<()> {
        let file = self.manifest_path();
        let association = canonical_notebook_path(&snapshot.manifest.notebook).map_err(|e| self.error(&file, e))?;
        if association != self.notebook {
            return Err(self.error(&file, "snapshot belongs to a different notebook"));
        }
        snapshot.manifest.notebook = self.notebook.clone();
        fs::create_dir_all(&self.dir).map_err(|e| self.error(&file, e))?;
        fs::File::open(self.dir.parent().expect("recovery root"))
            .and_then(|d| d.sync_all())
            .map_err(|e| self.error(&file, e))?;
        let mut keep = HashSet::new();
        for (cell, output) in snapshot.manifest.cells.iter_mut().zip(snapshot.outputs) {
            if let Some(output) = output {
                let hash = if let Some(hash) = output.hash.get().filter(|h| self.dir.join(format!("{h}.json")).exists())
                {
                    hash.clone()
                } else {
                    let bytes = serde_json::to_vec(&output.value).expect("JSON value serializes");
                    let hash = digest(&bytes);
                    let blob = self.dir.join(format!("{hash}.json"));
                    if !blob.exists() {
                        atomic_write(&blob, &bytes).map_err(|e| self.error(&file, e))?;
                    } else if !blob.is_file() {
                        return Err(self.error(&blob, "output blob is not a file"));
                    }
                    let _ = output.hash.set(hash.clone());
                    hash
                };
                keep.insert(format!("{hash}.json"));
                cell.outputs = Some(hash);
            }
        }
        let bytes = serde_json::to_vec(&snapshot.manifest).expect("manifest serializes");
        atomic_write(&file, &bytes).map_err(|e| self.error(&file, e))?;
        for entry in fs::read_dir(&self.dir).map_err(|e| self.error(&file, e))? {
            let entry = entry.map_err(|e| self.error(&file, e))?;
            let name = entry.file_name();
            if name != "snapshot.json" && !keep.contains(&name.to_string_lossy().into_owned()) {
                fs::remove_file(entry.path()).map_err(|e| self.error(&entry.path(), e))?;
            }
        }
        Ok(())
    }

    /// Saving, accepting an external reload, or explicitly discarding recovery removes it.
    pub fn clear(&self) -> Result<()> {
        let parent = self.dir.parent().expect("recovery root");
        let discarded = self.dir.with_extension("discarded");
        // Unpublish the whole manifest/blob set together. A crash during recursive deletion
        // must not leave a visible manifest referring to half-deleted output blobs.
        match fs::remove_dir_all(&discarded) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(self.error(&discarded, e)),
        }
        match fs::rename(&self.dir, &discarded) {
            Ok(()) => {
                fs::File::open(parent).and_then(|d| d.sync_all()).map_err(|e| self.error(parent, e))?;
                fs::remove_dir_all(&discarded).map_err(|e| self.error(&discarded, e))?;
                fs::File::open(parent).and_then(|d| d.sync_all()).map_err(|e| self.error(parent, e))?;
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(self.error(&self.dir, e)),
        }
    }
}

/// One identity for existing files and symlink aliases. A missing notebook retains its
/// resolved parent/target identity so a snapshot is still discoverable after deletion.
/// Hard-link aliases are separate paths: notebook saves replace one directory entry.
pub fn canonical_notebook_path(path: &Path) -> std::io::Result<PathBuf> {
    fn resolve(path: &Path, depth: usize) -> std::io::Result<PathBuf> {
        if depth > 64 {
            return Err(std::io::Error::other("too many path or symlink components"));
        }
        match fs::canonicalize(path) {
            Ok(path) => Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let parent = path.parent().ok_or(e)?;
                if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
                    let target = fs::read_link(path)?;
                    return resolve(&parent.join(target), depth + 1);
                }
                let name = path.file_name().ok_or_else(|| std::io::Error::other("invalid notebook path"))?;
                Ok(resolve(parent, depth + 1)?.join(name))
            }
            Err(e) => Err(e),
        }
    }
    resolve(&std::path::absolute(path)?, 0)
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn create_state_dir(path: &Path) -> std::io::Result<()> {
    let mut missing = Vec::new();
    let mut dir = path;
    while !dir.exists() && !dir.as_os_str().is_empty() {
        missing.push(dir);
        let Some(parent) = dir.parent() else { break };
        dir = parent;
    }
    fs::create_dir_all(path)?;
    for dir in missing.into_iter().rev() {
        let parent = dir.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().expect("state file parent");
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    fs::File::open(dir)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CellKind, Change, CommitError, CommitOptions};
    use serde_json::json;

    fn fixture() -> (tempfile::TempDir, Notebook, RecoveryStore) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.ipynb");
        fs::write(
            &path,
            serde_json::to_vec(&json!({
                "nbformat": 4, "nbformat_minor": 5, "metadata": {"custom": [1, 2]},
                "cells": [{"id": "code", "cell_type": "code", "source": "saved", "metadata": {},
                    "execution_count": 3, "outputs": [{"output_type": "display_data", "metadata": {},
                        "data": {"image/png": "original image", "text/plain": "plot"}}]}]
            }))
            .unwrap(),
        )
        .unwrap();
        let nb = Notebook::open(&path).unwrap();
        let store = RecoveryStore::at(&dir.path().join("state"), &path).unwrap();
        (dir, nb, store)
    }

    #[test]
    fn latest_source_structure_and_outputs_survive_reopening_without_touching_disk() {
        let (dir, mut nb, store) = fixture();
        let original = fs::read(nb.path()).unwrap();
        let key = nb.order()[0].clone();
        let mut cache = SnapshotCache::default();
        nb.set_source(&key, "insert mode text");
        store.persist(cache.capture(&nb)).unwrap();
        nb.set_source(&key, "latest insert mode text");
        let new = nb.new_cell(CellKind::Markdown, "new cell");
        nb.edit(vec![Change::Show { key: new, index: 0 }]);
        nb.cell_mut(&key).unwrap().raw.insert("execution_count".into(), 4.into());
        store.persist(cache.capture(&nb)).unwrap();
        let expected = nb.to_json();
        let path = nb.path().to_path_buf();
        drop(store);
        let reopened = RecoveryStore::at(&dir.path().join("state"), &path).unwrap();
        let recovered = reopened.load().unwrap().unwrap();
        assert_eq!(recovered.to_json(), expected);
        assert!(recovered.is_mutated());
        assert_eq!(fs::read(path).unwrap(), original, "recovery never writes the notebook");
    }

    #[test]
    fn source_edits_share_unchanged_output_blobs_and_collect_superseded_outputs() {
        let (_dir, mut nb, store) = fixture();
        let key = nb.order()[0].clone();
        let mut cache = SnapshotCache::default();
        let first = cache.capture(&nb);
        let original = first.outputs[0].clone().unwrap();
        store.persist(first).unwrap();
        let hash = original.hash.get().unwrap();
        let blob = store.dir.join(format!("{hash}.json"));
        let modified = fs::metadata(&blob).unwrap().modified().unwrap();
        nb.set_source(&key, "typed more");
        let second = cache.capture(&nb);
        assert!(Arc::ptr_eq(&original, second.outputs[0].as_ref().unwrap()));
        store.persist(second).unwrap();
        assert_eq!(fs::metadata(&blob).unwrap().modified().unwrap(), modified);
        nb.cell_mut(&key).unwrap().raw.insert("outputs".into(), json!([]));
        store.persist(cache.capture(&nb)).unwrap();
        assert!(!blob.exists());
        assert!(store.load().unwrap().unwrap().cell(&key).unwrap().outputs().is_empty());
    }

    #[test]
    fn recovery_retains_saved_baseline_and_refuses_external_changes_or_deletion() {
        let (_dir, mut nb, store) = fixture();
        let key = nb.order()[0].clone();
        nb.set_source(&key, "mine");
        store.persist(SnapshotCache::default().capture(&nb)).unwrap();
        let mut outside = Notebook::open(nb.path()).unwrap();
        outside.set_source(&key, "external");
        outside.commit(CommitOptions::default()).unwrap();
        let external = fs::read(nb.path()).unwrap();
        let mut recovered = store.load().unwrap().unwrap();
        assert!(matches!(recovered.commit(CommitOptions::default()), Err(CommitError::ChangedOnDisk(_))));
        assert_eq!(fs::read(nb.path()).unwrap(), external);
        recovered.reload().unwrap();
        assert_eq!(recovered.cell(&key).unwrap().source(), "external");
        let mut recovered = store.load().unwrap().unwrap();
        fs::remove_file(nb.path()).unwrap();
        assert!(matches!(recovered.commit(CommitOptions::default()), Err(CommitError::ChangedOnDisk(_))));
        assert!(!nb.path().exists());
    }

    #[test]
    fn recovery_can_save_against_its_baseline_and_cleanup_is_idempotent() {
        let (_dir, mut nb, store) = fixture();
        let key = nb.order()[0].clone();
        nb.set_source(&key, "mine");
        store.persist(SnapshotCache::default().capture(&nb)).unwrap();
        let mut recovered = store.load().unwrap().unwrap();
        recovered.commit(CommitOptions::default()).unwrap();
        store.clear().unwrap();
        store.clear().unwrap();
        assert!(store.load().unwrap().is_none());
        assert_eq!(Notebook::open(nb.path()).unwrap().cell(&key).unwrap().source(), "mine");
    }

    #[test]
    fn corrupt_manifests_and_blobs_report_notebook_and_state_file() {
        let (_dir, mut nb, store) = fixture();
        nb.set_source(&nb.order()[0].clone(), "mine");
        store.persist(SnapshotCache::default().capture(&nb)).unwrap();
        let bytes = fs::read(store.manifest_path()).unwrap();
        fs::write(store.manifest_path(), b"{").unwrap();
        let error = store.load().unwrap_err().to_string();
        assert!(error.contains(&nb.path().display().to_string()), "{error}");
        assert!(error.contains("snapshot.json"), "{error}");
        fs::write(store.manifest_path(), bytes).unwrap();
        let manifest: Manifest = serde_json::from_slice(&fs::read(store.manifest_path()).unwrap()).unwrap();
        let blob = store.dir.join(format!("{}.json", manifest.cells[0].outputs.as_ref().unwrap()));
        fs::write(&blob, b"[]").unwrap();
        let error = store.load().unwrap_err().to_string();
        assert!(error.contains("checksum mismatch"), "{error}");
        assert!(error.contains(&blob.display().to_string()), "{error}");
    }

    #[test]
    fn failed_snapshot_keeps_previous_manifest_and_reports_io_error() {
        let (_dir, mut nb, store) = fixture();
        let key = nb.order()[0].clone();
        let mut cache = SnapshotCache::default();
        nb.set_source(&key, "first");
        store.persist(cache.capture(&nb)).unwrap();
        let next_outputs = json!([{"output_type": "stream", "name": "stdout", "text": "new"}]);
        let hash = digest(&serde_json::to_vec(&next_outputs).unwrap());
        fs::create_dir(store.dir.join(format!("{hash}.json"))).unwrap();
        nb.cell_mut(&key).unwrap().raw.insert("outputs".into(), next_outputs);
        nb.set_source(&key, "second");
        assert!(store.persist(cache.capture(&nb)).is_err());
        assert_eq!(store.load().unwrap().unwrap().cell(&key).unwrap().source(), "first");
    }

    #[test]
    fn association_version_and_blob_references_are_validated() {
        let (_dir, nb, store) = fixture();
        store.persist(SnapshotCache::default().capture(&nb)).unwrap();
        let original = fs::read(store.manifest_path()).unwrap();
        for (field, value) in [("version", json!(2)), ("notebook", json!("/another.ipynb"))] {
            let mut manifest: Value = serde_json::from_slice(&original).unwrap();
            manifest[field] = value;
            fs::write(store.manifest_path(), serde_json::to_vec(&manifest).unwrap()).unwrap();
            assert!(store.load().unwrap_err().to_string().contains("unsupported version or mismatched"));
        }
        let mut manifest: Value = serde_json::from_slice(&original).unwrap();
        manifest["cells"][0]["outputs"] = "../../other".into();
        fs::write(store.manifest_path(), serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(store.load().unwrap_err().to_string().contains("invalid output blob reference"));
    }

    #[test]
    fn file_and_parent_symlink_aliases_share_lock_and_recovery_identity() {
        use std::os::unix::fs::symlink;
        let (dir, mut nb, store) = fixture();
        let file_alias = dir.path().join("alias.ipynb");
        symlink(nb.path(), &file_alias).unwrap();
        let parent_alias = dir.path().join("parent");
        symlink(nb.path().parent().unwrap(), &parent_alias).unwrap();
        let parent_path = parent_alias.join(nb.path().file_name().unwrap());
        let state = dir.path().join("state");
        for alias in [&file_alias, &parent_path] {
            assert!(RecoveryStore::at(&state, alias).is_err(), "alias must use the held lock: {}", alias.display());
        }
        nb.set_source(&nb.order()[0].clone(), "unsaved");
        store.persist(SnapshotCache::default().capture(&nb)).unwrap();
        drop(store);
        let alias_store = RecoveryStore::at(&state, &file_alias).unwrap();
        assert_eq!(alias_store.load().unwrap().unwrap().cell(&nb.order()[0]).unwrap().source(), "unsaved");
        drop(alias_store);
        // A dangling file link and a symlinked parent must still find the saved association.
        fs::remove_file(nb.path()).unwrap();
        for alias in [&file_alias, &parent_path, &nb.path().to_path_buf()] {
            let store = RecoveryStore::at(&state, alias).unwrap();
            assert!(store.load().unwrap().is_some());
        }
    }

    #[test]
    fn cross_directory_file_link_rename_keeps_target_lock_and_operational_reopen_path() {
        use std::os::unix::fs::symlink;
        let (dir, original, initial_store) = fixture();
        drop(initial_store);
        let target = original.path().to_path_buf();
        let original_bytes = fs::read(&target).unwrap();
        let links = dir.path().join("links");
        fs::create_dir(&links).unwrap();
        let alias = links.join("alias.ipynb");
        symlink("../test.ipynb", &alias).unwrap();
        let mut nb = Notebook::open(&alias).unwrap();
        let key = nb.order()[0].clone();
        nb.set_source(&key, "unsaved through link");
        let state = dir.path().join("state");
        let mut store = RecoveryStore::at(&state, &alias).unwrap();
        let manifest = store.manifest_path();
        store.persist(SnapshotCache::default().capture(&nb)).unwrap();
        nb.rename("renamed.ipynb").unwrap();
        store.follow_file_link_rename(nb.path()).unwrap();
        assert_eq!(store.manifest_path(), manifest);
        assert!(!alias.exists());
        assert!(fs::symlink_metadata(nb.path()).unwrap().file_type().is_symlink());
        assert_eq!(fs::read(&target).unwrap(), original_bytes);
        assert!(RecoveryStore::at(&state, &target).is_err(), "the target lock remains held");
        nb.set_source(&key, "latest unsaved through renamed link");
        store.persist(SnapshotCache::default().capture(&nb)).unwrap();
        drop(store);
        let reopened = RecoveryStore::at(&state, nb.path()).unwrap();
        let mut recovered = reopened.load().unwrap().unwrap();
        assert_eq!(recovered.path(), nb.path());
        assert_eq!(recovered.cell(&key).unwrap().source(), "latest unsaved through renamed link");
        recovered.commit(CommitOptions::default()).unwrap();
        assert!(fs::symlink_metadata(nb.path()).unwrap().file_type().is_symlink());
        assert_eq!(
            Notebook::open(&target).unwrap().cell(&key).unwrap().source(),
            "latest unsaved through renamed link"
        );
    }

    #[test]
    fn empty_outputs_stay_inline_without_blob_files() {
        let (_dir, mut nb, store) = fixture();
        nb.cell_mut(&nb.order()[0].clone()).unwrap().raw.insert("outputs".into(), json!([]));
        store.persist(SnapshotCache::default().capture(&nb)).unwrap();
        assert_eq!(fs::read_dir(&store.dir).unwrap().count(), 1, "only the manifest is needed");
        assert_eq!(store.load().unwrap().unwrap().to_json(), nb.to_json());
    }

    #[test]
    fn simultaneous_sessions_cannot_replace_each_others_recovery() {
        let (dir, nb, store) = fixture();
        let other = RecoveryStore::at(&dir.path().join("state"), nb.path());
        assert!(other.is_err());
        drop(store);
        assert!(RecoveryStore::at(&dir.path().join("state"), nb.path()).is_ok());
    }

    #[test]
    fn state_directory_errors_are_explicit() {
        let (dir, nb, _store) = fixture();
        let blocked = dir.path().join("file");
        fs::write(&blocked, "not a directory").unwrap();
        let result = RecoveryStore::at(&blocked, nb.path());
        let error = result.err().unwrap().to_string();
        assert!(error.contains(&blocked.display().to_string()), "{error}");
    }

    #[test]
    #[ignore = "manual save/recovery profile; timing is not a correctness assertion"]
    fn profile_large_output_save_and_recovery() {
        use std::time::{Duration, Instant};

        fn report(label: &str, mut times: Vec<Duration>) {
            times.sort();
            let median = times[times.len() / 2];
            let p95 = times[(times.len() * 95 / 100).min(times.len() - 1)];
            eprintln!("{label}: n={} median={median:?} p95={p95:?}", times.len());
        }

        let (_dir, mut nb, store) = fixture();
        let key = nb.order()[0].clone();
        nb.cell_mut(&key).unwrap().raw.insert(
            "outputs".into(),
            json!([{
                "output_type": "display_data", "metadata": {}, "data": {"image/png": "a".repeat(8 * 1024 * 1024)}
            }]),
        );
        for i in 1..1000 {
            let key = nb.new_cell(CellKind::Code, &format!("# cell {i}\n{}", "value = 123\n".repeat(10)));
            nb.edit(vec![Change::Show { key, index: i }]);
        }
        nb.commit(CommitOptions::default()).unwrap();
        eprintln!("fixture: 1,000 code cells, ten source lines/cell, one unchanged 8 MiB image output");

        let mut cache = SnapshotCache::default();
        let started = Instant::now();
        let snapshot = cache.capture(&nb);
        eprintln!("cold capture on editor thread: {:?}", started.elapsed());
        let started = Instant::now();
        store.persist(snapshot).unwrap();
        eprintln!("cold recovery encode/hash/fsync on worker: {:?}", started.elapsed());
        let output = cache.outputs[&key].clone();
        let blob = store.dir.join(format!("{}.json", output.hash.get().unwrap()));
        let modified = fs::metadata(&blob).unwrap().modified().unwrap();

        let mut captures = vec![];
        let mut writes = vec![];
        for i in 0..20 {
            nb.set_source(&key, &format!("source-only edit {i}"));
            let started = Instant::now();
            let snapshot = cache.capture(&nb);
            captures.push(started.elapsed());
            assert!(Arc::ptr_eq(&output, snapshot.outputs[0].as_ref().unwrap()));
            let started = Instant::now();
            store.persist(snapshot).unwrap();
            writes.push(started.elapsed());
        }
        report("warm capture on editor thread (source-only edits)", captures);
        report("warm recovery encode/fsync on worker (source-only edits)", writes);
        assert_eq!(fs::metadata(&blob).unwrap().modified().unwrap(), modified, "image blob was not rewritten");
        eprintln!("unchanged image: same shared object and persisted blob throughout all 20 edits");

        let mut encodes = vec![];
        let mut saves = vec![];
        for _ in 0..10 {
            let started = Instant::now();
            let bytes = nb.to_bytes();
            encodes.push(started.elapsed());
            std::hint::black_box(&bytes);
            let started = Instant::now();
            nb.commit(CommitOptions::default()).unwrap();
            saves.push(started.elapsed());
        }
        report("synchronous full notebook encode (to_bytes)", encodes);
        report("synchronous full commit (disk read/hash + encode/hash/fsync)", saves);
        eprintln!("normal-save timings exclude Neovim RPC/buffer resends and BufWritePre/Post plugin hooks");
    }
}
