use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone)]
pub struct UiTheme {
    pub history_border: Style,
    pub user_prefix: Style,
    pub assistant_prefix: Style,
    pub tool_prefix: Style,
    pub system_prefix: Style,
    pub system_body: Style,
    pub error_prefix: Style,
    pub heading: Style,
    pub bullet: Style,
    pub quote: Style,
    pub code: Style,
    pub body: Style,
    pub loader: Style,
    pub footer: Style,
}

impl Default for UiTheme {
    fn default() -> Self {
        Self {
            history_border: Style::default().fg(Color::Rgb(68, 76, 92)),
            user_prefix: Style::default()
                .fg(Color::Rgb(86, 182, 194))
                .add_modifier(Modifier::BOLD),
            assistant_prefix: Style::default()
                .fg(Color::Rgb(198, 120, 221))
                .add_modifier(Modifier::BOLD),
            tool_prefix: Style::default()
                .fg(Color::Rgb(209, 154, 102))
                .add_modifier(Modifier::BOLD),
            system_prefix: Style::default()
                .fg(Color::Rgb(92, 99, 112))
                .add_modifier(Modifier::BOLD),
            system_body: Style::default().fg(Color::Rgb(92, 99, 112)),
            error_prefix: Style::default()
                .fg(Color::Rgb(224, 108, 117))
                .add_modifier(Modifier::BOLD),
            heading: Style::default()
                .fg(Color::Rgb(97, 175, 239))
                .add_modifier(Modifier::BOLD),
            bullet: Style::default().fg(Color::Rgb(152, 195, 121)),
            quote: Style::default()
                .fg(Color::Rgb(124, 132, 149))
                .add_modifier(Modifier::ITALIC),
            code: Style::default().fg(Color::Rgb(229, 192, 123)),
            body: Style::default().fg(Color::White),
            loader: Style::default().fg(Color::Rgb(198, 120, 221)),
            footer: Style::default().fg(Color::Rgb(171, 178, 191)),
        }
    }
}
