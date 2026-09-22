# Changelog

All notable user-facing changes to `nvb`. Versions follow [Semantic Versioning](https://semver.org/);
before 1.0, a breaking change bumps the minor version.

## Unreleased

### Fixed

- Opening cells no longer fails with `E303` when Neovim cannot create its swap directory:
  cell buffers never had swap files, but naming them briefly tried to make one.

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
