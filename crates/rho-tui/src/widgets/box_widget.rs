use ratatui::{
    style::Style,
    widgets::{Block, Borders},
};

pub fn section_block<'a>(title: &'a str, border_style: Style) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(border_style)
}
