//! The canonical notebook: load (§6), cell sources and structural changes with undo (§7),
//! persistence (§9.4).

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::document::{Cell, CellKind};
use crate::key::{CellKey, KeyMinter};

/// One structural change to the document. Every change has an exact inverse, which is what
/// undo applies (§7.3). Cells leaving the document are tombstoned, never destroyed, so an
/// inverse can always bring a cell back with its outputs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// Makes a tombstoned cell live at `index` (clamped to the end).
    Show { key: CellKey, index: usize },
    /// Tombstones a live cell.
    Hide { key: CellKey },
    /// Moves a live cell to `index` (clamped to the end).
    Move { key: CellKey, index: usize },
    /// Changes a live cell's type.
    Kind { key: CellKey, kind: CellKind },
    /// Splits a live cell before source line `line`, which must leave a line on each side: the
    /// lines from there on move into the tombstoned cell `new`, which keeps its own type and
    /// becomes live directly below.
    Split { key: CellKey, line: usize, new: CellKey },
    /// Appends live cell `next`'s source to `key`'s, on a new line, and tombstones `next`.
    Merge { key: CellKey, next: CellKey },
}

#[derive(Debug, Default, Clone, Copy)]
pub struct CommitOptions {
    pub force: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("cannot read {0}: {1}")]
    Io(PathBuf, std::io::Error),
    #[error("{0} is not valid JSON: {1}")]
    Json(PathBuf, serde_json::Error),
    #[error("{0} is not a notebook: {1}")]
    NotANotebook(PathBuf, &'static str),
    #[error("{0} is nbformat {1}; nbv supports nbformat 4 only")]
    UnsupportedNbformat(PathBuf, String),
    #[error("{0}: cell {1} has cell_type {2}; nbformat 4 allows code, markdown and raw")]
    UnknownCellType(PathBuf, usize, String),
}

#[derive(Debug, thiserror::Error)]
pub enum CommitError {
    #[error("{0} changed on disk since it was loaded (use :w! to overwrite, :e! to reload)")]
    ChangedOnDisk(PathBuf),
    #[error("cannot write {0}: {1}")]
    Io(PathBuf, std::io::Error),
}

pub struct Notebook {
    path: PathBuf,
    /// The top-level object. Its `cells` entry is rebuilt from `order` on serialisation.
    top: Map<String, Value>,
    trailing_newline: bool,
    order: Vec<CellKey>,
    live: HashMap<CellKey, Cell>,
    /// Deleted cells, and cells created but not yet shown (§7.4).
    tombstones: HashMap<CellKey, Cell>,
    minter: KeyMinter,
    /// Inverse change groups, most recent last.
    undo: Vec<Vec<Change>>,
    redo: Vec<Vec<Change>>,
    /// Whether any source, structure, type, output or execution change has happened (§6.3).
    mutated: bool,
    disk_hash: Option<[u8; 32]>,
}

impl std::fmt::Debug for Notebook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Notebook").field("path", &self.path).field("order", &self.order).finish()
    }
}

impl Notebook {
    pub fn open(path: &Path) -> Result<Notebook, OpenError> {
        let bytes = fs::read(path).map_err(|e| OpenError::Io(path.into(), e))?;
        let mut nb = Notebook::from_bytes(path, &bytes, KeyMinter::default())?;
        nb.disk_hash = Some(Sha256::digest(&bytes).into());
        Ok(nb)
    }

    /// Parses notebook JSON without touching the disk. `path` is only used for naming.
    pub fn from_bytes(path: &Path, bytes: &[u8], mut minter: KeyMinter) -> Result<Notebook, OpenError> {
        let value: Value = serde_json::from_slice(bytes).map_err(|e| OpenError::Json(path.into(), e))?;
        let Value::Object(mut top) = value else {
            return Err(OpenError::NotANotebook(path.into(), "top level is not an object"));
        };
        match top.get("nbformat") {
            Some(Value::Number(n)) if n.as_u64() == Some(4) => {}
            Some(v) => return Err(OpenError::UnsupportedNbformat(path.into(), v.to_string())),
            None => return Err(OpenError::NotANotebook(path.into(), "missing nbformat")),
        }
        if !top.get("metadata").is_some_and(Value::is_object) {
            return Err(OpenError::NotANotebook(path.into(), "metadata is missing or not an object"));
        }
        let raw_cells = match top.get_mut("cells") {
            Some(Value::Array(cells)) => std::mem::take(cells),
            _ => return Err(OpenError::NotANotebook(path.into(), "missing cells array")),
        };
        let mut cells = Vec::with_capacity(raw_cells.len());
        for (i, c) in raw_cells.into_iter().enumerate() {
            let Value::Object(raw) = c else {
                return Err(OpenError::NotANotebook(path.into(), "a cell is not an object"));
            };
            let kind = raw.get("cell_type").and_then(Value::as_str);
            if kind.and_then(CellKind::from_nbformat).is_none() {
                let shown = raw.get("cell_type").map_or("missing".into(), Value::to_string);
                return Err(OpenError::UnknownCellType(path.into(), i + 1, shown));
            }
            cells.push(Cell::from_raw(raw));
        }

        // A persisted id becomes the key only if it is valid and unique (§6.2).
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for c in &cells {
            if let Some(id) = c.persisted_id() {
                *counts.entry(id).or_default() += 1;
            }
        }
        let usable = |c: &Cell| {
            c.persisted_id().filter(|id| CellKey::is_valid_nbformat_id(id) && counts[id] == 1).map(CellKey::new)
        };
        let keys: Vec<Option<CellKey>> = cells.iter().map(usable).collect();
        for k in keys.iter().flatten() {
            minter.reserve(k);
        }
        let keys: Vec<CellKey> = keys.into_iter().map(|k| k.unwrap_or_else(|| minter.mint())).collect();

        Ok(Notebook {
            path: path.into(),
            top,
            trailing_newline: bytes.last() == Some(&b'\n'),
            order: keys.clone(),
            live: keys.into_iter().zip(cells).collect(),
            tombstones: HashMap::new(),
            minter,
            undo: vec![],
            redo: vec![],
            mutated: false,
            disk_hash: None,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The kernel name from `metadata.kernelspec.name`, if any.
    pub fn kernel_name(&self) -> Option<&str> {
        self.top.get("metadata")?.get("kernelspec")?.get("name")?.as_str()
    }

    /// Records the kernel the notebook runs on, as Jupyter does when a kernel is chosen.
    pub fn set_kernelspec(&mut self, name: &str, display_name: &str, language: &str) {
        let Some(Value::Object(meta)) = self.top.get_mut("metadata") else {
            unreachable!("metadata is validated as an object at load");
        };
        let spec = serde_json::json!({ "display_name": display_name, "language": language, "name": name });
        if meta.get("kernelspec") != Some(&spec) {
            meta.insert("kernelspec".into(), spec);
            self.mutated = true;
        }
    }

    pub fn is_mutated(&self) -> bool {
        self.mutated
    }

    pub fn order(&self) -> &[CellKey] {
        &self.order
    }

    pub fn index_of(&self, key: &CellKey) -> Option<usize> {
        self.order.iter().position(|k| k == key)
    }

    /// A live or tombstoned cell.
    pub fn cell(&self, key: &CellKey) -> Option<&Cell> {
        self.live.get(key).or_else(|| self.tombstones.get(key))
    }

    /// A live or tombstoned cell, for runtime and output changes. Marks the document mutated.
    pub fn cell_mut(&mut self, key: &CellKey) -> Option<&mut Cell> {
        self.mutated = true;
        match self.live.get_mut(key) {
            Some(c) => Some(c),
            None => self.tombstones.get_mut(key),
        }
    }

    /// Runtime-only access (exec state, timings) that does not count as a mutation.
    pub fn runtime_mut(&mut self, key: &CellKey) -> Option<&mut crate::document::Runtime> {
        match self.live.get_mut(key) {
            Some(c) => Some(&mut c.runtime),
            None => self.tombstones.get_mut(key).map(|c| &mut c.runtime),
        }
    }

    pub fn is_live(&self, key: &CellKey) -> bool {
        self.live.contains_key(key)
    }

    pub fn is_tombstoned(&self, key: &CellKey) -> bool {
        self.tombstones.contains_key(key)
    }

    /// Replaces a live cell's source, as typed in its editor. Returns whether it changed.
    /// Text edits are not structural changes: the editor has its own undo for them.
    pub fn set_source(&mut self, key: &CellKey, source: &str) -> bool {
        let Some(cell) = self.live.get_mut(key) else { return false };
        let changed = cell.set_source(source);
        self.mutated |= changed;
        changed
    }

    /// Creates a tombstoned cell under a fresh key, ready for [`Change::Show`] or as the
    /// `new` half of a [`Change::Split`].
    pub fn new_cell(&mut self, kind: CellKind, source: &str) -> CellKey {
        let key = self.minter.mint();
        self.tombstones.insert(key.clone(), Cell::new(kind, source));
        key
    }

    /// Creates a tombstoned copy of a live or tombstoned cell under a fresh key, outputs
    /// included, for pasting.
    pub fn copy_cell(&mut self, raw: &Map<String, Value>) -> CellKey {
        let mut raw = raw.clone();
        raw.shift_remove("id");
        let key = self.minter.mint();
        let mut cell = Cell::from_raw(raw);
        cell.runtime.baseline = cell.source();
        self.tombstones.insert(key.clone(), cell);
        key
    }

    /// Applies a group of changes as one undo step. Returns the index of the cell to focus.
    pub fn edit(&mut self, changes: Vec<Change>) -> Option<usize> {
        let (inverse, focus) = self.apply_group(changes);
        if !inverse.is_empty() {
            self.undo.push(inverse);
            self.redo.clear();
        }
        focus
    }

    /// Reverts the most recent change group. Returns the index of the cell to focus, or
    /// `None` when there is nothing to undo.
    pub fn undo(&mut self) -> Option<usize> {
        let group = self.undo.pop()?;
        let (inverse, focus) = self.apply_group(group);
        self.redo.push(inverse);
        focus.or(Some(0))
    }

    pub fn redo(&mut self) -> Option<usize> {
        let group = self.redo.pop()?;
        let (inverse, focus) = self.apply_group(group);
        self.undo.push(inverse);
        focus.or(Some(0))
    }

    /// Applies changes in order; returns their inverses in reverse order, and the focus of the
    /// first change that applied. Changes that do not apply (a stale key) are skipped.
    fn apply_group(&mut self, changes: Vec<Change>) -> (Vec<Change>, Option<usize>) {
        let mut inverse = vec![];
        let mut focus = None;
        for c in changes {
            if let Some((inv, at)) = self.apply(c) {
                inverse.push(inv);
                focus.get_or_insert(at);
            }
        }
        if !inverse.is_empty() {
            self.mutated = true;
        }
        inverse.reverse();
        (inverse, focus)
    }

    fn apply(&mut self, change: Change) -> Option<(Change, usize)> {
        match change {
            Change::Show { key, index } => {
                let cell = self.tombstones.remove(&key)?;
                let at = index.min(self.order.len());
                self.order.insert(at, key.clone());
                self.live.insert(key.clone(), cell);
                Some((Change::Hide { key }, at))
            }
            Change::Hide { key } => {
                let at = self.index_of(&key)?;
                self.order.remove(at);
                let cell = self.live.remove(&key).expect("order and live agree");
                self.tombstones.insert(key.clone(), cell);
                Some((Change::Show { key, index: at }, at.min(self.order.len().saturating_sub(1))))
            }
            Change::Move { key, index } => {
                let from = self.index_of(&key)?;
                self.order.remove(from);
                let at = index.min(self.order.len());
                self.order.insert(at, key.clone());
                Some((Change::Move { key, index: from }, at))
            }
            Change::Kind { key, kind } => {
                let at = self.index_of(&key)?;
                let cell = self.live.get_mut(&key)?;
                let old = cell.kind();
                if old == kind {
                    return None;
                }
                cell.set_kind(kind);
                Some((Change::Kind { key, kind: old }, at))
            }
            Change::Split { key, line, new } => {
                let at = self.index_of(&key)?;
                if !self.tombstones.contains_key(&new) {
                    return None;
                }
                let source = self.live[&key].source();
                let lines: Vec<&str> = source.split('\n').collect();
                // Both halves keep at least one line, so merging them back restores the source.
                if line == 0 || line >= lines.len() {
                    return None;
                }
                let (head, tail) = (lines[..line].join("\n"), lines[line..].join("\n"));
                self.live.get_mut(&key).expect("checked").set_source(&head);
                let mut second = self.tombstones.remove(&new).expect("checked");
                second.set_source(&tail);
                self.order.insert(at + 1, new.clone());
                self.live.insert(new.clone(), second);
                Some((Change::Merge { key, next: new }, at))
            }
            Change::Merge { key, next } => {
                let at = self.index_of(&key)?;
                if key == next || !self.live.contains_key(&next) {
                    return None;
                }
                let head = self.live[&key].source();
                let tail = self.live[&next].source();
                let line = head.split('\n').count();
                self.live.get_mut(&key).expect("checked").set_source(&format!("{head}\n{tail}"));
                let pos = self.index_of(&next).expect("live");
                self.order.remove(pos);
                let cell = self.live.remove(&next).expect("checked");
                self.tombstones.insert(next.clone(), cell);
                Some((Change::Split { key, line, new: next }, at))
            }
        }
    }

    /// The document as nbformat JSON, as it would be committed.
    pub fn to_json(&self) -> Value {
        let mut top = self.top.clone();
        let cells: Vec<Value> = self
            .order
            .iter()
            .map(|k| {
                let mut raw = self.live[k].raw.clone();
                if self.mutated {
                    insert_sorted(&mut raw, "id", Value::String(k.as_str().into()));
                }
                Value::Object(raw)
            })
            .collect();
        top.insert("cells".into(), Value::Array(cells));
        if self.mutated && top.get("nbformat_minor").and_then(Value::as_u64).is_none_or(|m| m < 5) {
            top.insert("nbformat_minor".into(), 5.into());
        }
        Value::Object(top)
    }

    /// Serialises with Jupyter's 1-space indentation and the original key order.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let fmt = serde_json::ser::PrettyFormatter::with_indent(b" ");
        let mut ser = serde_json::Serializer::with_formatter(&mut out, fmt);
        serde::Serialize::serialize(&self.to_json(), &mut ser).expect("Value serialises");
        if self.trailing_newline {
            out.push(b'\n');
        }
        out
    }

    /// Commit the document to its `.ipynb` (§9.4).
    pub fn commit(&mut self, opts: CommitOptions) -> Result<(), CommitError> {
        let io = |e| CommitError::Io(self.path.clone(), e);
        let target = match fs::symlink_metadata(&self.path) {
            Ok(m) if m.file_type().is_symlink() => fs::canonicalize(&self.path).map_err(io)?,
            _ => self.path.clone(),
        };
        let on_disk = match fs::read(&target) {
            Ok(bytes) => Some(<[u8; 32]>::from(Sha256::digest(&bytes))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(io(e)),
        };
        if !opts.force && on_disk.is_some() && on_disk != self.disk_hash {
            return Err(CommitError::ChangedOnDisk(self.path.clone()));
        }

        let bytes = self.to_bytes();
        let dir = target.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(io)?;
        tmp.write_all(&bytes).map_err(io)?;
        if let Ok(meta) = fs::metadata(&target) {
            fs::set_permissions(tmp.path(), meta.permissions()).map_err(io)?;
        }
        tmp.as_file().sync_all().map_err(io)?;
        tmp.persist(&target).map_err(|e| io(e.error))?;
        if let Ok(d) = fs::File::open(dir) {
            let _ = d.sync_all();
        }
        self.disk_hash = Some(Sha256::digest(&bytes).into());
        Ok(())
    }

    /// Reloads from disk (§9.5): rebuilds the document, discarding tombstones and history.
    pub fn reload(&mut self) -> Result<(), OpenError> {
        let bytes = fs::read(&self.path).map_err(|e| OpenError::Io(self.path.clone(), e))?;
        let minter = std::mem::take(&mut self.minter);
        let mut nb = Notebook::from_bytes(&self.path, &bytes, minter)?;
        nb.disk_hash = Some(Sha256::digest(&bytes).into());
        *self = nb;
        Ok(())
    }
}

/// Inserts `key` where it belongs if `map`'s keys are sorted, else at the end.
fn insert_sorted(map: &mut Map<String, Value>, key: &str, value: Value) {
    if let Some(v) = map.get_mut(key) {
        *v = value;
        return;
    }
    let keys: Vec<&String> = map.keys().collect();
    let sorted = keys.windows(2).all(|w| w[0] <= w[1]);
    let idx = if sorted { keys.iter().position(|k| k.as_str() > key).unwrap_or(keys.len()) } else { keys.len() };
    map.shift_insert(idx, key.into(), value);
}
