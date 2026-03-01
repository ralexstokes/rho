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
        while remaining.chars().count() > width {
            let mut split_at = width;
            let candidate: String = remaining.chars().take(width).collect();
            if let Some(space_idx) = candidate.rfind(char::is_whitespace)
                && space_idx > 0
            {
                split_at = candidate[..space_idx].chars().count();
            }

            let chunk: String = remaining.chars().take(split_at).collect();
            out.push(chunk);
            remaining = remaining
                .chars()
                .skip(split_at)
                .collect::<String>()
                .trim_start()
                .to_string();
        }

        out.push(remaining);
    }

    out
}
