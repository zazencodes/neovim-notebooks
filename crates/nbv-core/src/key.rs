//! Cell identity (§6.2).

use std::collections::HashSet;
use std::fmt;

/// A cell's identity for the lifetime of a session. Unique among live and tombstoned cells.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CellKey(String);

impl CellKey {
    pub fn new(s: impl Into<String>) -> Self {
        CellKey(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether `s` is a valid nbformat 4.5 cell id: 1–64 characters of `[a-zA-Z0-9-_]`.
    pub fn is_valid_nbformat_id(s: &str) -> bool {
        (1..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    }
}

impl fmt::Debug for CellKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

impl fmt::Display for CellKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Mints fresh keys that never collide with any key it has seen.
#[derive(Default, Debug)]
pub struct KeyMinter {
    used: HashSet<CellKey>,
}

impl KeyMinter {
    /// Records `key` as used. Returns false if it was already used.
    pub fn reserve(&mut self, key: &CellKey) -> bool {
        self.used.insert(key.clone())
    }

    pub fn is_used(&self, key: &CellKey) -> bool {
        self.used.contains(key)
    }

    /// Jupyter's convention: 8 random hex characters.
    pub fn mint(&mut self) -> CellKey {
        loop {
            let key = CellKey(format!("{:08x}", rand::random::<u32>()));
            if self.used.insert(key.clone()) {
                return key;
            }
        }
    }
}
