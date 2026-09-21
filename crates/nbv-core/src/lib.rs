//! Neovim Notebooks core. Depends on neither Neovim nor Ratatui (R5).

pub mod document;
pub mod exec;
pub mod kernel;
pub mod key;
pub mod notebook;

pub use document::{Cell, CellKind, ExecState, Runtime};
pub use key::CellKey;
pub use notebook::{Change, CommitError, CommitOptions, Notebook, OpenError};
