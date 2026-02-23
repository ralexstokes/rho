use ratatui::text::Line;

pub struct TextBlock {
    padding_x: usize,
    padding_y: usize,
}

impl TextBlock {
    pub fn new(padding_x: usize, padding_y: usize) -> Self {
        Self {
            padding_x,
            padding_y,
        }
    }

    pub fn render_lines(&self, text: &str, width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for _ in 0..self.padding_y {
            lines.push(Line::from(String::new()));
        }

        let content_width = usize::from(width).saturating_sub(self.padding_x * 2).max(1);
        let left_pad = " ".repeat(self.padding_x);

        for wrapped in wrap_text(text, content_width) {
            lines.push(Line::from(format!("{left_pad}{wrapped}")));
        }

        for _ in 0..self.padding_y {
            lines.push(Line::from(String::new()));
        }

        lines
    }
}

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
            if let Some(space_idx) = candidate.rfind(char::is_whitespace) {
                if space_idx > 0 {
                    split_at = candidate[..space_idx].chars().count();
                }
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
