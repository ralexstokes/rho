use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone)]
pub struct UiTheme {
    pub history_border: Style,
    pub input_border: Style,
    pub user_prefix: Style,
    pub assistant_prefix: Style,
    pub tool_prefix: Style,
    pub system_prefix: Style,
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
            history_border: Style::default().fg(Color::Blue),
            input_border: Style::default().fg(Color::Magenta),
            user_prefix: Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            assistant_prefix: Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
            tool_prefix: Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            system_prefix: Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
            error_prefix: Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            heading: Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
            bullet: Style::default().fg(Color::LightBlue),
            quote: Style::default().fg(Color::LightMagenta),
            code: Style::default().fg(Color::LightYellow),
            body: Style::default().fg(Color::White),
            loader: Style::default().fg(Color::LightGreen),
            footer: Style::default().fg(Color::DarkGray),
        }
    }
}
