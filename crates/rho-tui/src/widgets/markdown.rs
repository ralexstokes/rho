use ratatui::text::{Line, Span};

use crate::{
    terminal_image::{image_fallback, render_image_sequence},
    theme::UiTheme,
};

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

        if let Some((_alt, src)) = parse_markdown_image(line) {
            if let Some((mime_type, base64_data)) = parse_data_uri(src) {
                if let Some((sequence, rows)) =
                    render_image_sequence(base64_data, mime_type, width.saturating_sub(4).max(1))
                {
                    for _ in 1..rows {
                        lines.push(Line::from(String::new()));
                    }
                    let move_up = if rows > 1 {
                        format!("\u{1b}[{}A", rows - 1)
                    } else {
                        String::new()
                    };
                    lines.push(Line::from(format!("{move_up}{sequence}")));
                    continue;
                }

                lines.push(Line::from(vec![Span::styled(
                    image_fallback(mime_type, None, None),
                    theme.quote,
                )]));
                continue;
            }

            lines.push(Line::from(vec![Span::styled(
                image_fallback("image/*", None, Some(src)),
                theme.quote,
            )]));
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

fn parse_markdown_image(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    if !line.starts_with("![") {
        return None;
    }

    let alt_end = line.find("](")?;
    let src_end = line.rfind(')')?;
    if src_end <= alt_end + 2 {
        return None;
    }

    let alt = &line[2..alt_end];
    let src = &line[alt_end + 2..src_end];
    Some((alt, src))
}

fn parse_data_uri(uri: &str) -> Option<(&str, &str)> {
    let uri = uri.trim();
    if !uri.starts_with("data:") {
        return None;
    }
    let without_prefix = uri.trim_start_matches("data:");
    let (meta, data) = without_prefix.split_once(',')?;
    let (mime, encoding) = meta.split_once(';')?;
    if encoding != "base64" {
        return None;
    }
    Some((mime, data))
}
