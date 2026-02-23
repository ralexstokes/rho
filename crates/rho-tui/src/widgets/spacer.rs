use ratatui::text::Line;

pub fn spacer_lines(lines: usize) -> Vec<Line<'static>> {
    (0..lines).map(|_| Line::from(String::new())).collect()
}
