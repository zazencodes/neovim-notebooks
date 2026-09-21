# AGENTS.md

Neovim Notebooks (`nvb`): a Rust/Ratatui TUI that edits Jupyter notebooks with an embedded Neovim. The project is changing rapidly; treat `docs/specs/` and the code as the source of truth over this file.

## Project Instructions

- After a big code change, rebuild and reinstall the binary so the user can test it: `cargo install --path crates/nbv-cli --force`.

## Repo Shape

- `crates/nbv-core`: notebook model, persistence, kernel. Must not depend on Neovim or Ratatui (`scripts/check-core-deps.sh`).
- `crates/nbv-nvim`: embedded Neovim client and cell buffer adapter.
- `crates/nbv-tui`: event loop, layout, drawing.
- `crates/nbv-cli`: the `nvb` binary.
- `lua/nbv/init.lua`: Lua companion loaded into the embedded Neovim.
- `docs/specs/`: design specs.

## Validation

- `cargo fmt --all`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace`.
- Neovim, kernel and tmux tests need `.tools/nvim-macos-arm64` and a `.venv` with `ipykernel` and `ruff` (see README); without them they skip.
- `cargo` lives in `~/.cargo/bin`, which may not be on PATH in non-login shells.
