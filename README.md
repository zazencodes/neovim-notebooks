# Neovim Notebooks (`nbv`)

Jupyter notebooks in a real Neovim. `nbv` is a Rust/Ratatui application that owns the terminal,
embeds `nvim --embed` as its editing engine, and runs cells on an unmodified Jupyter kernel.
The whole notebook is one ordinary Neovim buffer, so your `init.lua`, plugins, LSP, Treesitter,
motions, macros and search work across every cell. Outputs, images included, render inline.

```
nbv analysis.ipynb
```

One notebook application. No notebook plugin stack. No nbv-specific configuration.

The design lives in [`docs/specs/2026-09-20-nbv-v1-architecture.md`](docs/specs/2026-09-20-nbv-v1-architecture.md).

## Requirements

- **Neovim 0.12 or newer** on `PATH`, or passed with `--nvim` / `$NBV_NVIM`. nbv does not ship Neovim.
- **Python with `ipykernel`** for running cells. nbv looks, in order, for:
  1. the active virtualenv (`$VIRTUAL_ENV/bin/python -m ipykernel_launcher`), for Python notebooks;
  2. the notebook's kernelspec (`metadata.kernelspec.name`) in the Jupyter data directories;
  3. `python3 -m ipykernel_launcher` on `PATH`.

  Other kernels resolve through their kernelspec and are experimental (spec §4.3).
- **tmux 3.3 or newer**, if you run nbv inside tmux.

## Install

```
cargo install --path crates/nbv-cli
```

## Use

`nbv <notebook.ipynb>` opens the notebook as one Python buffer. Each cell starts with a marker
line in jupytext's percent format, carrying the cell's identity:

```python
# %% [markdown] id="e5f6a7b8"
# ## Results
# %% id="a1b2c3d4"
import numpy as np
np.arange(3)
```

Edit freely: yank and paste cells, delete and undo them, run formatters over the whole buffer.
Outputs follow their cells, because identity lives in the marker text. Typing `# %%` on its own
line starts a new cell. Its marker gains an id when you leave insert mode.

| Command | Default key | Action |
|---|---|---|
| `:NbvRun` | `<localleader>x` | Run cell under cursor |
| `:NbvRunAdvance` | `<S-Enter>`, `<localleader><CR>` | Run and move to the next cell |
| `:NbvRunAll` | `<localleader>X` | Run every cell |
| `:NbvRunAbove` | `<localleader>ba` | Run all cells above the cursor |
| `:NbvInterrupt` | `<localleader>i` | Interrupt the kernel |
| `:NbvRestart` | `<localleader>R` | Restart the kernel |
| `:NbvCellAdd[!]` | `<localleader>o` / `O` | New cell below / above |
| `:NbvCellDelete` | `<localleader>dd` | Delete cell |
| `:NbvCellSplit` | `<localleader>s` | Split at cursor |
| `:NbvCellMerge` | `<localleader>m` | Merge with next |
| `:NbvCellMove {up\|down}` | `<localleader>k` / `j` | Reorder |
| `:NbvCellType {code\|markdown\|raw}` | `<localleader>tc` / `tm` / `tr` | Change type |
| `:NbvClearOutput[!]` | `<localleader>c` | Clear cell / all outputs |

`]c` and `[c` move between cells; `ic` and `ac` are cell text objects. Keymaps are buffer-local
to the notebook. Set `vim.g.nbv_no_default_keymaps = true` to skip them; the commands stay.
`<S-Enter>` needs a terminal that reports modified keys (the Kitty keyboard protocol, or tmux's
`extended-keys`). The `<localleader>` bindings work everywhere.

**Saving.** `:w`, `:wq`, `:x`, `ZZ` and `:update` write the `.ipynb`, and only the `.ipynb`: the
projected Python text never touches disk. Format-on-save works, because `BufWritePre` and
`BufWritePost` fire around the commit, but write hooks see the buffer, not a file. A notebook
changed on disk since it was loaded is not overwritten: `:w!` overwrites it, `:e!` reloads it.
Opening and saving without edits loses nothing, and older (4.0–4.4) notebooks gain cell ids only
once you change something.

**Troubleshooting.** `nbv --clean` starts Neovim without your configuration. If a problem goes
away with `--clean`, its source is in your config or plugins.

## Images

nbv picks a graphics mechanism per terminal. Where none works, images fall back to halfblocks
(coloured text), so output is always visible.

| Terminal | Direct | Inside tmux |
|---|---|---|
| Kitty | Kitty protocol | Kitty Unicode placeholders (needs `allow-passthrough`) |
| Ghostty | Kitty protocol | Kitty Unicode placeholders (needs `allow-passthrough`) |
| WezTerm | iTerm2 protocol | tmux-native Sixel if tmux has Sixel, else halfblocks |
| Others | Sixel if detected, else halfblocks | tmux-native Sixel if available, else halfblocks |

**Verification status.** The automated suite checks placement and clipping with halfblocks
inside tmux. Pixel-level behaviour on real terminals, including whether Neovim floats cover images
correctly per protocol, is the manual Spike 6 matrix. It has not been run yet, so no terminal is
called first-class so far.

### tmux

nbv checks these options at startup and names any that are missing:

```tmux
set -g allow-passthrough on           # images via Kitty placeholders
set -s extended-keys on               # <S-Enter> and other modified keys
set -s focus-events on                # resend images after reattach
set -as terminal-features ',*:RGB'    # true colour
```

## Development

```
cargo test --workspace
```

The Neovim-driven tests use `$NBV_NVIM`, or `.tools/nvim-macos-arm64/bin/nvim` when present,
and skip themselves without Neovim 0.12. The kernel and tmux tests use a `.venv` at the repository
root with `ipykernel` and `ruff` (`uv venv .venv && uv pip install --python .venv/bin/python
ipykernel ruff`). CI runs everything on Linux and macOS.

| Crate | Role |
|---|---|
| `nbv-core` | Lossless document, cell identity, projection and reconciliation, persistence, kernel state machine and transport. Depends on neither Neovim nor Ratatui (`scripts/check-core-deps.sh`). |
| `nbv-nvim` | `NvimClient` boundary, redraw stream and grid model, notebook buffer adapter, and the integration harness (`harness` feature). |
| `nbv-tui` | Event loop, compositor, output layer, image backend, terminal and tmux detection. |
| `nbv-cli` | The `nbv` binary, and the tmux end-to-end harness. |
| `lua/nbv` | The Lua companion loaded into the embedded Neovim. It holds no notebook state. |

`tests/corpus/` holds deliberately ugly notebooks. Every one must survive open → save without
losing information.
