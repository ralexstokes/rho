use ratatui::text::{Line, Span};

use crate::theme::UiTheme;

use super::text::wrap_text;

pub fn render_markdown(markdown: &str, width: u16, theme: &UiTheme) -> Vec<Line<'static>> {
    let content_width = usize::from(width).max(1);
    let mut lines = Vec::new();
    let mut in_code_block = false;

    for raw_line in markdown.split('\n') {
        let line = raw_line.trim_end_matches('\r');
        if line.starts_with("```") {
            in_code_block = !in_code_block;
            lines.push(Line::from(vec![Span::styled(
                line.to_string(),
                theme.code,
            )]));
            continue;
        }

        if in_code_block {
            for wrapped in wrap_text(line, content_width) {
                lines.push(Line::from(vec![Span::styled(wrapped, theme.code)]));
            }
            continue;
        }

        if let Some(rest) = parse_heading(line) {
            for wrapped in wrap_text(rest, content_width) {
                lines.push(Line::from(vec![Span::styled(wrapped, theme.heading)]));
            }
            continue;
        }

        if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
            let bullet_width = content_width.saturating_sub(2).max(1);
            let wrapped = wrap_text(rest, bullet_width);
            for (index, item_line) in wrapped.into_iter().enumerate() {
                if index == 0 {
                    lines.push(Line::from(vec![
                        Span::styled("• ", theme.bullet),
                        Span::styled(item_line, theme.body),
                    ]));
                } else {
                    lines.push(Line::from(vec![
                        Span::styled("  ", theme.bullet),
                        Span::styled(item_line, theme.body),
                    ]));
                }
            }
            continue;
        }

        if let Some(rest) = line.strip_prefix("> ") {
            let quote_width = content_width.saturating_sub(2).max(1);
            for wrapped in wrap_text(rest, quote_width) {
                lines.push(Line::from(vec![
                    Span::styled("│ ", theme.quote),
                    Span::styled(wrapped, theme.quote),
                ]));
            }
            continue;
        }

        for wrapped in wrap_text(line, content_width) {
            lines.push(Line::from(vec![Span::styled(wrapped, theme.body)]));
        }
    }

    if lines.is_empty() {
        lines.push(Line::from(String::new()));
    }

    lines
}

fn parse_heading(line: &str) -> Option<&str> {
    let hash_count = line.chars().take_while(|ch| *ch == '#').count();
    if hash_count == 0 || hash_count > 6 {
        return None;
    }
    let mut chars = line.chars();
    for _ in 0..hash_count {
        let _ = chars.next();
    }

    if !matches!(chars.next(), Some(' ')) {
        return None;
    }

    Some(chars.as_str())
}
