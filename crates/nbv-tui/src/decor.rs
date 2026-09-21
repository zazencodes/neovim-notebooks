//! Where outputs go (§11.1): placeholder placements for the companion's `render`.

use nbv_core::{CellKey, CellKind, Notebook};
use rmpv::Value;

/// Placeholder highlight groups (§11.3).
pub const SLOTS: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    pub key: CellKey,
    /// Anchor line: the next cell's marker (output above it), or the last buffer line for the
    /// final cell (output below it).
    pub line: usize,
    pub above: bool,
    pub height: usize,
    pub slot: u16,
}

/// Placements for every code cell whose output is non-empty. Slots are assigned in buffer
/// order modulo `SLOTS`; two outputs sharing a slot are 64 outputs apart and cannot be on
/// screen together, since each takes at least a marker line, a body line and an output row.
pub fn placements(nb: &Notebook, mut height: impl FnMut(&CellKey) -> usize) -> Vec<Placement> {
    let layout = nb.layout();
    let last_line = nb.mirror().len().saturating_sub(1);
    let mut out = vec![];
    for (i, span) in layout.iter().enumerate() {
        if span.kind != CellKind::Code || span.marker.is_none() {
            continue;
        }
        let h = height(&span.key);
        if h == 0 {
            continue;
        }
        // Anchored above the next marker, so lines appended to the cell push the output down.
        let (line, above) = match layout.get(i + 1).and_then(|s| s.marker) {
            Some(next) => (next, true),
            None => (last_line, false),
        };
        out.push(Placement { key: span.key.clone(), line, above, height: h, slot: (out.len() % SLOTS) as u16 });
    }
    out
}

/// The slot → cell map for a set of placements.
pub fn slot_map(placements: &[Placement]) -> Vec<Option<CellKey>> {
    let mut slots = vec![None; SLOTS];
    for p in placements {
        slots[p.slot as usize] = Some(p.key.clone());
    }
    slots
}

pub fn map(entries: Vec<(&str, Value)>) -> Value {
    Value::Map(entries.into_iter().map(|(k, v)| (k.into(), v)).collect())
}

/// Placements as the companion's `render` expects them.
pub fn to_value(placements: &[Placement]) -> Value {
    Value::Array(
        placements
            .iter()
            .map(|p| {
                map(vec![
                    ("line", (p.line as u64).into()),
                    ("above", p.above.into()),
                    ("height", (p.height as u64).into()),
                    ("slot", (p.slot as u64).into()),
                ])
            })
            .collect(),
    )
}
