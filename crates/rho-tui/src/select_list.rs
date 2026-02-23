use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::autocomplete::AutocompleteItem;

#[derive(Debug, Clone)]
pub struct SelectList {
    items: Vec<AutocompleteItem>,
    selected: usize,
    max_visible: usize,
}

impl SelectList {
    pub fn new(items: Vec<AutocompleteItem>, max_visible: usize) -> Self {
        Self {
            items,
            selected: 0,
            max_visible: max_visible.max(3),
        }
    }

    pub fn move_up(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.items.len() - 1
        } else {
            self.selected - 1
        };
    }

    pub fn move_down(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected = if self.selected + 1 >= self.items.len() {
            0
        } else {
            self.selected + 1
        };
    }

    pub fn selected_item(&self) -> Option<&AutocompleteItem> {
        self.items.get(self.selected)
    }

    pub fn render_lines(&self, width: usize) -> Vec<Line<'static>> {
        if self.items.is_empty() {
            return vec![Line::from(vec![Span::styled(
                "  no suggestions",
                Style::default().fg(Color::DarkGray),
            )])];
        }

        let start = self
            .selected
            .saturating_sub(self.max_visible.saturating_sub(1) / 2)
            .min(self.items.len().saturating_sub(self.max_visible));
        let end = (start + self.max_visible).min(self.items.len());

        let mut lines = Vec::new();
        for index in start..end {
            let Some(item) = self.items.get(index) else {
                continue;
            };
            let selected = index == self.selected;
            let prefix = if selected { "→ " } else { "  " };
            let style = if selected {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };

            let mut line = format!("{prefix}{}", item.label);
            if let Some(description) = &item.description {
                line.push_str("  ");
                line.push_str(description.as_str());
            }

            if line.chars().count() > width {
                line = line.chars().take(width.saturating_sub(1)).collect();
            }

            lines.push(Line::from(vec![Span::styled(line, style)]));
        }

        if self.items.len() > self.max_visible {
            lines.push(Line::from(vec![Span::styled(
                format!("  ({}/{})", self.selected + 1, self.items.len()),
                Style::default().fg(Color::DarkGray),
            )]));
        }

        lines
    }
}
