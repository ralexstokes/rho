pub fn truncate_to_width(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }

    let count = text.chars().count();
    if count <= max_width {
        return text.to_string();
    }

    if max_width <= 3 {
        return text.chars().take(max_width).collect();
    }

    let mut out: String = text.chars().take(max_width.saturating_sub(3)).collect();
    out.push_str("...");
    out
}
