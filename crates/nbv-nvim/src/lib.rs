//! Neovim adapter: the `NvimClient` boundary, the UI event stream and grid model, and the
//! notebook buffer adapter (core ↔ Neovim buffer). Depends on `nbv-core`, never on the TUI.

pub mod client;
pub mod editor;
pub mod grid;
pub mod redraw;

pub use client::{EmbeddedNvim, NvimClient, NvimError, NvimEvent};
pub use editor::{Calls, Editor, EditorEvent};
pub use grid::Grid;
