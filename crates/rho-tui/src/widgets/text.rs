use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }

    let mut out = Vec::new();
    for raw in text.split('\n') {
        if raw.is_empty() {
            out.push(String::new());
            continue;
        }

        let mut remaining = raw.trim_end_matches('\r').to_string();
        while UnicodeWidthStr::width(remaining.as_str()) > width {
            let split_at = split_byte_index_for_width(remaining.as_str(), width);
            let chunk = remaining[..split_at].to_string();
            out.push(chunk);
            remaining = remaining[split_at..].trim_start().to_string();
            if remaining.is_empty() {
                break;
            }
        }

        out.push(remaining);
    }

    out
}

fn split_byte_index_for_width(text: &str, max_width: usize) -> usize {
    let mut current_width = 0usize;
    let mut split_at = text.len();
    let mut last_whitespace = None;

    for (idx, ch) in text.char_indices() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if current_width + char_width > max_width {
            split_at = idx;
            break;
        }
        current_width += char_width;
        if ch.is_whitespace() && idx > 0 {
            last_whitespace = Some(idx);
        }
    }

    if split_at == text.len() {
        return split_at;
    }
    if let Some(space_idx) = last_whitespace
        && space_idx < split_at
    {
        return space_idx;
    }
    if split_at == 0 {
        return text.chars().next().map_or(0, char::len_utf8);
    }

    split_at
}

#[cfg(test)]
mod tests {
    use super::wrap_text;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn wraps_using_display_width_for_wide_characters() {
        let line = "⛺ 1. Pre-Colonial Era (Before 1492)";
        let wrapped = wrap_text(line, 20);
        assert!(!wrapped.is_empty());
        for segment in wrapped {
            assert!(UnicodeWidthStr::width(segment.as_str()) <= 20);
        }
    }

    #[test]
    fn wrap_makes_progress_when_wide_character_exceeds_width() {
        let wrapped = wrap_text("🧭abc", 1);
        assert_eq!(
            wrapped,
            vec![
                "🧭".to_string(),
                "a".to_string(),
                "b".to_string(),
                "c".to_string()
            ]
        );
    }
}
