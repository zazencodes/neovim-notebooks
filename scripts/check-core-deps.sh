#!/usr/bin/env bash
# R5 (§5.2): nbv-core depends on neither Neovim nor Ratatui.
set -euo pipefail
tree=$(cargo tree -p nbv-core -e normal --prefix none --no-dedupe)
forbidden='^(nbv-nvim|nbv-tui|nvim-rs|ratatui|ratatui-image|ratatui-core|crossterm) '
if grep -E "$forbidden" <<<"$tree"; then
  echo "nbv-core must not depend on the crates above (R5)" >&2
  exit 1
fi
echo "nbv-core dependency rule holds"
