//! Terminal identification, image protocol selection (§12) and tmux policy (§12.2).

use std::io::Write;
use std::process::Command;

use crossterm::event::{KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags};

use ratatui_image::picker::{Picker, ProtocolType};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Term {
    Kitty,
    Ghostty,
    WezTerm,
    Other(String),
}

/// What nbv knows about the tmux server it runs under.
#[derive(Clone, Debug, Default)]
pub struct Tmux {
    pub passthrough: bool,
    /// tmux itself parses and redraws Sixel (3.4+ built with Sixel support).
    pub sixel: bool,
    pub outer: Option<Term>,
    /// tmux gets modified keys (`<S-CR>`) from the outer terminal and passes them on in the
    /// CSI u form nbv reads (§10.2).
    pub extended_keys: bool,
}

fn classify(name: &str) -> Term {
    let n = name.to_ascii_lowercase();
    if n.contains("kitty") {
        Term::Kitty
    } else if n.contains("ghostty") {
        Term::Ghostty
    } else if n.contains("wezterm") {
        Term::WezTerm
    } else {
        Term::Other(name.to_string())
    }
}

/// The terminal nbv draws to directly, from the environment.
pub fn direct_terminal() -> Term {
    let var = |k: &str| std::env::var(k).unwrap_or_default();
    if !var("KITTY_WINDOW_ID").is_empty() || var("TERM") == "xterm-kitty" {
        return Term::Kitty;
    }
    if !var("GHOSTTY_RESOURCES_DIR").is_empty() || var("TERM_PROGRAM") == "ghostty" {
        return Term::Ghostty;
    }
    if !var("WEZTERM_EXECUTABLE").is_empty() || var("TERM_PROGRAM") == "WezTerm" {
        return Term::WezTerm;
    }
    classify(&var("TERM_PROGRAM"))
}

pub fn in_tmux() -> bool {
    std::env::var_os("TMUX").is_some()
}

fn tmux(args: &[&str]) -> Option<String> {
    let out = Command::new("tmux").args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Queries the tmux server for what image support depends on, and the outer terminal. The
/// outer terminal is identified through tmux: capability replies inside tmux describe tmux.
pub fn probe_tmux() -> Tmux {
    let opt = |name: &str| tmux(&["show-options", "-gv", name]).unwrap_or_default();
    let server = |name: &str| tmux(&["show-options", "-sv", name]).unwrap_or_default();
    let client = tmux(&["display-message", "-p", "#{client_termname}\t#{client_termtype}\t#{client_termfeatures}"])
        .unwrap_or_default();
    let mut fields = client.split('\t');
    let termname = fields.next().unwrap_or_default().to_string();
    let termtype = fields.next().unwrap_or_default().to_string();
    let features = fields.next().unwrap_or_default().to_string();
    let outer = match classify(&termtype) {
        Term::Other(_) => classify(&termname),
        t => t,
    };
    Tmux {
        passthrough: matches!(opt("allow-passthrough").as_str(), "on" | "all"),
        sixel: features.split(',').any(|f| f == "sixel"),
        outer: Some(outer),
        extended_keys: matches!(server("extended-keys").as_str(), "on" | "always")
            && server("extended-keys-format") == "csi-u"
            // Without it, tmux never asks the outer terminal to report modifiers.
            && features.split(',').any(|f| f == "extkeys"),
    }
}

/// Asks the terminal to report modified keys (`<S-CR>`, `<C-CR>`) distinctly from plain ones
/// (§10.2). Directly, through the Kitty keyboard protocol; inside tmux, which ignores that
/// protocol, through modifyOtherKeys, which tmux answers in CSI u form when configured to.
/// Terminals with neither send the plain key.
pub fn report_modified_keys(tmux: bool) -> std::io::Result<()> {
    let mut out = std::io::stdout();
    if tmux {
        out.write_all(b"\x1b[>4;1m")?;
    } else {
        crossterm::queue!(out, PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES))?;
    }
    out.flush()
}

/// Undoes `report_modified_keys`.
pub fn stop_reporting_modified_keys(tmux: bool) {
    let mut out = std::io::stdout();
    let _ = if tmux { out.write_all(b"\x1b[>4m") } else { crossterm::queue!(out, PopKeyboardEnhancementFlags) };
    let _ = out.flush();
}

/// The per-terminal override table (§12), applied over capability detection.
///
/// Directly: Kitty and Ghostty use Kitty; WezTerm uses iTerm2; others use what was detected,
/// restricted to Sixel or halfblocks. Inside tmux only mechanisms tmux understands, or that
/// reach the screen as text, are used: Kitty Unicode placeholders (with passthrough), tmux's
/// own Sixel, or halfblocks. Pixels painted behind tmux's back (iTerm2) are never used.
pub fn choose_protocol(detected: ProtocolType, direct: &Term, tmux: Option<&Tmux>) -> ProtocolType {
    let sixel_or_blocks = |ok: bool| if ok { ProtocolType::Sixel } else { ProtocolType::Halfblocks };
    match tmux {
        None => match direct {
            Term::Kitty | Term::Ghostty => ProtocolType::Kitty,
            Term::WezTerm => ProtocolType::Iterm2,
            Term::Other(_) => sixel_or_blocks(detected == ProtocolType::Sixel),
        },
        Some(t) => match t.outer.as_ref() {
            Some(Term::Kitty | Term::Ghostty) if t.passthrough => ProtocolType::Kitty,
            _ => sixel_or_blocks(t.sixel),
        },
    }
}

/// Builds the picker: capability query (cell size, Sixel), then the override table. The
/// terminal must be in raw mode, and nothing else may be reading stdin yet.
pub fn picker(tmux: Option<&Tmux>) -> Picker {
    let replies = crate::query::query(std::time::Duration::from_millis(500));
    let cell = replies.cell.or_else(|| {
        // Fall back to the pixel size the tty reports.
        let ws = crossterm::terminal::window_size().ok()?;
        (ws.width > 0 && ws.columns > 0 && ws.rows > 0).then(|| (ws.width / ws.columns, ws.height / ws.rows))
    });
    let detected = if replies.sixel { ProtocolType::Sixel } else { ProtocolType::Halfblocks };
    #[allow(deprecated)] // the alternative queries stdin from a thread that can outlive it
    let mut picker = match cell {
        Some((w, h)) => Picker::from_fontsize(ratatui_image::FontSize::new(w, h)),
        None => Picker::halfblocks(),
    };
    // Inside tmux, DA1 is tmux's own answer: Sixel there means tmux was built with it, and
    // tmux-native Sixel also needs an outer terminal that draws Sixel.
    let tmux = tmux.map(|t| Tmux { sixel: t.sixel && replies.sixel, ..t.clone() });
    picker.set_protocol_type(choose_protocol(detected, &direct_terminal(), tmux.as_ref()));
    picker
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_table() {
        let d = ProtocolType::Halfblocks;
        assert_eq!(choose_protocol(d, &Term::Kitty, None), ProtocolType::Kitty);
        assert_eq!(choose_protocol(d, &Term::Ghostty, None), ProtocolType::Kitty);
        assert_eq!(choose_protocol(ProtocolType::Kitty, &Term::WezTerm, None), ProtocolType::Iterm2);
        assert_eq!(choose_protocol(ProtocolType::Sixel, &Term::Other("foot".into()), None), ProtocolType::Sixel);
        assert_eq!(choose_protocol(ProtocolType::Kitty, &Term::Other("x".into()), None), ProtocolType::Halfblocks);
    }

    #[test]
    fn tmux_never_uses_iterm2_and_needs_passthrough_for_kitty() {
        let d = ProtocolType::Kitty;
        let mut t = Tmux { outer: Some(Term::Ghostty), passthrough: true, ..Default::default() };
        assert_eq!(choose_protocol(d, &Term::Other("tmux".into()), Some(&t)), ProtocolType::Kitty);
        t.passthrough = false;
        assert_eq!(choose_protocol(d, &Term::Other("tmux".into()), Some(&t)), ProtocolType::Halfblocks);
        let w = Tmux { outer: Some(Term::WezTerm), passthrough: true, sixel: true, ..Default::default() };
        assert_eq!(choose_protocol(d, &Term::WezTerm, Some(&w)), ProtocolType::Sixel);
        let w = Tmux { outer: Some(Term::WezTerm), passthrough: true, ..Default::default() };
        assert_eq!(choose_protocol(d, &Term::WezTerm, Some(&w)), ProtocolType::Halfblocks);
    }
}
