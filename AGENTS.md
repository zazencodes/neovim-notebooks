# AGENTS.md

Neovim Notebooks (`nvb`): a Rust/Ratatui TUI that edits Jupyter notebooks with an embedded Neovim. The project is changing rapidly; treat `.agents/docs/specs/` and the code as the source of truth over this file.

## Project Instructions

- After a big code change, rebuild and reinstall the binary so the user can test it: `cargo install --path crates/nbv-cli --force`.

## Repo Shape

- `crates/nbv-core`: notebook model, persistence, kernel. Must not depend on Neovim or Ratatui (`scripts/check-core-deps.sh`).
- `crates/nbv-nvim`: embedded Neovim client and cell buffer adapter.
- `crates/nbv-tui`: event loop, layout, drawing.
- `crates/nbv-cli`: the `nvb` binary.
- `lua/nbv/init.lua`: Lua companion loaded into the embedded Neovim.
- `.agents/docs/specs/`: design specs, local and git-ignored. Code comments cite their sections (`§7.6`): the v1 architecture spec, as amended by the cell-editors spec.

## Validation

- `cargo fmt --all`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace`.
- Neovim, kernel and tmux tests need `.tools/nvim-macos-arm64` and a `.venv` with `ipykernel` and `ruff` (see README); without them they skip.
- `cargo` lives in `~/.cargo/bin`, which may not be on PATH in non-login shells.

## Changelog

- `CHANGELOG.md` records user-facing changes: keys, commands, behaviour, requirements, install. Skip internal refactors, tests and spec-only edits.
- Every commit with a user-facing change adds its entry under `## Unreleased` in that commit, grouped under `### Added`, `### Changed`, `### Fixed` or `### Removed`. Write for users, not as a commit list.

## Releases

The agent does the whole release. A release is a `vX.Y.Z` tag on `main`; pushing it runs `.github/workflows/release.yml`, which checks the tag against `Cargo.toml` and the README install command, then creates the GitHub release with the tag's `CHANGELOG.md` section (`scripts/release-notes.sh`) as its notes. Never create a release by hand.

1. Start from a clean, up-to-date `main` with green CI, and run the full validation above.
2. Pick the version from `## Unreleased` (SemVer; before 1.0 a breaking change bumps the minor, anything else the patch).
3. In one commit, `Release vX.Y.Z`:
   - rename `## Unreleased` to `## X.Y.Z - YYYY-MM-DD` and add a new empty `## Unreleased` above it;
   - set `version` in the root `Cargo.toml` and run `cargo check` so `Cargo.lock` follows;
   - change the README install command to `--tag vX.Y.Z`.
4. Check the notes with `scripts/release-notes.sh X.Y.Z`.
5. `git tag -a vX.Y.Z -m vX.Y.Z`, then `git push origin main vX.Y.Z`.
6. Watch the workflow (`gh run watch`), then check with `gh release view vX.Y.Z` that the release exists, is not a draft or prerelease, and carries the changelog section.

If the workflow fails, fix the cause on `main`, delete the tag locally and on `origin`, and tag again. Fix notes on a published release in `CHANGELOG.md` and with `gh release edit vX.Y.Z --notes-file`, keeping both the same.
