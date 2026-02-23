use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

pub struct Key;

impl Key {
    pub const ENTER: &'static str = "enter";
    pub const ESCAPE: &'static str = "escape";
    pub const TAB: &'static str = "tab";
    pub const BACKSPACE: &'static str = "backspace";
    pub const DELETE: &'static str = "delete";
    pub const LEFT: &'static str = "left";
    pub const RIGHT: &'static str = "right";
    pub const UP: &'static str = "up";
    pub const DOWN: &'static str = "down";
    pub const HOME: &'static str = "home";
    pub const END: &'static str = "end";

    pub fn ctrl(ch: char) -> String {
        format!("ctrl+{}", ch.to_ascii_lowercase())
    }
}

pub fn is_key_release(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Release
}

pub fn is_key_repeat(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Repeat
}

pub fn parse_key_event(event: &KeyEvent) -> Option<String> {
    let key_name = match event.code {
        KeyCode::Backspace => Key::BACKSPACE.to_string(),
        KeyCode::Enter => Key::ENTER.to_string(),
        KeyCode::Left => Key::LEFT.to_string(),
        KeyCode::Right => Key::RIGHT.to_string(),
        KeyCode::Up => Key::UP.to_string(),
        KeyCode::Down => Key::DOWN.to_string(),
        KeyCode::Home => Key::HOME.to_string(),
        KeyCode::End => Key::END.to_string(),
        KeyCode::Tab => Key::TAB.to_string(),
        KeyCode::Delete => Key::DELETE.to_string(),
        KeyCode::Esc => Key::ESCAPE.to_string(),
        KeyCode::Char(ch) => ch.to_ascii_lowercase().to_string(),
        _ => return None,
    };

    let mut modifiers = Vec::new();
    if event.modifiers.contains(KeyModifiers::CONTROL) {
        modifiers.push("ctrl");
    }
    if event.modifiers.contains(KeyModifiers::SHIFT) {
        modifiers.push("shift");
    }
    if event.modifiers.contains(KeyModifiers::ALT) {
        modifiers.push("alt");
    }

    if modifiers.is_empty() {
        Some(key_name)
    } else {
        Some(format!("{}+{}", modifiers.join("+"), key_name))
    }
}

pub fn matches_key(event: &KeyEvent, key_id: &str) -> bool {
    parse_key_event(event).is_some_and(|parsed| parsed == key_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ctrl_keys() {
        let event = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(parse_key_event(&event), Some("ctrl+c".to_string()));
    }

    #[test]
    fn matches_arrow_key() {
        let event = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        assert!(matches_key(&event, Key::UP));
    }

    #[test]
    fn detects_release_and_repeat_events() {
        let repeat = KeyEvent {
            code: KeyCode::Char('a'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Repeat,
            state: crossterm::event::KeyEventState::NONE,
        };
        let release = KeyEvent {
            code: KeyCode::Char('a'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: crossterm::event::KeyEventState::NONE,
        };
        assert!(is_key_repeat(&repeat));
        assert!(is_key_release(&release));
    }
}
