#![allow(dead_code)]

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

#[derive(Debug, Clone)]
pub struct SettingItem {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub current_value: String,
    pub values: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SettingsList {
    items: Vec<SettingItem>,
    selected: usize,
    max_visible: usize,
}

impl SettingsList {
    pub fn new(items: Vec<SettingItem>, max_visible: usize) -> Self {
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

    pub fn cycle_selected_value(&mut self) -> Option<(String, String)> {
        let item = self.items.get_mut(self.selected)?;
        if item.values.is_empty() {
            return None;
        }

        let current_index = item
            .values
            .iter()
            .position(|value| value == &item.current_value)
            .unwrap_or(0);
        let next_index = (current_index + 1) % item.values.len();
        item.current_value = item.values[next_index].clone();
        Some((item.id.clone(), item.current_value.clone()))
    }

    pub fn render_lines(&self, width: usize) -> Vec<Line<'static>> {
        if self.items.is_empty() {
            return vec![Line::from("  no settings")];
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
            let label_style = if selected {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let value_style = if selected {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::Gray)
            };

            let mut line = format!("{prefix}{}", item.label);
            if line.chars().count() + item.current_value.chars().count() + 3 < width {
                let pad = width
                    .saturating_sub(line.chars().count())
                    .saturating_sub(item.current_value.chars().count())
                    .saturating_sub(1);
                line.push_str(&" ".repeat(pad));
            } else {
                line.push(' ');
            }

            lines.push(Line::from(vec![
                Span::styled(line, label_style),
                Span::styled(item.current_value.clone(), value_style),
            ]));

            if selected {
                if let Some(description) = &item.description {
                    lines.push(Line::from(vec![Span::styled(
                        format!("   {description}"),
                        Style::default().fg(Color::DarkGray),
                    )]));
                }
            }
        }

        lines
    }
}
