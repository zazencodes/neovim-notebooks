//! Neovim Notebooks core. Depends on neither Neovim nor Ratatui (R5).

pub mod adapter;
pub mod document;
pub mod exec;
pub mod kernel;
pub mod key;
pub mod notebook;
pub mod structure;

pub use adapter::{CellKind, LanguageProjection, Marker, PythonProjection};
pub use document::{Cell, ExecState, Runtime};
pub use key::CellKey;
pub use notebook::{CellSpan, CommitError, CommitOptions, LineEdit, Notebook, OpenError, Reconciled};
