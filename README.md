# Neovim Notebooks (`nvb`)

Jupyter notebooks in a real Neovim. `nvb` is a Rust/Ratatui application that owns the terminal,
embeds `nvim --embed` as its editing engine, and runs cells on an unmodified Jupyter kernel.
The notebook is a list of distinct cells with their outputs below them. Move between cells
with Vim keys, press `Enter` to edit one in your own Neovim (config, plugins, LSP, Treesitter),
and press `Esc` or `Ctrl-C` in Normal mode to leave it. Outputs, images included, render inline.

```
nvb analysis.ipynb
```

One notebook application. No notebook plugin stack. No nbv-specific configuration.

The design lives in [`docs/specs/2026-09-20-nbv-v1-architecture.md`](docs/specs/2026-09-20-nbv-v1-architecture.md),
as amended by [`docs/specs/2026-09-21-nbv-cell-editors.md`](docs/specs/2026-09-21-nbv-cell-editors.md).

## Requirements

- **Neovim 0.12 or newer** on `PATH`, or passed with `--nvim` / `$NBV_NVIM`. nbv does not ship Neovim.
- **A Jupyter kernel** for running cells. nbv picks one at startup, in order:
  1. the active virtualenv (`$VIRTUAL_ENV/bin/python -m ipykernel_launcher`), for Python notebooks;
  2. a virtualenv in the directory you start nbv from (such as `.venv`), for Python notebooks;
  3. the notebook's kernelspec (`metadata.kernelspec.name`) in the Jupyter data directories;
  4. `python3 -m ipykernel_launcher` on `PATH`.

  The header shows which kernel is in use. Selecting it there (see below) picks another from
  the active virtualenv, the virtualenvs in the working directory, every installed kernelspec
  and `python3` on `PATH`; choosing a
  kernelspec saves it in the notebook. nbv remembers your pick for each notebook
  (`~/.local/state/nbv/kernels.json`) and starts with it next time, ahead of the order above. Code cells are edited in the kernel's language. Non-Python kernels are
  experimental (spec §4.3).

  If the chosen Python lacks `ipykernel`, nbv offers to install it (with that Python's pip, or
  `uv pip install` for a virtualenv without pip) before starting the kernel.
- **tmux 3.3 or newer**, if you run nbv inside tmux (3.5 for `Shift+Enter` / `Ctrl+Enter`, see [tmux](#tmux)).

## Install

```
cargo install --path crates/nbv-cli
```

## Use

`nvb <notebook.ipynb>` opens the notebook. Each cell is a box with its execution count on the
left and its outputs underneath. nbv has two modes, shown in the header.

**Navigation mode** (`NAV`) acts on whole cells:

| Key | Action |
|---|---|
| `j` `k` | Next / previous cell; counts work (`3j`) |
| `gg` `G` `{n}G` | First / last / nth cell |
| `<C-d>` `<C-u>` | Scroll half a page |
| `Enter` | Edit the cell |
| `o` `O` | New code cell below / above, selected; repeat to add several |
| `dd` `yy` `p` `P` | Delete / yank / paste below / paste above |
| `u` `<C-r>` | Undo / redo a cell change (delete, move, type, join, paste…) |
| `J` | Join the next cell onto this one |
| `]e` `[e` | Move the cell down / up |
| `tc` `tm` `tr` | Make it code / markdown / raw |
| `x` `Shift+Enter` | Run and select the next cell (past the end, add one and edit it) |
| `r` `Ctrl+Enter` | Run and stay on the cell |
| `ii` `00` | Interrupt / restart the kernel |
| `:` | Neovim's command line: `:w`, `:wq`, `:q!`, `:Telescope`, anything |
| `?` | List every key (`q` closes it) |

**The header** sits above the first cell: press `k` on the first cell to reach it, `h` `l` to
select the file name or the kernel, and `Enter` to rename the file or pick the kernel. `j`
goes back to the cells. `gg` and `G` always stay on cells.

**Edit mode** (`EDIT`) is Neovim, in a window that holds only that cell. Motions, search,
undo and text objects stop at the cell's edges. Your config applies as in any buffer: code
cells are buffers of the notebook's language, markdown cells are `markdown` buffers.

`<Esc>` or `<C-c>` in Normal mode leaves the cell (from Insert mode, `<Esc><Esc>` or
`<C-c><C-c>`), and clears search highlighting. `Shift+Enter` runs the cell and moves to the next one; `Ctrl+Enter` runs it and
keeps you editing. Both work from Normal and Insert mode.

`Shift+Enter` and `Ctrl+Enter` need a terminal that reports modifier keys: Kitty, Ghostty,
WezTerm and iTerm2 do; macOS Terminal does not, and there they act as plain `Enter`. `x` and
`r` work in every terminal.

Actions without a key are Ex commands: `:NbvRunAll`, `:NbvRunAbove`, `:NbvSplit` (split the
edited cell at the cursor), and `:NbvClearOutput[!]`. They act on the cell being edited, or the
selected one.

**Saving.** `:w` from anywhere writes the `.ipynb` (and so do `:wq` and `:x`, as in any
Neovim buffer), and only the `.ipynb`: nothing is written under the cell buffers' names. Format-on-save works per cell,
because `BufWritePre` and `BufWritePost` fire around the commit, but write hooks see buffers,
not files. Running cells counts as a change, so `:q` asks you to save outputs (`:q!` discards
them). A notebook changed on disk since it was loaded is not overwritten: `:w!` overwrites it,
`:e!` reloads it. Opening and saving without edits loses nothing, and older (4.0–4.4)
notebooks gain cell ids only once you change something.

**Errors.** When a cell cannot run (no kernel, the kernel failed to start, the kernel died),
the reason appears in red under the cell, and the header marks the kernel failed. Nothing
falls back quietly: a notebook nbv cannot read correctly (an unknown cell type, missing
metadata) is refused at startup with the reason, and an image that cannot be decoded says so
where it would be.

**Language servers.** Each cell is its own buffer, so a language server sees each cell as a
separate file. Names defined in one cell show as undefined in the next. This is a known gap.

**Troubleshooting.** `nvb --clean` starts Neovim without your configuration. If a problem goes
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

For `Shift+Enter` and `Ctrl+Enter`, tmux (3.5 or newer) must pass modified keys on in the
form nbv reads. Without these lines the header shows `⚠ tmux: Shift/Ctrl+Enter off`, `?`
lists what to add, and everything else works:

```tmux
set -s extended-keys on
set -s extended-keys-format csi-u
set -as terminal-features 'xterm*:extkeys'
```

`extended-keys on` (not `always`) changes nothing for programs that don't ask for modified
keys, such as your shell. The third line makes tmux ask your terminal for them.

Images need no tmux configuration: they are drawn as coloured text (halfblocks) inside tmux
by default. For sharp images in Kitty or Ghostty, enable passthrough:

```tmux
set -g allow-passthrough on
```

Passthrough lets any program in a pane send escape sequences straight to your terminal,
bypassing tmux. Leave it off if you don't want that.

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
| `nbv-core` | Lossless document, cell identity, structural changes with undo, persistence, kernel state machine and transport. Depends on neither Neovim nor Ratatui (`scripts/check-core-deps.sh`). |
| `nbv-nvim` | `NvimClient` boundary, redraw stream and grid model, cell buffer adapter, and the integration harness (`harness` feature). |
| `nbv-tui` | Event loop, notebook layout, navigation mode, compositor, output layer, image backend, terminal and tmux detection. |
| `nbv-cli` | The `nvb` binary, and the tmux end-to-end harness. |
| `lua/nbv` | The Lua companion loaded into the embedded Neovim. It holds no notebook state. |

`tests/corpus/` holds deliberately ugly notebooks. Every one must survive open → save without
losing information.
