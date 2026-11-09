# Neovim Notebooks (`nvb`)

Jupyter notebooks in a real Neovim.

<p>
  <a href="https://github.com/zazencodes/neovim-notebooks/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/zazencodes/neovim-notebooks/actions/workflows/ci.yml/badge.svg"></a>
  <a href="LICENSE"><img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-blue"></a>
</p>

`nvb` is **[n]eo[v]im note[b]ook**: a Rust/Ratatui application that owns the terminal,
embeds `nvim --embed` as its editing engine, and runs cells on an unmodified Jupyter kernel.
The notebook is a list of distinct cells with their outputs below them. Move between cells
with Vim keys, press `Enter` to edit one in your own Neovim (config, plugins, LSP, Treesitter),
and press `Esc` or `Ctrl-C` in Normal mode to leave it. Outputs, images included, render inline.

```
nvb analysis.ipynb
```

https://github.com/user-attachments/assets/05df38a2-bb52-4fdd-b78e-aa6ffe8e3304

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
  experimental.

  If the chosen Python lacks `ipykernel`, nbv offers to install it (with that Python's pip, or
  `uv pip install` for a virtualenv without pip) before starting the kernel.
- **tmux 3.3 or newer**, if you run nbv inside tmux (3.5 for `Shift+Enter` / `Ctrl+Enter`, see [tmux](#tmux)).

## Install

```
cargo install nvb
```

This installs the `nvb` binary. macOS and Linux are supported.

<p align="center">
  <a href="https://zazencodes.com/?utm_source=github&utm_medium=referral&utm_campaign=neovim-notebooks">
    <img
      src="docs/assets/zazencodes-banner.webp"
      alt="ZazenCodes — Engineering for the Agentic Era"
      width="100%"
    >
  </a>
  <br>
  Created by <a href="https://zazencodes.com/">ZazenCodes</a>
</p>

## Use

`nvb --create <notebook.ipynb>` creates an empty Python 3 notebook and exits. The parent
directory must exist; an existing path is always refused. Open it normally, then use the
header kernel picker to select another kernel.

`nvb <notebook.ipynb>` opens the notebook. Each cell is a box with its execution count on the
left and its outputs underneath. nbv has two modes, shown in the header.

**Navigation mode** (`NAV`) acts on whole cells:

| Key | Action |
|---|---|
| `j` `k` | Next / previous cell; counts work (`3j`) |
| `gg` `G` `{n}G` | First / last / nth cell |
| `<C-d>` `<C-u>` | Scroll half a page and select the cell in the middle of the screen |
| `<C-e>` `<C-y>` | Scroll a line down / up, as in Vim; counts work (`5<C-e>`). The mouse wheel scrolls three lines |
| `za` | Collapse / expand the selected cell's outputs |
| `zt` `zz` `zb` | Scroll the cell to the top / center / bottom of the screen, as in Vim. `zb` puts the end of the cell's outputs on the bottom line |
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
| `/` `n` `N` | Search the whole notebook; next / previous cell with a match |
| `:` | Neovim's command line: `:w`, `:wq`, `:q!`, `:Telescope`, anything |
| `?` | List every key (`q` closes it) |

**The header** sits above the first cell: press `k` on the first cell to reach it, `h` `l` to
select the file name or the kernel, and `Enter` to rename the file or pick the kernel. `j`
goes back to the cells. `gg` and `G` always stay on cells.

**Edit mode** (`EDIT`) is Neovim, in a window that holds only that cell. Motions, search,
undo and text objects stop at the cell's edges. Your config applies as in any buffer: code
cells are buffers of the notebook's language, markdown cells are `markdown` buffers.
Markdown plugins such as [render-markdown.nvim](https://github.com/MeanderingProgrammer/render-markdown.nvim)
or [markview.nvim](https://github.com/OXY2DEV/markview.nvim) render markdown cells, in navigation
mode as well; without one, markdown cells show their highlighted source.

`<Esc>` or `<C-c>` in Normal mode leaves the cell (from Insert mode, `<Esc><Esc>` or
`<C-c><C-c>`), and clears search highlighting. `Shift+Enter` runs the cell and moves to the next one; `Ctrl+Enter` runs it and
keeps you editing. Both work from Normal and Insert mode.

`Shift+Enter` and `Ctrl+Enter` need a terminal that reports modifier keys: Kitty, Ghostty,
WezTerm and iTerm2 do; macOS Terminal does not, and there they act as plain `Enter`. `x` and
`r` work in every terminal.

Actions without a key are Ex commands: `:NvbRunAll`, `:NvbRunAbove`, `:NvbSplit` (split the
edited cell at the cursor), and `:NvbClearOutput[!]`. They act on the cell being edited, or the
selected one.

**Search.** `/` in navigation mode is Neovim's search prompt, over every cell: `Enter` selects
the next cell with a match, and `n` `N` go on to the next or previous one, wrapping around.
`'ignorecase'`, `'smartcase'` and `'wrapscan'` apply as in any buffer. Matches are highlighted
in every cell, and inside a cell `n` moves between them as usual.

**Inspecting outputs.** Inline output shows the last 40 text rows. `:NvbOutput` opens a
read-only Neovim scratch buffer containing every output from the current cell, at full length.
Use `/`, `n`, Visual selection, `y` or `yy` as usual; `q` or `Esc` in Normal mode closes it.
This is a snapshot; close and reopen it to see new output. ANSI colors are removed for text
inspection. Rich output prefers `text/plain` (including the kernel's formatted tables), then
`application/vnd.dataresource+json` as tab-separated rows, then formatted `application/json`.
Other MIME types are listed explicitly. `:NvbOutput text/html` requests that MIME's source
for rich outputs; a missing representation reports an error. Source search in navigation
mode continues to search cell source. `za` toggles inline output collapse per cell for this
session; it preserves the output in the notebook and in the full viewer.

`:NvbOpenOutput <output-number> <mime>` deliberately opens one rich representation externally,
using `open` on macOS or `xdg-open` on Linux. Numbers in the full viewer start at 1. For example,
`:NvbOpenOutput 2 text/html` opens the second output's HTML, and
`:NvbOpenOutput 1 image/svg+xml` opens SVG. Supported MIME types are `text/html`, `image/svg+xml`,
`text/plain`, `text/csv`, `application/json`, `image/png`, `image/jpeg` and `image/gif`.
The command requires an explicit MIME and reports missing or unsupported data and opener
failures. HTML and SVG run in your external application's security context and may load
remote resources or active content: open only trusted output. Nothing opens automatically.
Extracted files live in a private temporary directory until nbv exits; save a copy in the
external application to keep one. The notebook retains every original MIME representation.

**Saving.** The notebook saves itself about a second after a change, cell outputs included,
whenever Neovim is in Normal mode: never while you type in Insert mode or on the command line.
`:w` from anywhere writes the `.ipynb` too (and so do `:wq` and `:x`, as in any
Neovim buffer), and only the `.ipynb`: nothing is written under the cell buffers' names. Format-on-save works per cell,
because `BufWritePre` and `BufWritePost` fire around the commit on every changed cell, on every save, but write hooks see buffers,
not files. Opening and saving without edits loses nothing, and older (4.0–4.4)
notebooks gain cell ids only once you change something.

**Crash recovery.** Independently of autosave, nbv snapshots unsaved notebook sources,
cell structure and outputs about once a second, including while you stay in Insert mode.
Snapshots do not run format-on-save hooks. Encoding and durable writes run in the background;
unchanged outputs, including images, are reused between snapshots. A crash can lose edits
since the last completed snapshot, normally about a second. Slow disk writes can extend that
interval; edits coalesce while a write is in progress.

Recovery lives under `$XDG_STATE_HOME/nbv/recovery/` (by default
`~/.local/state/nbv/recovery/`), associated with the notebook's canonical absolute path.
File and directory symlink aliases share recovery and the session lock. After a crash,
opening that path offers **Recover unsaved snapshot** or **Discard snapshot and open disk
file**, showing the full file name. Cancelling leaves the snapshot available for next time.
Recovering retains the saved baseline: an externally changed or deleted notebook cannot be
overwritten by an ordinary save; use `:e!` to load the disk version or explicitly `:w!` to
keep the recovered version. A damaged or unreadable snapshot is reported with both paths.

Recovery follows a rename; renaming an opened file symlink moves that link in its directory
and keeps its real target in place. A successful save, an accepted external reload, or a clean quit
(including `:q!`, which explicitly discards changes) removes it. The same notebook path can
be open in only one nbv process at a time, to protect its recovery data.

**Changes on disk.** When another program changes the `.ipynb`, nbv reloads it, as Neovim's
`'autoread'` does, keeping the cell you are editing open. If you also have unsaved changes,
nbv asks whether to load the file (discarding them) or keep yours (overwriting it). Either
happens only in Normal mode, so a prompt never takes the keys you are typing. Dismissing the
prompt keeps your changes unsaved: `:e!` loads the file, `:w!` overwrites it.

**Errors.** When a cell cannot run (no kernel, the kernel failed to start, the kernel died),
the reason appears in red under the cell, and the header marks the kernel failed. Nothing
falls back quietly: a notebook nbv cannot read correctly (an unknown cell type, missing
metadata) is refused at startup with the reason, and an image that cannot be decoded says so
where it would be.

**Language servers.** Python notebooks can share one language-server document across code
cells, while keeping separate editable cell buffers. Configure a server in your Neovim as
usual, then opt in after its configuration is available:

```lua
vim.lsp.config('pyright', {
  cmd = { 'pyright-langserver', '--stdio' },
  filetypes = { 'python' },
})
vim.lsp.enable('pyright')
if vim.g.nbv then
  require('nbv.lsp').enable('pyright')
end
```

This uses your server command, settings, capabilities, project root and callbacks. The chosen
server attaches once to the notebook; ordinary files retain their usual automatic attachment.
Diagnostics, completion (including resolved auto-import edits into another cell), definitions,
references and rename use all code cells in notebook order, including unsaved and offscreen
cells. Your existing LSP mappings and completion plugins continue to use cell buffers.
`:lsp restart` works from a cell.

The integration currently supports **Python source**, with Pyright tested. Markdown and raw
cells are excluded. IPython magics and shell escapes remain verbatim and may report Python
syntax errors. Code follows textual notebook order, rather than kernel execution history.
Formatting, semantic tokens and other features without supported coordinate mapping are
unavailable on the notebook client. See [language-server support](docs/language-servers.md)
for capability details and the transport design. Without opt-in, servers retain their usual
per-cell view.

**Troubleshooting.** `nvb --clean` starts Neovim without your configuration. If a problem goes
away with `--clean`, its source is in your config or plugins. Kernel channel and send failures
fail queued and running cells; press `00` to restart or choose a kernel in the header.
Unexpected embedded Neovim exits report its exit status, RPC failure and the last 16 KiB of
stderr after restoring the terminal. Kernel startup and death errors include the same bounded
stderr tail, even when a process prints a single huge line.

Lifecycle diagnostics are local in `$XDG_STATE_HOME/nbv/diagnostics.log` (default
`~/.local/state/nbv/diagnostics.log`). The log rotates to `diagnostics.log.1`; each file is at
most 64 KiB and created readable only by your user. An empty `diagnostics.lock` coordinates
rotation across open notebooks. It records lifecycle events and kernel
generation numbers. Notebook source, outputs, input answers, environment variables, connection
keys and raw process stderr are excluded from the log. When reporting a failure, include the
terminal error and these logs; review the terminal error first, since a kernel or plugin can
include private information in its stderr. These files can be removed at any time.

## Images

nbv picks a graphics mechanism per terminal. Where none works, images fall back to halfblocks
(coloured text), so output is always visible.

| Terminal | Direct | Inside tmux |
|---|---|---|
| Kitty | Kitty protocol | Kitty Unicode placeholders |
| Ghostty | Kitty protocol | Kitty Unicode placeholders |
| WezTerm | iTerm2 protocol | tmux-native Sixel if tmux has Sixel, else halfblocks |
| Others | Sixel if detected, else halfblocks | tmux-native Sixel if available, else halfblocks |

**Verification status.** The automated suite checks placement and clipping with halfblocks
inside tmux. Pixel-level behaviour on real terminals, including whether Neovim floats cover images
correctly per protocol, is checked by hand. That has not been done yet, so no terminal is
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

Images need no tmux configuration. In Kitty or Ghostty, nbv turns on `allow-passthrough` for
its own pane, so images reach your terminal at full resolution. Passthrough lets programs in
that pane send escape sequences straight to your terminal, bypassing tmux.

## Development

```
cargo test --workspace
```

The Neovim-driven tests use `$NBV_NVIM`, or `.tools/nvim-macos-arm64/bin/nvim` when present,
and skip themselves without Neovim 0.12. The kernel and tmux tests use a `.venv` at the repository
root with `ipykernel` and `ruff` (`uv venv .venv && uv pip install --python .venv/bin/python
ipykernel ruff`). The real language-server test requires Pyright 1.1.414:

```sh
npm install --prefix .tools/pyright --no-audit --no-fund pyright@1.1.414
NBV_LSP_SERVER="$PWD/.tools/pyright/node_modules/.bin/pyright-langserver" cargo test -p nbv-nvim --test language_server
```

Without `NBV_LSP_SERVER`, that test reports a skip locally; CI requires it and provisions the
server. CI runs everything on Linux and macOS.

| Crate | Role |
|---|---|
| `nbv-core` | Lossless document, cell identity, structural changes with undo, persistence, kernel state machine and transport. Depends on neither Neovim nor Ratatui (`scripts/check-core-deps.sh`). |
| `nbv-nvim` | `NvimClient` boundary, redraw stream and grid model, cell buffer adapter, and the integration harness (`harness` feature). |
| `nbv-tui` | Event loop, notebook layout, navigation mode, compositor, output layer, image backend, terminal and tmux detection. |
| `nvb` (`crates/nbv-cli`) | The `nvb` binary, and the tmux end-to-end harness. |
| `crates/nbv-nvim/lua/nbv` | The Lua companion loaded into the embedded Neovim. It holds no notebook state. |

[Recovery performance](docs/recovery-performance.md) includes a repeatable release-mode
component profile and the remaining synchronous save costs.

`tests/corpus/` holds deliberately ugly notebooks. Every one must survive open → save without
losing information.

### Stream scaling benchmark

Run the same release workload against a pristine baseline snapshot and this checkout:

```sh
CARGO_TARGET_DIR="$PWD/target" CARGO_BUILD_JOBS=2 scripts/bench-stream.sh 68a896b
```

Use your isolated build-cache path for `CARGO_TARGET_DIR` when working alongside other
checkouts. The script leaves the checkout alone, removes its temporary baseline snapshot,
and uses the supplied build directory for both builds. Before each build it cleans only the
four workspace packages' release artifacts, keeping third-party dependencies while forcing
path-dependent code and embedded harness paths to rebuild. It measures kernel-message handling
alone and handling plus rebuilding the inline view after every 62-byte ANSI/UTF-8 stream
chunk, at width 80. Each run verifies that every output line remains in the document.
This is a component benchmark of `Executor::handle` and `outputs::build_cached`; application
layout, image preparation and terminal drawing are outside its scope. Compilation and
notebook setup are outside the timed region.

Measured on an Apple Silicon Mac on 2026-11-09 (milliseconds, one run, concurrent development
workloads; compare scaling rather than treating the timings as fixed thresholds):

| Chunks | Baseline append | Incremental append | Baseline append + view | Incremental append + view |
|---:|---:|---:|---:|---:|
| 1,000 | 63.164 | 0.273 | 232.794 | 11.605 |
| 4,000 | 1,008.576 | 1.045 | 3,954.077 | 60.665 |
| 16,000 | 17,778.293 | 5.910 | 96,169.023 | 559.641 |

These measurements were repeated with workspace release artifacts cleaned before each
build, and supersede the initial shared-cache measurements. At 16,000 chunks, this
component's handling plus rendering is about 172× faster. Concurrent load affects elapsed
scaling; the parsed-byte regression independently verifies that later-line rewrites do not
reparse earlier history. These figures do not measure whole-application throughput.
Width changes, reloads and explicit display replacements rebuild derived text from the full
saved output. Appends reuse the bounded rendered tail, including across stdout/stderr
boundaries and within a long unterminated line.

## License

[MIT](LICENSE)
