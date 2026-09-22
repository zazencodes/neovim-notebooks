//! Navigation mode (§10.4): Vim-style keys that act on whole cells, while no cell is being
//! edited. Keys arrive in Neovim's `<>` notation; counts and two-key chords work as in Vim.

use nbv_core::CellKind;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Down(usize),
    Up(usize),
    First,
    /// `G`, or `{count}G` for the count-th cell.
    Last(Option<usize>),
    HalfPageDown,
    HalfPageUp,
    /// Scroll the selected cell to the middle of the view.
    Center,
    /// Edit the selected cell.
    Edit,
    /// A new code cell below (or above), edited in insert mode.
    Open {
        above: bool,
    },
    Delete,
    Yank,
    Paste {
        above: bool,
    },
    Undo,
    Redo,
    MoveDown,
    MoveUp,
    /// Join the next cell onto this one.
    Merge,
    Kind(CellKind),
    /// Run the cell and stay on it.
    Run,
    /// Run the cell and select the next (past the end, add one and edit it).
    RunAdvance,
    Interrupt,
    Restart,
    /// Neovim's command line.
    Cmdline,
    /// The previous / next item in the header.
    Left,
    Right,
    /// The key list.
    Help,
}

/// Heads the key list when tmux passes on no modified keys.
pub const TMUX_SETUP: (&str, &[(&str, &str)]) = (
    "tmux: Shift+Enter and Ctrl+Enter are off",
    &[
        ("", "Add to tmux.conf, then restart tmux:"),
        ("", "set -s extended-keys on"),
        ("", "set -s extended-keys-format csi-u"),
        ("", "set -as terminal-features 'xterm*:extkeys'"),
    ],
);

/// The key list `?` shows, in sections.
pub const HELP: &[(&str, &[(&str, &str)])] = &[
    (
        "Cells (NAV)",
        &[
            ("j k", "next / previous cell; counts work (3j)"),
            ("gg G {n}G", "first / last / nth cell"),
            ("<C-d> <C-u>", "scroll half a page, selecting the middle cell"),
            ("zz", "center the cell on screen"),
            ("<CR>", "edit the cell"),
            ("o O", "new code cell below / above"),
            ("dd yy", "delete / yank the cell"),
            ("p P", "paste below / above"),
            ("u <C-r>", "undo / redo a cell change"),
            ("J", "join the next cell onto this one"),
            ("]e [e", "move the cell down / up"),
            ("tc tm tr", "make it code / markdown / raw"),
            ("x <S-CR>", "run and select the next cell"),
            ("r <C-CR>", "run and stay on the cell"),
            ("ii 00", "interrupt / restart the kernel"),
            (":", "Neovim's command line"),
            ("?", "this list"),
        ],
    ),
    (
        "Header (k on the first cell)",
        &[("h l", "file name / kernel"), ("<CR>", "rename the file / pick the kernel"), ("j", "back to the cells")],
    ),
    (
        "Editing a cell (EDIT)",
        &[
            ("<Esc>", "leave the cell, from Normal mode (from Insert, <Esc><Esc>)"),
            ("<S-CR>", "run the cell and go to the next"),
            ("<C-CR>", "run the cell and keep editing"),
            ("", "everything else is your own Neovim"),
        ],
    ),
    (
        "Commands",
        &[
            (":w :wq :q!", "save / save and quit / quit without saving"),
            (":NvbRunAll", "run every code cell"),
            (":NvbRunAbove", "run the code cells above this one"),
            (":NvbSplit", "split the edited cell at the cursor"),
            (":NvbClearOutput[!]", "clear this cell's outputs (! for every cell)"),
        ],
    ),
];

/// Keys that start a two-key chord.
const PREFIXES: [&str; 9] = ["g", "d", "y", "t", "i", "0", "]", "[", "z"];

#[derive(Debug, Default)]
pub struct Nav {
    count: Option<usize>,
    pending: Option<&'static str>,
}

impl Nav {
    /// What has been typed toward the next action, for display (like Vim's 'showcmd').
    pub fn partial(&self) -> String {
        format!("{}{}", self.count.map(|n| n.to_string()).unwrap_or_default(), self.pending.unwrap_or(""))
    }

    /// Feeds one key; returns an action once one is complete. Every action has exactly one
    /// key, and none needs a modifier the terminal might not report.
    pub fn feed(&mut self, key: &str) -> Option<Action> {
        if self.pending.is_none()
            && let Some(d) = key.chars().next().filter(|c| c.is_ascii_digit() && key.len() == 1)
            && (d != '0' || self.count.is_some())
        {
            let d = d.to_digit(10).unwrap() as usize;
            self.count = Some(self.count.unwrap_or(0).saturating_mul(10).saturating_add(d));
            return None;
        }
        let count = self.count.take();
        let n = count.unwrap_or(1);
        let pending = self.pending.take();
        use Action::*;
        Some(match (pending, key) {
            (None, "j") => Down(n),
            (None, "k") => Up(n),
            (Some("g"), "g") => First,
            (None, "G") => Last(count),
            (None, "<C-d>") => HalfPageDown,
            (None, "<C-u>") => HalfPageUp,
            (Some("z"), "z") => Center,
            (None, "<CR>") => Edit,
            (None, "o") => Open { above: false },
            (None, "O") => Open { above: true },
            (Some("d"), "d") => Delete,
            (Some("y"), "y") => Yank,
            (None, "p") => Paste { above: false },
            (None, "P") => Paste { above: true },
            (None, "u") => Undo,
            (None, "<C-r>") => Redo,
            (Some("]"), "e") => MoveDown,
            (Some("["), "e") => MoveUp,
            (None, "J") => Merge,
            (Some("t"), "c") => Kind(CellKind::Code),
            (Some("t"), "m") => Kind(CellKind::Markdown),
            (Some("t"), "r") => Kind(CellKind::Raw),
            (None, "x" | "<S-CR>") => RunAdvance,
            (None, "r" | "<C-CR>") => Run,
            (Some("i"), "i") => Interrupt,
            (Some("0"), "0") => Restart,
            (None, ":") => Cmdline,
            (None, "h") => Left,
            (None, "l") => Right,
            (None, "?") => Help,
            (None, first) if let Some(p) = PREFIXES.iter().find(|p| **p == first) => {
                self.pending = Some(p);
                self.count = count;
                return None;
            }
            // <Esc> and anything unbound cancel what was typed.
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(keys: &[&str]) -> Vec<Action> {
        let mut nav = Nav::default();
        keys.iter().filter_map(|k| nav.feed(k)).collect()
    }

    #[test]
    fn motions_take_counts() {
        assert_eq!(feed(&["j", "3", "k", "1", "0", "j"]), [Action::Down(1), Action::Up(3), Action::Down(10)]);
        assert_eq!(feed(&["g", "g", "G", "2", "G"]), [Action::First, Action::Last(None), Action::Last(Some(2))]);
        assert_eq!(feed(&["h", "l", "?"]), [Action::Left, Action::Right, Action::Help]);
        assert_eq!(feed(&["<S-CR>", "<C-CR>"]), [Action::RunAdvance, Action::Run]);
    }

    #[test]
    fn chords() {
        assert_eq!(
            feed(&["d", "d", "y", "y", "t", "m", "i", "i", "0", "0", "]", "e", "[", "e", "z", "z"]),
            [
                Action::Delete,
                Action::Yank,
                Action::Kind(CellKind::Markdown),
                Action::Interrupt,
                Action::Restart,
                Action::MoveDown,
                Action::MoveUp,
                Action::Center,
            ]
        );
    }

    #[test]
    fn escape_and_unbound_keys_cancel() {
        let mut nav = Nav::default();
        assert_eq!(nav.feed("2"), None);
        assert_eq!(nav.feed("d"), None);
        assert_eq!(nav.partial(), "2d");
        assert_eq!(nav.feed("<Esc>"), None);
        assert_eq!(nav.partial(), "");
        assert_eq!(nav.feed("d"), None);
        assert_eq!(nav.feed("q"), None);
        assert_eq!(nav.feed("<Down>"), None, "one key per action: arrows are unbound");
        assert_eq!(nav.feed("j"), Some(Action::Down(1)), "the chord was abandoned");
    }
}
