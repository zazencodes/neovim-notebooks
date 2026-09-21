//! Neovim adapter: the `NvimClient` boundary, the UI event stream and grid model, and the cell
//! buffer adapter (core ↔ Neovim buffers). Depends on `nbv-core`, never on the TUI.

pub mod client;
pub mod editor;
pub mod grid;
#[cfg(feature = "harness")]
pub mod harness;
pub mod redraw;

pub use client::{EmbeddedNvim, NvimClient, NvimError, NvimEvent};
pub use editor::{Calls, Editor, EditorEvent, EditorRect, Focus, Viewport};
pub use grid::Grid;
