//! Terminal input → Neovim input (§10.2).

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

fn mods(m: KeyModifiers, with_shift: bool) -> String {
    let mut s = String::new();
    if m.contains(KeyModifiers::CONTROL) {
        s.push_str("C-");
    }
    if with_shift && m.contains(KeyModifiers::SHIFT) {
        s.push_str("S-");
    }
    if m.contains(KeyModifiers::ALT) {
        s.push_str("M-");
    }
    if m.contains(KeyModifiers::SUPER) {
        s.push_str("D-");
    }
    s
}

/// Encodes a key press in Neovim's `<>` notation, or `None` for releases and unknown keys.
pub fn encode(key: KeyEvent) -> Option<String> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let named = |name: &str| {
        let m = mods(key.modifiers, true);
        format!("<{m}{name}>")
    };
    Some(match key.code {
        KeyCode::Char(c) => {
            // Shift is already applied to the character itself.
            let m = mods(key.modifiers, false);
            let name = match c {
                '<' => "lt".to_string(),
                '\\' if !m.is_empty() => "Bslash".to_string(),
                '|' if !m.is_empty() => "Bar".to_string(),
                ' ' if !m.is_empty() => "Space".to_string(),
                c => c.to_string(),
            };
            if m.is_empty() && name.chars().count() == 1 { name } else { format!("<{m}{name}>") }
        }
        KeyCode::Enter => named("CR"),
        KeyCode::Tab => named("Tab"),
        KeyCode::BackTab => {
            let m = mods(key.modifiers - KeyModifiers::SHIFT, true);
            format!("<{m}S-Tab>")
        }
        KeyCode::Backspace => named("BS"),
        KeyCode::Esc => named("Esc"),
        KeyCode::Left => named("Left"),
        KeyCode::Right => named("Right"),
        KeyCode::Up => named("Up"),
        KeyCode::Down => named("Down"),
        KeyCode::Home => named("Home"),
        KeyCode::End => named("End"),
        KeyCode::PageUp => named("PageUp"),
        KeyCode::PageDown => named("PageDown"),
        KeyCode::Insert => named("Insert"),
        KeyCode::Delete => named("Del"),
        KeyCode::F(n) => named(&format!("F{n}")),
        _ => return None,
    })
}

/// A mouse event as `nvim_input_mouse` arguments: (button, action, modifier, row, col).
pub fn mouse(ev: MouseEvent) -> Option<(&'static str, &'static str, String, usize, usize)> {
    let button = |b: MouseButton| match b {
        MouseButton::Left => "left",
        MouseButton::Right => "right",
        MouseButton::Middle => "middle",
    };
    let (b, action) = match ev.kind {
        MouseEventKind::Down(b) => (button(b), "press"),
        MouseEventKind::Up(b) => (button(b), "release"),
        MouseEventKind::Drag(b) => (button(b), "drag"),
        MouseEventKind::Moved => ("move", ""),
        MouseEventKind::ScrollUp => ("wheel", "up"),
        MouseEventKind::ScrollDown => ("wheel", "down"),
        MouseEventKind::ScrollLeft => ("wheel", "left"),
        MouseEventKind::ScrollRight => ("wheel", "right"),
    };
    let m = mods(ev.modifiers, true);
    Some((b, action, m.trim_end_matches('-').replace('-', ""), ev.row as usize, ev.column as usize))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(code: KeyCode, m: KeyModifiers) -> Option<String> {
        encode(KeyEvent::new(code, m))
    }

    #[test]
    fn notation() {
        assert_eq!(k(KeyCode::Char('a'), KeyModifiers::NONE).as_deref(), Some("a"));
        assert_eq!(k(KeyCode::Char('A'), KeyModifiers::SHIFT).as_deref(), Some("A"));
        assert_eq!(k(KeyCode::Char('<'), KeyModifiers::NONE).as_deref(), Some("<lt>"));
        assert_eq!(k(KeyCode::Char('w'), KeyModifiers::CONTROL).as_deref(), Some("<C-w>"));
        assert_eq!(k(KeyCode::Enter, KeyModifiers::SHIFT).as_deref(), Some("<S-CR>"));
        assert_eq!(k(KeyCode::Enter, KeyModifiers::CONTROL).as_deref(), Some("<C-CR>"));
        assert_eq!(k(KeyCode::Char(' '), KeyModifiers::CONTROL).as_deref(), Some("<C-Space>"));
        assert_eq!(k(KeyCode::BackTab, KeyModifiers::SHIFT).as_deref(), Some("<S-Tab>"));
        assert_eq!(k(KeyCode::F(5), KeyModifiers::ALT).as_deref(), Some("<M-F5>"));
    }
}
