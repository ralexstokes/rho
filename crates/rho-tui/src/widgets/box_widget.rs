use ratatui::{
    style::Style,
    widgets::{Block, BorderType, Borders},
};

pub fn section_block<'a>(title: &'a str, border_style: Style) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(title)
        .border_style(border_style)
}
