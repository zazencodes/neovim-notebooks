//! The lossless document (§6.1): raw notebook JSON beneath a typed index.

use serde_json::{Map, Value};

use crate::adapter::CellKind;

/// Per-cell runtime state. Never serialised.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Runtime {
    pub exec: ExecState,
    /// Source at load or at the most recent execution request (§7.6).
    pub baseline: String,
    /// Wall time of the most recent completed execution.
    pub duration: Option<std::time::Duration>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExecState {
    #[default]
    Idle,
    Queued,
    Running,
    Ok,
    Error,
}

/// One cell: its raw JSON object plus the fields the current type does not allow.
#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    pub raw: Map<String, Value>,
    /// Fields removed from `raw` by a type change, restored when the type changes back.
    pub stash: Map<String, Value>,
    pub runtime: Runtime,
}

/// Fields each cell type does not allow (§6.1).
fn disallowed(kind: CellKind) -> &'static [&'static str] {
    match kind {
        CellKind::Code => &["attachments"],
        CellKind::Markdown | CellKind::Raw => &["outputs", "execution_count"],
    }
}

impl Cell {
    pub fn new(kind: CellKind, source: &str) -> Cell {
        let mut raw = Map::new();
        raw.insert("cell_type".into(), kind.as_nbformat().into());
        if kind == CellKind::Code {
            raw.insert("execution_count".into(), Value::Null);
        }
        raw.insert("metadata".into(), Value::Object(Map::new()));
        if kind == CellKind::Code {
            raw.insert("outputs".into(), Value::Array(vec![]));
        }
        raw.insert("source".into(), Value::Array(vec![]));
        let mut cell = Cell { raw, stash: Map::new(), runtime: Runtime::default() };
        cell.set_source(source);
        cell.runtime.baseline = source.to_string();
        cell
    }

    pub fn from_raw(raw: Map<String, Value>) -> Cell {
        let mut cell = Cell { raw, stash: Map::new(), runtime: Runtime::default() };
        cell.runtime.baseline = cell.source();
        cell
    }

    /// Unknown cell types are treated as raw for projection; their `cell_type` is untouched.
    pub fn kind(&self) -> CellKind {
        self.raw
            .get("cell_type")
            .and_then(Value::as_str)
            .and_then(CellKind::from_nbformat)
            .unwrap_or(CellKind::Raw)
    }

    pub fn set_kind(&mut self, kind: CellKind) {
        if kind == self.kind() {
            return;
        }
        for field in disallowed(kind) {
            match self.raw.shift_remove(*field) {
                // Defaults are recreated on the way back; stashing them would be noise.
                Some(Value::Null) if *field == "execution_count" => {}
                Some(Value::Array(a)) if *field == "outputs" && a.is_empty() => {}
                Some(v) => {
                    self.stash.insert((*field).into(), v);
                }
                None => {}
            }
        }
        let restorable: Vec<String> =
            self.stash.keys().filter(|f| !disallowed(kind).contains(&f.as_str())).cloned().collect();
        for field in restorable {
            let v = self.stash.remove(&field).expect("key listed above");
            self.raw.insert(field, v);
        }
        if kind == CellKind::Code {
            self.raw.entry("execution_count").or_insert(Value::Null);
            self.raw.entry("outputs").or_insert(Value::Array(vec![]));
        }
        self.raw.insert("cell_type".into(), kind.as_nbformat().into());
    }

    /// nbformat multiline string: either a string or a list of strings.
    pub fn source(&self) -> String {
        multiline(self.raw.get("source"))
    }

    /// Writes `source`, keeping the representation (string or list) the cell already uses.
    /// Returns whether anything changed.
    pub fn set_source(&mut self, source: &str) -> bool {
        if self.raw.contains_key("source") && self.source() == source {
            return false;
        }
        let value = match self.raw.get("source") {
            Some(Value::String(_)) => Value::String(source.to_string()),
            _ => Value::Array(split_keep_newlines(source).into_iter().map(Value::String).collect()),
        };
        self.raw.insert("source".into(), value);
        true
    }

    pub fn outputs(&self) -> &[Value] {
        self.raw.get("outputs").and_then(Value::as_array).map_or(&[], Vec::as_slice)
    }

    pub fn outputs_mut(&mut self) -> &mut Vec<Value> {
        let entry = self.raw.entry("outputs").or_insert(Value::Array(vec![]));
        if !entry.is_array() {
            *entry = Value::Array(vec![]);
        }
        entry.as_array_mut().expect("just ensured")
    }

    pub fn execution_count(&self) -> Option<u64> {
        self.raw.get("execution_count").and_then(Value::as_u64)
    }

    pub fn persisted_id(&self) -> Option<&str> {
        self.raw.get("id").and_then(Value::as_str)
    }

    pub fn is_stale(&self) -> bool {
        self.kind() == CellKind::Code && self.source() != self.runtime.baseline
    }
}

pub fn multiline(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts.iter().filter_map(Value::as_str).collect(),
        _ => String::new(),
    }
}

/// Splits into nbformat's list form: every element but the last ends with `\n`.
pub fn split_keep_newlines(s: &str) -> Vec<String> {
    s.split_inclusive('\n').map(str::to_string).collect()
}
