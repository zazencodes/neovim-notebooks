# Changelog

All notable user-facing changes to `nvb`. Versions follow [Semantic Versioning](https://semver.org/);
before 1.0, a breaking change bumps the minor version.

## Unreleased

### Fixed

- A cell's output no longer goes missing when the kernel reports the cell finished before the
  output arrives. Large outputs such as images were often dropped.
- Images inside tmux in Kitty or Ghostty are drawn at full resolution instead of halfblocks.
  nbv turns on `allow-passthrough` for its own pane; no tmux configuration is needed.

## 0.1.4 - 2026-10-25

### Added

- `zt` and `zb` in navigation mode scroll the selected cell to the top or bottom of the screen,
  alongside `zz`. `zb` puts the end of the cell's outputs on the bottom line, so a long output
  ends at the bottom of the screen.

## 0.1.3 - 2026-10-13

### Changed

- nbv is published on crates.io. Install it with `cargo install nvb`; no `--git` or `--tag` is needed,
  and the package is now named `nvb`, after the binary.

## 0.1.2 - 2026-10-13

### Added

- The notebook saves itself about a second after a change, whenever Neovim is in Normal mode.
- Changes another program makes to the `.ipynb` load into nbv on their own. If you also have
  unsaved changes, nbv asks whether to load the file or keep yours.
- `/` in navigation mode searches the whole notebook, and `n` `N` go to the next or previous
  cell with a match.
- `<C-e>` and `<C-y>` in navigation mode scroll the notebook a line at a time, as in Vim, and
  the mouse wheel scrolls it three lines per notch. The selected cell stays selected until it
  scrolls out of view. Because nbv now takes mouse input, selecting text with the mouse
  needs your terminal's modifier, usually Shift (Option in iTerm2).

### Changed

- `:e!` keeps the cell you are editing open, with the text from disk, instead of closing it.

## 0.1.1 - 2026-10-12

### Fixed

- Opening cells no longer fails with `E303` when Neovim cannot create its swap directory:
  cell buffers never had swap files, but naming them briefly tried to make one.
- Kernel ports stay reserved until the kernel is launched, narrowing the moment in which
  another program could take them.

## 0.1.0 - 2026-09-22

First public release.

### Added

- Open `.ipynb` notebooks with `nvb <notebook.ipynb>`: cells as boxes with execution counts and
  outputs, images included, rendered inline.
- Navigation mode with Vim keys for whole cells: select, add, delete, yank, paste, join, reorder,
  change type, and undo/redo of structural changes.
- Edit mode: each cell opens in the embedded Neovim with your own config, plugins, LSP and
  Treesitter; motions, search and undo stop at the cell's edges.
- Run cells on an unmodified Jupyter kernel (`x`, `r`, `Shift+Enter`, `Ctrl+Enter`), interrupt
  and restart it, plus `:NvbRunAll`, `:NvbRunAbove`, `:NvbSplit` and `:NvbClearOutput[!]`.
- Kernel selection from the active virtualenv, project virtualenvs, installed kernelspecs and
  `python3`, remembered per notebook; offers to install `ipykernel` when it is missing.
- Header with file rename and kernel picker, and a `?` help float listing every key.
- Lossless saving with `:w`: per-cell format-on-save, refusal to overwrite a notebook changed on
  disk, and no information lost on open → save.
- Rendered markdown cells via render-markdown.nvim or markview.nvim, line wrapping, and output
  text wrapping.
- Images through the Kitty, iTerm2 or Sixel protocols, falling back to halfblocks, including
  inside tmux.
