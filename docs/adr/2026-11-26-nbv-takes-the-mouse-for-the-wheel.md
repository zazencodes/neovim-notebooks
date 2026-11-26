# nbv takes the mouse for wheel scrolling

## Decision

nbv asks the terminal for mouse input so the wheel scrolls the notebook. Selecting and copying
text with the mouse goes through the terminal's bypass modifier (Shift in most terminals, Option
in iTerm2), and the README says so plainly.

## Why

Scrolling the notebook with the wheel is a must. Terminals cannot report the wheel without also
reporting clicks and drags, so nbv cannot have the wheel and leave plain drags to the terminal.
Every common terminal already offers a modifier that bypasses mouse reporting.

## Rejected

- **Leaving the mouse to the terminal.** A plain drag would select, but the wheel would stop
  scrolling the notebook: tmux drops wheel events for a full-screen program that has not asked
  for the mouse.
- **Selection implemented inside nbv.** A second copy mechanism beside the terminal's and
  Neovim's, for no gain.
