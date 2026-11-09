# Cross-cell language-server context

Call `require('nbv.lsp').enable('pyright')` after configuring that named server, guarded by
`if vim.g.nbv then ... end` in a shared Neovim configuration. Exactly one server configuration
can own notebook context. This API does not install servers. Python is the supported language;
Pyright 1.1.414 is exercised in CI. Existing per-cell attachment remains the default without
this call.

## Supported operations

- Push diagnostics with document versions, and pull diagnostics (including Pyright's dynamic
  registration). Old document versions and results for replaced snapshots are discarded.
  Unversioned push diagnostics cannot be safely assigned after cell movement; a server without
  pull diagnostics reports an explicit error describing this requirement.
- Completion, textEdit and insert/replace edits, CompletionList default edit ranges,
  additionalTextEdits into any code cell, and completionItem/resolve. Auto-imports can edit an
  earlier, offscreen cell. Server-private completion data round-trips unchanged.
- Hover, signature help, definitions/declarations/type definitions, references and
  workspace symbols; Locations and LocationLinks point at cell buffers. Synthetic separator
  locations are removed. Location extents crossing a cell boundary are clipped to their cell.
- Prepare rename (both Range and range/placeholder results), rename and workspace edits,
  including changes, documentChanges, server-initiated workspace/applyEdit and ordinary-file
  edits alongside cell edits. An edit crossing a cell boundary or targeting a synthetic line
  fails explicitly before applying any part of the workspace edit. Resource operations on
  the synthetic document are refused. Notebook cells are inserted/deleted through notebook
  structural commands; a language server cannot create or remove them with file operations.
- UTF-8 and UTF-16 positions: projection changes line numbers only. Characters retain the
  encoding negotiated by Neovim. Neovim applies text edits with that client's encoding.

Document formatting (including range/on-type formatting), semantic tokens, folding ranges,
inlay hints, document symbols, code lenses/actions, selection ranges, document highlights,
linked editing and call/type hierarchy are not exposed by the notebook transport. Their
result shapes need separate mappings. Ordinary-file clients keep their capabilities. Cell
formatting through non-LSP plugins still sees the actual cell text.

Only Python code cells are concatenated. Markdown and raw cells contribute no source. Blank
separator lines between code cells have no cell identity. Source is preserved verbatim;
IPython magics, shell escapes and polyglot cells can produce Python errors. This is static
analysis of notebook order, independent of the order in which cells ran in the kernel.

## Transport and ownership

Rust remains the canonical notebook owner. The companion reports opt-in with Ready, or an
explicit enable notification when enabled later. Only then does Rust build the initial ordered
snapshot of code-cell keys, names, filetypes and sources. Structural synchronization, reload,
language switches and notebook rename refresh that snapshot. Placement/scroll RPCs contain no
notebook document, and neither rebuild sources nor refresh the projection in Lua. The cell
factory is registered separately on the first layout and retained for offscreen materialization.
`nbv.lsp` overlays live buffer
text on that snapshot, including changes whose Rust line notification is still in flight.
The module caches projection metadata for protocol translation; it never persists source or
changes notebook structure. Its one synthetic URI is `<notebook>.ipynb.nvb.py`, beside the
notebook for normal project/import resolution, and is never written to disk.

Neovim 0.12's LSP client supports a custom RPC transport. It does not implement notebook-cell
synchronization. This integration uses its documented `cmd(dispatchers, config)` factory and
an ordinary text document, which Pyright supports. The transport suppresses per-cell open,
change, close and save notifications and sends one synthetic didOpen followed by versioned
full-document changes. Full replacements are valid for incremental-sync servers as well.
Cell buffers stay ordinary editable buffers; user on_attach callbacks run for those buffers.
A root_dir wrapper excludes notebook buffers from this named server's automatic activation,
while delegating ordinary files to the original root callback or root-marker configuration.

All code-cell buffers are materialized while enabled so navigation and workspace edits can
reach offscreen cells without reading fictitious files. Buffer I/O stays intercepted by the
companion. Completion consumers apply additional edits through Neovim's public
`vim.lsp.util.apply_text_edits`: a scoped wrapper routes tagged notebook edits to their cell
buffers and delegates ordinary edits unchanged. Cross-cell completion edits validate their
destination source before applying. Consumers that implement their own text-edit application
rather than this Neovim utility are outside the supported completion integration.

Requests capture source and mapping identity. Results arriving after source, order, membership,
name or buffer replacement changes fail with ContentModified. Deferred completion resolve
permits the exact accepted completion edit in its originating cell, since Neovim resolves
from CompleteDone after inserting the selected word; unrelated source changes still fail.
Diagnostics clear when the
projection changes. Cell insertion, deletion, movement and kind changes refresh the projection;
changing away from Python stops the notebook client and switching back starts it again.
Detached cells reattach through the LspDetach lifecycle hook. Neovim's `:lsp restart` retains the
transport and configured callbacks. Asynchronous root discovery captures a generation and
validates the first cell/buffer before starting; callbacks for replaced notebooks/cells expire.

Snapshot construction scans Neovim buffers once. URI lookups are constant time, projected-line
lookups use binary search, and on_lines refreshes coalesce into one scheduled callback.
Harness counters at the Rust source-reconstruction boundary and Lua RPC interception verify
that repeated placements send/rebuild zero document sources with LSP both disabled and enabled;
late enablement and structural changes still send their initial/replacement context.

## Validation

`crates/nbv-nvim/tests/language_server.rs` launches real Pyright through the embedded Neovim
harness. Set `NBV_LSP_SERVER` to an installed `pyright-langserver`; CI provisions the pinned
version and refuses to skip this test. Harness XDG directories remain isolated per Neovim.

The test verifies cross-cell context, missing-name diagnostics, navigation, references,
prepare rename and applied rename/workspace edits after emoji and non-ASCII identifiers,
completion and a resolved auto-import from cell 2 into cell 1, unsaved source changes,
ordinary-file attachment, native popup acceptance with deferred cross-cell auto-import,
external-file Locations/LocationLinks, FileType options in actual cell windows, required-root
activation policy, detach/restart, insertion/deletion/reorder/kind changes, and kernel
language switches. It also injects stale diagnostic versions and completion default edit
ranges at the RPC boundary for deterministic protocol regressions. Pyright's cached eager
import edits are deferred to real completionItem/resolve in the native-acceptance regression,
so it always exercises the post-insertion resolve path. A separate deferred-root
regression ensures expired callbacks cannot start a server for replacement cells.

Pyright natively uses UTF-16. The same real-server scenario also runs through a test-only
UTF-8-to-UTF-16 protocol bridge (`tests/pyright_utf8.lua`) so Neovim negotiates and applies
UTF-8 positions against real Pyright results. This does not claim native Pyright UTF-8 support.

Primary API references: [Neovim LSP](https://neovim.io/doc/user/lsp.html),
[LSP 3.17 specification](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/).
