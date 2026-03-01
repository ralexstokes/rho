use ratatui::text::Line;

pub fn spacer_lines(count: usize) -> Vec<Line<'static>> {
    vec![Line::from(String::new()); count]
}
