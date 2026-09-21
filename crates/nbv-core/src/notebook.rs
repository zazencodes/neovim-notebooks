//! The canonical notebook: load (§6), projection and reconciliation (§7), persistence (§9.4).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::adapter::{CellKind, LanguageProjection, Marker, PythonProjection};
use crate::document::Cell;
use crate::key::{CellKey, KeyMinter};

/// Replace lines `[first, last)` of the previous text with `lines`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineEdit {
    pub first: usize,
    pub last: usize,
    pub lines: Vec<String>,
}

/// Outcome of reconciling one edit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reconciled {
    /// Whether the document changed.
    pub changed: bool,
    /// Edits that make every marker show its assigned key (§7.3). Ordered bottom-up, so each
    /// applies to the text as left by the ones before it without index adjustment.
    pub normalise: Vec<LineEdit>,
}

/// Where a live cell sits in the projected text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellSpan {
    pub key: CellKey,
    pub kind: CellKind,
    /// The marker line, or `None` for a leading region awaiting normalisation.
    pub marker: Option<usize>,
    /// First body line.
    pub body: usize,
    /// One past the last body line.
    pub end: usize,
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
}

#[derive(Debug, thiserror::Error)]
pub enum CommitError {
    #[error("{0} changed on disk since it was loaded (use :w! to overwrite, :e! to reload)")]
    ChangedOnDisk(PathBuf),
    #[error("cannot write {0}: {1}")]
    Io(PathBuf, std::io::Error),
}

#[derive(Debug, thiserror::Error)]
#[error("edit [{first}, {last}) is outside a {len}-line buffer")]
pub struct EditError {
    pub first: usize,
    pub last: usize,
    pub len: usize,
}

pub struct Notebook {
    path: PathBuf,
    /// The top-level object. Its `cells` entry is rebuilt from `order` on serialisation.
    top: Map<String, Value>,
    trailing_newline: bool,
    order: Vec<CellKey>,
    live: HashMap<CellKey, Cell>,
    tombstones: HashMap<CellKey, Cell>,
    minter: KeyMinter,
    /// Keys minted for cells a structural command is inserting, adopted when they appear.
    pending: HashSet<CellKey>,
    adapter: PythonProjection,
    mirror: Vec<String>,
    layout: Vec<CellSpan>,
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
        let raw_cells = match top.get_mut("cells") {
            Some(Value::Array(cells)) => std::mem::take(cells),
            _ => return Err(OpenError::NotANotebook(path.into(), "missing cells array")),
        };
        let mut cells = Vec::with_capacity(raw_cells.len());
        for c in raw_cells {
            let Value::Object(raw) = c else {
                return Err(OpenError::NotANotebook(path.into(), "a cell is not an object"));
            };
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

        let trailing_newline = bytes.last() == Some(&b'\n');
        let mut nb = Notebook {
            path: path.into(),
            top,
            trailing_newline,
            order: keys.clone(),
            live: keys.into_iter().zip(cells).collect(),
            tombstones: HashMap::new(),
            minter,
            pending: HashSet::new(),
            adapter: PythonProjection,
            mirror: vec![],
            layout: vec![],
            mutated: false,
            disk_hash: None,
        };
        nb.mirror = nb.project();
        nb.layout = nb.compute_layout();
        Ok(nb)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn adapter(&self) -> &PythonProjection {
        &self.adapter
    }

    /// The kernel name from `metadata.kernelspec.name`, if any.
    pub fn kernel_name(&self) -> Option<&str> {
        self.top.get("metadata")?.get("kernelspec")?.get("name")?.as_str()
    }

    pub fn is_mutated(&self) -> bool {
        self.mutated
    }

    pub fn order(&self) -> &[CellKey] {
        &self.order
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

    /// Mints a key for a cell a structural command is about to insert (§7.5).
    pub fn mint_key(&mut self) -> CellKey {
        let key = self.minter.mint();
        self.pending.insert(key.clone());
        key
    }

    /// The projected text of the whole document (§7.1).
    pub fn project(&self) -> Vec<String> {
        let mut out = Vec::new();
        for key in &self.order {
            let cell = &self.live[key];
            let kind = cell.kind();
            out.push(self.adapter.format_marker(kind, key));
            out.extend(self.adapter.to_buffer(kind, &cell.source()));
        }
        out
    }

    /// The text core believes the buffer holds.
    pub fn mirror(&self) -> &[String] {
        &self.mirror
    }

    pub fn layout(&self) -> &[CellSpan] {
        &self.layout
    }

    /// The span containing `line`, if any.
    pub fn span_at(&self, line: usize) -> Option<&CellSpan> {
        self.layout.iter().rev().find(|s| s.marker.unwrap_or(0) <= line)
    }

    fn compute_layout(&self) -> Vec<CellSpan> {
        let mut spans = Vec::new();
        let mut line = 0;
        for key in &self.order {
            let cell = &self.live[key];
            let n = self.adapter.to_buffer(cell.kind(), &cell.source()).len();
            spans.push(CellSpan {
                key: key.clone(),
                kind: cell.kind(),
                marker: Some(line),
                body: line + 1,
                end: line + 1 + n,
            });
            line += 1 + n;
        }
        spans
    }

    /// Whole-buffer replacement. Always safe: identity is in the text (§7.2).
    pub fn resync(&mut self, lines: Vec<String>) -> Reconciled {
        let len = self.mirror.len();
        self.apply_edit(LineEdit { first: 0, last: len, lines }).expect("whole-buffer edit is in range")
    }

    /// Reconciles one line edit (§7.2).
    pub fn apply_edit(&mut self, edit: LineEdit) -> Result<Reconciled, EditError> {
        let LineEdit { first, last, lines } = edit;
        if first > last || last > self.mirror.len() {
            return Err(EditError { first, last, len: self.mirror.len() });
        }
        let inserted = lines.len();
        self.mirror.splice(first..last, lines);

        // Old marker positions mapped into new coordinates, for markers outside the edit.
        let old_by_line: HashMap<usize, CellKey> =
            self.layout.iter().filter_map(|s| s.marker.map(|m| (m, s.key.clone()))).collect();
        let old_leading = self.layout.first().filter(|s| s.marker.is_none()).map(|s| s.key.clone());
        let survivor = |line: usize| -> Option<&CellKey> {
            let old = if line < first {
                line
            } else if line >= first + inserted {
                line - inserted + (last - first)
            } else {
                return None;
            };
            old_by_line.get(&old)
        };

        let markers: Vec<(usize, Marker)> =
            self.mirror.iter().enumerate().filter_map(|(i, l)| self.adapter.parse_marker(l).map(|m| (i, m))).collect();

        let claimable =
            |k: &CellKey| self.live.contains_key(k) || self.tombstones.contains_key(k) || self.pending.contains(k);
        let survivors: Vec<Option<&CellKey>> = markers.iter().map(|(i, _)| survivor(*i)).collect();

        // Step 1: an untouched marker showing the key it already had keeps it.
        let mut assigned: Vec<Option<CellKey>> = markers
            .iter()
            .zip(&survivors)
            .map(|((_, m), old)| old.filter(|k| m.key.as_ref() == Some(*k)).cloned())
            .collect();
        let mut taken: HashSet<CellKey> = assigned.iter().flatten().cloned().collect();

        // Step 2: identity is in the text. Every other marker takes the key it shows, first
        // occurrence first. This includes untouched markers still awaiting normalisation, so
        // a copy whose original has since gone reclaims the key (`:%!cmd` inserts the new
        // text before deleting the old).
        for (slot, (_, marker)) in assigned.iter_mut().zip(&markers) {
            if slot.is_none()
                && let Some(k) = &marker.key
                && claimable(k)
                && taken.insert(k.clone())
            {
                *slot = Some(k.clone());
            }
        }

        // Step 3: an untouched marker whose text claims nothing keeps its pending key.
        for (slot, old) in assigned.iter_mut().zip(&survivors) {
            if slot.is_none()
                && let Some(k) = old
                && taken.insert((*k).clone())
            {
                *slot = Some((*k).clone());
            }
        }

        // The leading region keeps its pending key while it stays non-empty.
        let lead_end = markers.first().map_or(self.mirror.len(), |(i, _)| *i);
        let has_leading = self.mirror[..lead_end].iter().any(|l| !l.trim().is_empty());
        let leading_key = has_leading.then(|| match old_leading {
            Some(k) if self.live.contains_key(&k) && !taken.contains(&k) => k,
            _ => self.minter.mint(),
        });
        if let Some(k) = &leading_key {
            taken.insert(k.clone());
        }

        // Everything else is a new cell.
        let assigned: Vec<CellKey> = assigned.into_iter().map(|k| k.unwrap_or_else(|| self.minter.mint())).collect();

        let mut spans = Vec::with_capacity(markers.len() + 1);
        if let Some(k) = leading_key {
            spans.push(CellSpan { key: k, kind: CellKind::Code, marker: None, body: 0, end: lead_end });
        }
        for (idx, ((line, marker), key)) in markers.iter().zip(assigned).enumerate() {
            let end = markers.get(idx + 1).map_or(self.mirror.len(), |(i, _)| *i);
            spans.push(CellSpan { key, kind: marker.kind, marker: Some(*line), body: line + 1, end });
        }

        for s in &spans {
            self.pending.remove(&s.key);
        }
        let changed = self.adopt(&spans);
        self.mutated |= changed;

        self.layout = spans;
        Ok(Reconciled { changed, normalise: self.pending_normalisation() })
    }

    /// Edits that make every marker show its assigned key and give a leading region its
    /// marker (§7.3), computed against the current text. Ordered bottom-up.
    pub fn pending_normalisation(&self) -> Vec<LineEdit> {
        let mut edits = Vec::new();
        for span in self.layout.iter().rev() {
            let Some(line) = span.marker else { continue };
            let shown = self.adapter.parse_marker(&self.mirror[line]).and_then(|m| m.key);
            if shown.as_ref() != Some(&span.key) {
                edits.push(LineEdit {
                    first: line,
                    last: line + 1,
                    lines: vec![self.adapter.format_marker(span.kind, &span.key)],
                });
            }
        }
        if let Some(lead) = self.layout.first().filter(|s| s.marker.is_none()) {
            edits.push(LineEdit {
                first: 0,
                last: 0,
                lines: vec![self.adapter.format_marker(CellKind::Code, &lead.key)],
            });
        }
        edits
    }

    /// Makes the document match `spans` over the current mirror. Returns whether it changed.
    fn adopt(&mut self, spans: &[CellSpan]) -> bool {
        let new_order: Vec<CellKey> = spans.iter().map(|s| s.key.clone()).collect();
        let keep: HashSet<&CellKey> = new_order.iter().collect();
        let mut changed = new_order != self.order;

        let gone: Vec<CellKey> = self.order.iter().filter(|k| !keep.contains(k)).cloned().collect();
        for k in gone {
            let cell = self.live.remove(&k).expect("order and live agree");
            self.tombstones.insert(k, cell);
        }
        for span in spans {
            let source = self.adapter.from_buffer(span.kind, &self.mirror[span.body..span.end]);
            let cell = match self.live.get_mut(&span.key) {
                Some(c) => c,
                None => {
                    let cell = match self.tombstones.remove(&span.key) {
                        Some(c) => c,
                        None => Cell::new(span.kind, &source),
                    };
                    changed = true;
                    self.live.entry(span.key.clone()).or_insert(cell)
                }
            };
            if cell.kind() != span.kind {
                cell.set_kind(span.kind);
                changed = true;
            }
            changed |= cell.set_source(&source);
        }
        self.order = new_order;
        changed
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

    /// Reloads from disk (§9.5): rebuilds the document and discards tombstones. Returns the new
    /// projection, which the frontend writes over the whole buffer.
    pub fn reload(&mut self) -> Result<Vec<String>, OpenError> {
        let bytes = fs::read(&self.path).map_err(|e| OpenError::Io(self.path.clone(), e))?;
        let minter = std::mem::take(&mut self.minter);
        let mut nb = Notebook::from_bytes(&self.path, &bytes, minter)?;
        nb.disk_hash = Some(Sha256::digest(&bytes).into());
        *self = nb;
        Ok(self.mirror.clone())
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
