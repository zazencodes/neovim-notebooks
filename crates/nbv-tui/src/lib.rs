//! Ratatui frontend: the event loop, the compositor, the output layer and the image backend.
//! It owns the terminal; Neovim never touches it (§5.1).

pub mod ansi;
pub mod app;
pub mod compose;
pub mod keys;
pub mod outputs;
pub mod query;
pub mod terminal;

pub use app::{Options, run};
