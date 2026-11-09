#!/usr/bin/env bash
# Compare a pristine baseline snapshot and this worktree with the same release workload.
# Both builds use the supplied CARGO_TARGET_DIR. No checkout or worktree is changed.
set -euo pipefail
root=$(git rev-parse --show-toplevel)
baseline=${1:-68a896b}
snapshot=$(mktemp -d "${TMPDIR:-/tmp}/nvb-stream-bench.XXXXXX")
trap 'rm -rf "$snapshot"' EXIT
export CARGO_BUILD_JOBS=2
: "${CARGO_TARGET_DIR:?Set CARGO_TARGET_DIR to your isolated build cache}"
export CARGO_TARGET_DIR
git -C "$root" archive "$baseline" | tar -x -C "$snapshot"
mkdir -p "$snapshot/crates/nbv-tui/examples"
cp "$root/crates/nbv-tui/examples/stream-scaling.rs" "$snapshot/crates/nbv-tui/examples/"
# The baseline has no persistent text-cache API; its adapter runs the original build.
cat >> "$snapshot/crates/nbv-tui/src/outputs.rs" <<'RUST'

#[derive(Default)]
pub struct TextCache { _private: () }
pub fn build_cached(cell: &Cell, width: u16, font: FontSize, images: &mut Images, _cache: &mut TextCache) -> OutputView {
    build(cell, width, font, images)
}
RUST
# Path-dependent workspace artifacts embed source and harness paths. Rebuild all
# workspace crates at each boundary while retaining third-party dependency artifacts.
clean_workspace() {
    CARGO_TARGET_DIR="$CARGO_TARGET_DIR" CARGO_BUILD_JOBS=2 cargo clean --release --manifest-path "$1" \
        -p nbv-core -p nbv-nvim -p nbv-tui -p nvb
}
printf 'baseline %s\n' "$baseline"
clean_workspace "$snapshot/Cargo.toml"
CARGO_TARGET_DIR="$CARGO_TARGET_DIR" CARGO_BUILD_JOBS=2 cargo run --quiet --release --manifest-path "$snapshot/Cargo.toml" -p nbv-tui --example stream-scaling
printf 'working tree\n'
clean_workspace "$root/Cargo.toml"
CARGO_TARGET_DIR="$CARGO_TARGET_DIR" CARGO_BUILD_JOBS=2 cargo run --quiet --release --manifest-path "$root/Cargo.toml" -p nbv-tui --example stream-scaling
