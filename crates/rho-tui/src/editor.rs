use std::cmp::min;

#[derive(Debug, Clone)]
struct EditorSnapshot {
    lines: Vec<String>,
    cursor_line: usize,
    cursor_col: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LastAction {
    Kill,
    Yank,
    Other,
}

#[derive(Debug, Clone)]
pub struct EditorRender {
    pub lines: Vec<String>,
    pub cursor_row: usize,
    pub cursor_col: usize,
}

#[derive(Debug, Clone)]
pub struct EditorSubmit {
    pub raw: String,
    pub expanded: String,
}

#[derive(Debug, Clone)]
pub struct EditorState {
    lines: Vec<String>,
    cursor_line: usize,
    cursor_col: usize,
    preferred_visual_col: Option<usize>,
    history: Vec<String>,
    history_index: Option<usize>,
    history_snapshot: String,
    undo_stack: Vec<EditorSnapshot>,
    kill_ring: Vec<String>,
    last_action: LastAction,
    last_yank_len: Option<usize>,
    large_pastes: Vec<(String, String)>,
    paste_counter: u32,
}

impl Default for EditorState {
    fn default() -> Self {
        Self::new()
    }
}

impl EditorState {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            cursor_line: 0,
            cursor_col: 0,
            preferred_visual_col: None,
            history: Vec::new(),
            history_index: None,
            history_snapshot: String::new(),
            undo_stack: Vec::new(),
            kill_ring: Vec::new(),
            last_action: LastAction::Other,
            last_yank_len: None,
            large_pastes: Vec::new(),
            paste_counter: 0,
        }
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn set_text(&mut self, text: String) {
        self.lines = normalize_lines(text);
        self.cursor_line = self.lines.len().saturating_sub(1);
        self.cursor_col = char_len(self.lines[self.cursor_line].as_str());
        self.preferred_visual_col = None;
    }

    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.cursor_line = 0;
        self.cursor_col = 0;
        self.preferred_visual_col = None;
        self.history_index = None;
        self.undo_stack.clear();
        self.last_action = LastAction::Other;
        self.last_yank_len = None;
    }

    pub fn insert_char(&mut self, ch: char) {
        self.push_undo_snapshot();
        self.exit_history_mode();
        let line = &mut self.lines[self.cursor_line];
        let byte_index = char_to_byte_index(line.as_str(), self.cursor_col);
        line.insert(byte_index, ch);
        self.cursor_col += 1;
        self.preferred_visual_col = None;
        self.last_action = LastAction::Other;
        self.last_yank_len = None;
    }

    pub fn insert_newline(&mut self) {
        self.push_undo_snapshot();
        self.exit_history_mode();

        let split_at = char_to_byte_index(self.lines[self.cursor_line].as_str(), self.cursor_col);
        let tail = self.lines[self.cursor_line].split_off(split_at);
        self.lines.insert(self.cursor_line + 1, tail);
        self.cursor_line += 1;
        self.cursor_col = 0;
        self.preferred_visual_col = None;
        self.last_action = LastAction::Other;
        self.last_yank_len = None;
    }

    pub fn insert_text(&mut self, text: &str) {
        self.push_undo_snapshot();
        self.exit_history_mode();
        self.insert_text_internal(text);
        self.preferred_visual_col = None;
        self.last_action = LastAction::Other;
        self.last_yank_len = None;
    }

    pub fn handle_paste(&mut self, pasted_text: String) {
        let normalized = pasted_text.replace("\r\n", "\n").replace('\r', "\n");
        let line_count = normalized.split('\n').count();
        let char_count = normalized.chars().count();

        if line_count > 10 || char_count > 1000 {
            self.paste_counter += 1;
            let marker = if line_count > 10 {
                format!("[paste #{} +{} lines]", self.paste_counter, line_count)
            } else {
                format!("[paste #{} {} chars]", self.paste_counter, char_count)
            };
            self.large_pastes.push((marker.clone(), normalized));
            self.insert_text(marker.as_str());
            return;
        }

        self.insert_text(normalized.as_str());
    }

    pub fn move_left(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_line > 0 {
            self.cursor_line -= 1;
            self.cursor_col = char_len(self.lines[self.cursor_line].as_str());
        }
        self.preferred_visual_col = None;
        self.last_action = LastAction::Other;
        self.last_yank_len = None;
    }

    pub fn move_right(&mut self) {
        let line_len = char_len(self.lines[self.cursor_line].as_str());
        if self.cursor_col < line_len {
            self.cursor_col += 1;
        } else if self.cursor_line + 1 < self.lines.len() {
            self.cursor_line += 1;
            self.cursor_col = 0;
        }
        self.preferred_visual_col = None;
        self.last_action = LastAction::Other;
        self.last_yank_len = None;
    }

    pub fn move_up(&mut self) {
        if self.cursor_line == 0 {
            return;
        }

        let target_col = self.preferred_visual_col.unwrap_or(self.cursor_col);
        self.cursor_line -= 1;
        self.cursor_col = min(target_col, char_len(self.lines[self.cursor_line].as_str()));
        self.preferred_visual_col = Some(target_col);
        self.last_action = LastAction::Other;
        self.last_yank_len = None;
    }

    pub fn move_down(&mut self) {
        if self.cursor_line + 1 >= self.lines.len() {
            return;
        }

        let target_col = self.preferred_visual_col.unwrap_or(self.cursor_col);
        self.cursor_line += 1;
        self.cursor_col = min(target_col, char_len(self.lines[self.cursor_line].as_str()));
        self.preferred_visual_col = Some(target_col);
        self.last_action = LastAction::Other;
        self.last_yank_len = None;
    }

    pub fn move_home(&mut self) {
        self.cursor_col = 0;
        self.preferred_visual_col = None;
        self.last_action = LastAction::Other;
        self.last_yank_len = None;
    }

    pub fn move_end(&mut self) {
        self.cursor_col = char_len(self.lines[self.cursor_line].as_str());
        self.preferred_visual_col = None;
        self.last_action = LastAction::Other;
        self.last_yank_len = None;
    }

    pub fn backspace(&mut self) {
        if self.cursor_col == 0 && self.cursor_line == 0 {
            return;
        }

        self.push_undo_snapshot();
        self.exit_history_mode();

        if self.cursor_col > 0 {
            let line = &mut self.lines[self.cursor_line];
            let start = char_to_byte_index(line.as_str(), self.cursor_col - 1);
            let end = char_to_byte_index(line.as_str(), self.cursor_col);
            line.replace_range(start..end, "");
            self.cursor_col -= 1;
        } else {
            let tail = self.lines.remove(self.cursor_line);
            self.cursor_line -= 1;
            self.cursor_col = char_len(self.lines[self.cursor_line].as_str());
            self.lines[self.cursor_line].push_str(tail.as_str());
        }

        self.preferred_visual_col = None;
        self.last_action = LastAction::Other;
        self.last_yank_len = None;
    }

    pub fn delete(&mut self) {
        let line_len = char_len(self.lines[self.cursor_line].as_str());
        if self.cursor_col == line_len && self.cursor_line + 1 >= self.lines.len() {
            return;
        }

        self.push_undo_snapshot();
        self.exit_history_mode();

        if self.cursor_col < line_len {
            let line = &mut self.lines[self.cursor_line];
            let start = char_to_byte_index(line.as_str(), self.cursor_col);
            let end = char_to_byte_index(line.as_str(), self.cursor_col + 1);
            line.replace_range(start..end, "");
        } else {
            let tail = self.lines.remove(self.cursor_line + 1);
            self.lines[self.cursor_line].push_str(tail.as_str());
        }

        self.preferred_visual_col = None;
        self.last_action = LastAction::Other;
        self.last_yank_len = None;
    }

    pub fn delete_word_backward(&mut self) {
        if self.cursor_col == 0 && self.cursor_line == 0 {
            return;
        }

        self.push_undo_snapshot();
        self.exit_history_mode();

        let deleted = if self.cursor_col == 0 {
            let tail = self.lines.remove(self.cursor_line);
            self.cursor_line -= 1;
            self.cursor_col = char_len(self.lines[self.cursor_line].as_str());
            self.lines[self.cursor_line].push_str(tail.as_str());
            "\n".to_string()
        } else {
            let line = &mut self.lines[self.cursor_line];
            let mut chars: Vec<char> = line.chars().collect();
            let mut start = self.cursor_col;
            while start > 0 && chars[start - 1].is_whitespace() {
                start -= 1;
            }
            while start > 0 && !chars[start - 1].is_whitespace() {
                start -= 1;
            }

            let deleted: String = chars[start..self.cursor_col].iter().collect();
            chars.drain(start..self.cursor_col);
            *line = chars.into_iter().collect();
            self.cursor_col = start;
            deleted
        };

        self.push_kill(deleted, true);
        self.preferred_visual_col = None;
    }

    pub fn delete_to_line_start(&mut self) {
        if self.cursor_col == 0 {
            return;
        }

        self.push_undo_snapshot();
        self.exit_history_mode();

        let line = &mut self.lines[self.cursor_line];
        let end = char_to_byte_index(line.as_str(), self.cursor_col);
        let deleted = line[..end].to_string();
        line.replace_range(..end, "");
        self.cursor_col = 0;

        self.push_kill(deleted, true);
        self.preferred_visual_col = None;
    }

    pub fn delete_to_line_end(&mut self) {
        let line_len = char_len(self.lines[self.cursor_line].as_str());
        if self.cursor_col == line_len && self.cursor_line + 1 >= self.lines.len() {
            return;
        }

        self.push_undo_snapshot();
        self.exit_history_mode();

        let deleted = if self.cursor_col < line_len {
            let line = &mut self.lines[self.cursor_line];
            let start = char_to_byte_index(line.as_str(), self.cursor_col);
            let deleted = line[start..].to_string();
            line.replace_range(start.., "");
            deleted
        } else {
            let tail = self.lines.remove(self.cursor_line + 1);
            self.lines[self.cursor_line].push_str(tail.as_str());
            "\n".to_string()
        };

        self.push_kill(deleted, false);
        self.preferred_visual_col = None;
    }

    pub fn undo(&mut self) {
        if let Some(snapshot) = self.undo_stack.pop() {
            self.lines = snapshot.lines;
            self.cursor_line = snapshot.cursor_line;
            self.cursor_col = snapshot.cursor_col;
            self.history_index = None;
            self.preferred_visual_col = None;
            self.last_action = LastAction::Other;
            self.last_yank_len = None;
        }
    }

    pub fn yank(&mut self) {
        let Some(text) = self.kill_ring.last().cloned() else {
            return;
        };

        self.push_undo_snapshot();
        self.exit_history_mode();
        self.insert_text_internal(text.as_str());
        self.last_action = LastAction::Yank;
        self.last_yank_len = Some(text.chars().count());
    }

    pub fn yank_pop(&mut self) {
        if self.last_action != LastAction::Yank || self.kill_ring.len() <= 1 {
            return;
        }
        let Some(last_len) = self.last_yank_len else {
            return;
        };

        self.push_undo_snapshot();

        for _ in 0..last_len {
            self.backspace_without_undo();
        }

        if let Some(last) = self.kill_ring.pop() {
            self.kill_ring.insert(0, last);
        }
        let replacement = self.kill_ring.last().cloned().unwrap_or_default();
        self.insert_text_internal(replacement.as_str());
        self.last_action = LastAction::Yank;
        self.last_yank_len = Some(replacement.chars().count());
    }

    pub fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }

        if self.history_index.is_none() {
            self.history_snapshot = self.text();
            self.history_index = Some(0);
        } else if let Some(index) = self.history_index {
            if index + 1 < self.history.len() {
                self.history_index = Some(index + 1);
            }
        }

        if let Some(index) = self.history_index {
            self.set_text(self.history[index].clone());
        }
    }

    pub fn history_down(&mut self) {
        let Some(index) = self.history_index else {
            return;
        };

        if index == 0 {
            self.history_index = None;
            self.set_text(self.history_snapshot.clone());
        } else {
            self.history_index = Some(index - 1);
            if let Some(next) = self.history.get(index - 1) {
                self.set_text(next.clone());
            }
        }
    }

    pub fn submit(&mut self) -> EditorSubmit {
        let raw = self.text();
        let expanded = self.expand_large_pastes(raw.clone());
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            self.add_to_history(raw.clone());
        }
        self.clear();
        EditorSubmit { raw, expanded }
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn cursor_position(&self) -> (usize, usize) {
        (self.cursor_line, self.cursor_col)
    }

    pub fn replace_prefix_before_cursor(&mut self, prefix_chars: usize, replacement: &str) {
        self.push_undo_snapshot();
        self.exit_history_mode();

        let start_col = self.cursor_col.saturating_sub(prefix_chars);
        let line = &mut self.lines[self.cursor_line];
        let start_byte = char_to_byte_index(line.as_str(), start_col);
        let end_byte = char_to_byte_index(line.as_str(), self.cursor_col);
        line.replace_range(start_byte..end_byte, replacement);
        self.cursor_col = start_col + replacement.chars().count();
        self.preferred_visual_col = None;
        self.last_action = LastAction::Other;
        self.last_yank_len = None;
    }

    pub fn render(&self, width: usize, height: usize) -> EditorRender {
        let width = width.max(1);
        let height = height.max(1);
        let total_lines = self.lines.len();

        let mut top = total_lines.saturating_sub(height);
        if self.cursor_line < top {
            top = self.cursor_line;
        } else if self.cursor_line >= top + height {
            top = self.cursor_line + 1 - height;
        }

        let bottom = min(top + height, total_lines);
        let mut rendered = Vec::new();
        let mut cursor_row = 0;
        let mut cursor_col = 0;

        for line_index in top..bottom {
            let line = &self.lines[line_index];
            let line_len = char_len(line.as_str());
            let mut start_col = 0usize;

            if line_index == self.cursor_line && line_len > width {
                let max_start = line_len.saturating_sub(width);
                start_col = self.cursor_col.saturating_sub(width.saturating_sub(1)).min(max_start);
            }

            let visible: String = line.chars().skip(start_col).take(width).collect();
            rendered.push(visible);

            if line_index == self.cursor_line {
                cursor_row = line_index - top;
                cursor_col = self.cursor_col.saturating_sub(start_col).min(width.saturating_sub(1));
            }
        }

        while rendered.len() < height {
            rendered.push(String::new());
        }

        EditorRender {
            lines: rendered,
            cursor_row,
            cursor_col,
        }
    }

    fn add_to_history(&mut self, entry: String) {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            return;
        }

        if self.history.first().is_some_and(|last| last == trimmed) {
            return;
        }

        self.history.insert(0, trimmed.to_string());
        if self.history.len() > 100 {
            self.history.truncate(100);
        }
    }

    fn push_undo_snapshot(&mut self) {
        let snapshot = EditorSnapshot {
            lines: self.lines.clone(),
            cursor_line: self.cursor_line,
            cursor_col: self.cursor_col,
        };

        if self
            .undo_stack
            .last()
            .is_some_and(|last| last.lines == snapshot.lines && last.cursor_line == snapshot.cursor_line && last.cursor_col == snapshot.cursor_col)
        {
            return;
        }
        self.undo_stack.push(snapshot);
    }

    fn exit_history_mode(&mut self) {
        self.history_index = None;
    }

    fn insert_text_internal(&mut self, text: &str) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        for ch in normalized.chars() {
            if ch == '\n' {
                let split_at = char_to_byte_index(self.lines[self.cursor_line].as_str(), self.cursor_col);
                let tail = self.lines[self.cursor_line].split_off(split_at);
                self.lines.insert(self.cursor_line + 1, tail);
                self.cursor_line += 1;
                self.cursor_col = 0;
            } else {
                let line = &mut self.lines[self.cursor_line];
                let byte_index = char_to_byte_index(line.as_str(), self.cursor_col);
                line.insert(byte_index, ch);
                self.cursor_col += 1;
            }
        }
    }

    fn backspace_without_undo(&mut self) {
        if self.cursor_col == 0 && self.cursor_line == 0 {
            return;
        }

        if self.cursor_col > 0 {
            let line = &mut self.lines[self.cursor_line];
            let start = char_to_byte_index(line.as_str(), self.cursor_col - 1);
            let end = char_to_byte_index(line.as_str(), self.cursor_col);
            line.replace_range(start..end, "");
            self.cursor_col -= 1;
        } else {
            let tail = self.lines.remove(self.cursor_line);
            self.cursor_line -= 1;
            self.cursor_col = char_len(self.lines[self.cursor_line].as_str());
            self.lines[self.cursor_line].push_str(tail.as_str());
        }
    }

    fn push_kill(&mut self, text: String, prepend: bool) {
        if text.is_empty() {
            return;
        }

        if self.last_action == LastAction::Kill {
            if let Some(last) = self.kill_ring.last_mut() {
                if prepend {
                    let mut merged = text;
                    merged.push_str(last);
                    *last = merged;
                } else {
                    last.push_str(text.as_str());
                }
            } else {
                self.kill_ring.push(text);
            }
        } else {
            self.kill_ring.push(text);
        }

        self.last_action = LastAction::Kill;
        self.last_yank_len = None;
    }

    fn expand_large_pastes(&self, mut text: String) -> String {
        for (marker, content) in &self.large_pastes {
            text = text.replace(marker.as_str(), content.as_str());
        }
        text
    }
}

fn normalize_lines(text: String) -> Vec<String> {
    let mut lines: Vec<String> = text
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .split('\n')
        .map(ToOwned::to_owned)
        .collect();
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn char_len(text: &str) -> usize {
    text.chars().count()
}

fn char_to_byte_index(text: &str, char_index: usize) -> usize {
    if char_index == 0 {
        return 0;
    }
    text.char_indices()
        .nth(char_index)
        .map(|(byte, _)| byte)
        .unwrap_or(text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_paste_is_markerized_and_expanded_on_submit() {
        let mut editor = EditorState::new();
        let paste = (0..12).map(|i| format!("line-{i}")).collect::<Vec<_>>().join("\n");
        editor.handle_paste(paste.clone());

        let raw = editor.text();
        assert!(raw.starts_with("[paste #1 +12 lines]"));

        let submit = editor.submit();
        assert_eq!(submit.expanded, paste);
    }

    #[test]
    fn history_navigation_round_trips_snapshot() {
        let mut editor = EditorState::new();
        editor.insert_text("first");
        let _ = editor.submit();
        editor.insert_text("current");

        editor.history_up();
        assert_eq!(editor.text(), "first");
        editor.history_down();
        assert_eq!(editor.text(), "current");
    }

    #[test]
    fn undo_restores_previous_state() {
        let mut editor = EditorState::new();
        editor.insert_text("abc");
        editor.backspace();
        assert_eq!(editor.text(), "ab");
        editor.undo();
        assert_eq!(editor.text(), "abc");
    }
}
