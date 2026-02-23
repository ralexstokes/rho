use std::{
    io::{Stdout, stdout},
    time::Duration,
};

use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::{SinkExt, StreamExt};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use rho_core::{
    Message, MessageRole,
    protocol::{
        ClientEnvelope, ClientEvent, ErrorEvent, ServerEnvelope, ServerEvent, StartSession, UserMessage,
    },
};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Error as WsError, Message as WsMessage},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiClient {
    url: String,
}

impl TuiClient {
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into() }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub async fn run(&self) -> Result<(), TuiClientError> {
        let (socket, _) = connect_async(self.url.as_str())
            .await
            .map_err(TuiClientError::Connect)?;
        let (writer, reader) = socket.split();

        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
        let (inbound_tx, mut inbound_rx) = mpsc::unbounded_channel();

        tokio::spawn(run_writer(writer, outbound_rx));
        tokio::spawn(run_reader(reader, inbound_tx));

        send_outbound(
            &outbound_tx,
            ClientEvent::StartSession(StartSession { session_id: None }),
        )?;

        let session_id = wait_for_session_ack(&mut inbound_rx).await?;
        let mut app = AppState::new(self.url.clone(), session_id);

        let mut terminal = TerminalGuard::new()?;
        let mut events = EventStream::new();
        let mut tick = tokio::time::interval(Duration::from_millis(33));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        while !app.should_quit {
            terminal
                .terminal_mut()
                .draw(|frame| draw_ui(frame, &app))
                .map_err(TuiClientError::Io)?;

            tokio::select! {
                maybe_inbound = inbound_rx.recv() => {
                    match maybe_inbound {
                        Some(event) => app.handle_network_event(event),
                        None => {
                            app.push_system("connection closed".to_string());
                            app.should_quit = true;
                        }
                    }
                }
                maybe_event = events.next() => {
                    match maybe_event {
                        Some(Ok(event)) => {
                            handle_terminal_event(event, &mut app, &outbound_tx)?;
                        }
                        Some(Err(err)) => return Err(TuiClientError::Io(err)),
                        None => {
                            app.should_quit = true;
                        }
                    }
                }
                _ = tick.tick() => {}
            }
        }

        drop(outbound_tx);
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum TuiClientError {
    #[error("websocket connect failed: {0}")]
    Connect(#[source] WsError),
    #[error("websocket send failed: {0}")]
    Send(#[source] WsError),
    #[error("websocket receive failed: {0}")]
    Receive(#[source] WsError),
    #[error("websocket closed unexpectedly")]
    Closed,
    #[error("invalid protocol payload: {0}")]
    Protocol(#[source] serde_json::Error),
    #[error("io failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("session initialization failed: {0}")]
    SessionInitialization(String),
    #[error("outbound channel is closed")]
    OutboundChannelClosed,
}

type WsWriter = futures_util::stream::SplitSink<
    WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    WsMessage,
>;
type WsReader = futures_util::stream::SplitStream<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>>;

#[derive(Debug)]
enum NetworkEvent {
    Server(ServerEvent),
    Closed,
    ReceiveError(String),
    ProtocolError(String),
}

#[derive(Debug, Clone)]
enum LogKind {
    User,
    Assistant,
    Tool,
    System,
    Error,
}

#[derive(Debug, Clone)]
struct LogLine {
    kind: LogKind,
    text: String,
}

#[derive(Debug)]
struct AppState {
    url: String,
    session_id: String,
    log_lines: Vec<LogLine>,
    active_assistant_line: Option<usize>,
    input: String,
    cursor_chars: usize,
    should_quit: bool,
}

impl AppState {
    fn new(url: String, session_id: String) -> Self {
        let mut app = Self {
            url,
            session_id,
            log_lines: Vec::new(),
            active_assistant_line: None,
            input: String::new(),
            cursor_chars: 0,
            should_quit: false,
        };
        app.push_system("connected".to_string());
        app
    }

    fn push_line(&mut self, kind: LogKind, text: String) {
        self.log_lines.push(LogLine { kind, text });
    }

    fn push_system(&mut self, text: String) {
        self.push_line(LogKind::System, text);
        self.active_assistant_line = None;
    }

    fn push_error(&mut self, text: String) {
        self.push_line(LogKind::Error, text);
        self.active_assistant_line = None;
    }

    fn push_user(&mut self, text: String) {
        self.push_line(LogKind::User, text);
        self.active_assistant_line = None;
    }

    fn append_assistant_delta(&mut self, delta: &str) {
        if let Some(index) = self.active_assistant_line {
            if let Some(line) = self.log_lines.get_mut(index) {
                line.text.push_str(delta);
                return;
            }
        }

        self.log_lines.push(LogLine {
            kind: LogKind::Assistant,
            text: delta.to_string(),
        });
        self.active_assistant_line = self.log_lines.len().checked_sub(1);
    }

    fn finalize_assistant(&mut self, message: String) {
        if let Some(index) = self.active_assistant_line {
            if let Some(line) = self.log_lines.get_mut(index) {
                line.text = message;
            }
        } else {
            self.log_lines.push(LogLine {
                kind: LogKind::Assistant,
                text: message,
            });
        }
        self.active_assistant_line = None;
    }

    fn handle_network_event(&mut self, event: NetworkEvent) {
        match event {
            NetworkEvent::Server(server_event) => match server_event {
                ServerEvent::SessionAck(_) => {}
                ServerEvent::AssistantDelta(delta) => self.append_assistant_delta(delta.delta.as_str()),
                ServerEvent::ToolStarted(tool_started) => {
                    self.push_line(LogKind::Tool, format_tool_started(&tool_started.call));
                }
                ServerEvent::ToolCompleted(tool_completed) => {
                    self.push_line(LogKind::Tool, format_tool_completed(&tool_completed.result));
                }
                ServerEvent::Final(final_message) => {
                    self.finalize_assistant(final_message.message.content);
                }
                ServerEvent::Error(error) => {
                    self.push_error(format_error(&error));
                }
            },
            NetworkEvent::Closed => {
                self.push_error("websocket closed unexpectedly".to_string());
                self.should_quit = true;
            }
            NetworkEvent::ReceiveError(err) => {
                self.push_error(err);
                self.should_quit = true;
            }
            NetworkEvent::ProtocolError(err) => {
                self.push_error(err);
                self.should_quit = true;
            }
        }
    }

    fn commit_input_line(&mut self) -> String {
        self.cursor_chars = 0;
        std::mem::take(&mut self.input)
    }

    fn input_char_len(&self) -> usize {
        self.input.chars().count()
    }

    fn cursor_char_index(&self) -> usize {
        self.cursor_chars.min(self.input_char_len())
    }

    fn move_cursor_left(&mut self) {
        self.cursor_chars = self.cursor_char_index().saturating_sub(1);
    }

    fn move_cursor_right(&mut self) {
        let len = self.input_char_len();
        self.cursor_chars = (self.cursor_char_index() + 1).min(len);
    }

    fn move_cursor_home(&mut self) {
        self.cursor_chars = 0;
    }

    fn move_cursor_end(&mut self) {
        self.cursor_chars = self.input_char_len();
    }

    fn insert_char(&mut self, ch: char) {
        let cursor = self.cursor_char_index();
        let byte_index = char_to_byte_index(self.input.as_str(), cursor);
        self.input.insert(byte_index, ch);
        self.cursor_chars = cursor + 1;
    }

    fn backspace(&mut self) {
        let cursor = self.cursor_char_index();
        if cursor == 0 {
            return;
        }

        let remove_start = char_to_byte_index(self.input.as_str(), cursor - 1);
        let remove_end = char_to_byte_index(self.input.as_str(), cursor);
        self.input.replace_range(remove_start..remove_end, "");
        self.cursor_chars = cursor - 1;
    }

    fn delete(&mut self) {
        let cursor = self.cursor_char_index();
        let len = self.input_char_len();
        if cursor >= len {
            return;
        }

        let remove_start = char_to_byte_index(self.input.as_str(), cursor);
        let remove_end = char_to_byte_index(self.input.as_str(), cursor + 1);
        self.input.replace_range(remove_start..remove_end, "");
    }

    fn render_input(&self, max_cols: usize) -> (String, usize) {
        if max_cols == 0 {
            return (String::new(), 0);
        }

        let total_chars = self.input_char_len();
        let cursor = self.cursor_char_index();
        if total_chars <= max_cols {
            return (self.input.clone(), cursor.min(max_cols));
        }

        let max_start = total_chars.saturating_sub(max_cols);
        let start = cursor.saturating_sub(max_cols.saturating_sub(1)).min(max_start);
        let end = (start + max_cols).min(total_chars);
        let rendered = self
            .input
            .chars()
            .skip(start)
            .take(end.saturating_sub(start))
            .collect();
        let cursor_col = cursor.saturating_sub(start).min(max_cols.saturating_sub(1));
        (rendered, cursor_col)
    }
}

async fn run_writer(
    mut writer: WsWriter,
    mut outbound_rx: mpsc::UnboundedReceiver<ClientEvent>,
) -> Result<(), TuiClientError> {
    while let Some(event) = outbound_rx.recv().await {
        send_client_event(&mut writer, event).await?;
    }
    Ok(())
}

async fn run_reader(
    mut reader: WsReader,
    inbound_tx: mpsc::UnboundedSender<NetworkEvent>,
) -> Result<(), TuiClientError> {
    loop {
        match recv_server_envelope(&mut reader).await {
            Ok(envelope) => {
                if inbound_tx.send(NetworkEvent::Server(envelope.event)).is_err() {
                    return Ok(());
                }
            }
            Err(TuiClientError::Closed) => {
                let _ = inbound_tx.send(NetworkEvent::Closed);
                return Ok(());
            }
            Err(TuiClientError::Receive(err)) => {
                let _ = inbound_tx.send(NetworkEvent::ReceiveError(err.to_string()));
                return Ok(());
            }
            Err(TuiClientError::Protocol(err)) => {
                let _ = inbound_tx.send(NetworkEvent::ProtocolError(err.to_string()));
                return Ok(());
            }
            Err(other) => {
                let _ = inbound_tx.send(NetworkEvent::ReceiveError(other.to_string()));
                return Ok(());
            }
        }
    }
}

fn send_outbound(
    outbound_tx: &mpsc::UnboundedSender<ClientEvent>,
    event: ClientEvent,
) -> Result<(), TuiClientError> {
    outbound_tx
        .send(event)
        .map_err(|_| TuiClientError::OutboundChannelClosed)
}

fn draw_ui(frame: &mut Frame<'_>, app: &AppState) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let history_lines: Vec<Line<'_>> = app
        .log_lines
        .iter()
        .map(|entry| match entry.kind {
            LogKind::User => Line::from(vec![
                Span::styled(
                    "you> ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(entry.text.as_str()),
            ]),
            LogKind::Assistant => Line::from(vec![
                Span::styled(
                    "assistant> ",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(entry.text.as_str()),
            ]),
            LogKind::Tool => Line::from(vec![
                Span::styled(
                    "tool> ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(entry.text.as_str()),
            ]),
            LogKind::System => Line::from(vec![
                Span::styled(
                    "system> ",
                    Style::default()
                        .fg(Color::Gray)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(entry.text.as_str()),
            ]),
            LogKind::Error => Line::from(vec![
                Span::styled(
                    "error> ",
                    Style::default()
                        .fg(Color::Red)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(entry.text.as_str()),
            ]),
        })
        .collect();

    let viewport_height = usize::from(layout[0].height.saturating_sub(2));
    let history_scroll = history_lines.len().saturating_sub(viewport_height);
    let history_scroll = u16::try_from(history_scroll).unwrap_or(u16::MAX);

    let history = Paragraph::new(history_lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("rho chat")
                .border_style(Style::default().fg(Color::Blue)),
        )
        .scroll((history_scroll, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(history, layout[0]);

    let input_width = usize::from(layout[1].width.saturating_sub(2));
    let (input_display, cursor_col) = app.render_input(input_width);
    let input = Paragraph::new(input_display).block(
        Block::default()
            .borders(Borders::ALL)
            .title("input")
            .border_style(Style::default().fg(Color::Magenta)),
    );
    frame.render_widget(input, layout[1]);

    let status = Paragraph::new(format!(
        "url={} session_id={}  enter=send  /cancel  esc=quit",
        app.url, app.session_id
    ))
    .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(status, layout[2]);

    let input_x = layout[1]
        .x
        .saturating_add(1)
        .saturating_add(u16::try_from(cursor_col).unwrap_or(u16::MAX));
    let input_y = layout[1].y.saturating_add(1);
    frame.set_cursor_position((input_x, input_y));
}

fn handle_terminal_event(
    event: Event,
    app: &mut AppState,
    outbound_tx: &mpsc::UnboundedSender<ClientEvent>,
) -> Result<(), TuiClientError> {
    match event {
        Event::Key(key) => handle_key_event(key, app, outbound_tx),
        Event::Resize(_, _) => Ok(()),
        _ => Ok(()),
    }
}

fn handle_key_event(
    key: KeyEvent,
    app: &mut AppState,
    outbound_tx: &mpsc::UnboundedSender<ClientEvent>,
) -> Result<(), TuiClientError> {
    if key.kind != KeyEventKind::Press {
        return Ok(());
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
        app.should_quit = true;
        return Ok(());
    }

    match key.code {
        KeyCode::Esc => {
            app.should_quit = true;
        }
        KeyCode::Enter => {
            submit_input(app, outbound_tx)?;
        }
        KeyCode::Backspace => app.backspace(),
        KeyCode::Delete => app.delete(),
        KeyCode::Left => app.move_cursor_left(),
        KeyCode::Right => app.move_cursor_right(),
        KeyCode::Home => app.move_cursor_home(),
        KeyCode::End => app.move_cursor_end(),
        KeyCode::Char(ch)
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT) =>
        {
            app.insert_char(ch)
        }
        _ => {}
    }

    Ok(())
}

fn submit_input(
    app: &mut AppState,
    outbound_tx: &mpsc::UnboundedSender<ClientEvent>,
) -> Result<(), TuiClientError> {
    let line = app.commit_input_line();
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(());
    }

    if trimmed == "/quit" {
        app.should_quit = true;
        return Ok(());
    }

    if trimmed == "/cancel" {
        send_outbound(
            outbound_tx,
            ClientEvent::Cancel(rho_core::protocol::CancelRequest {
                session_id: app.session_id.clone(),
            }),
        )?;
        app.push_system("cancel requested".to_string());
        return Ok(());
    }

    app.push_user(line.clone());
    send_outbound(
        outbound_tx,
        ClientEvent::UserMessage(UserMessage {
            session_id: app.session_id.clone(),
            message: Message::new(MessageRole::User, line),
        }),
    )?;

    Ok(())
}

async fn send_client_event(writer: &mut WsWriter, event: ClientEvent) -> Result<(), TuiClientError> {
    let text = serde_json::to_string(&ClientEnvelope::new(event)).map_err(TuiClientError::Protocol)?;
    writer
        .send(WsMessage::Text(text.into()))
        .await
        .map_err(TuiClientError::Send)
}

async fn wait_for_session_ack(
    inbound_rx: &mut mpsc::UnboundedReceiver<NetworkEvent>,
) -> Result<String, TuiClientError> {
    while let Some(network_event) = inbound_rx.recv().await {
        match network_event {
            NetworkEvent::Server(server_event) => match server_event {
                ServerEvent::SessionAck(ack) => return Ok(ack.session_id),
                ServerEvent::Error(error) => {
                    return Err(TuiClientError::SessionInitialization(format_error(&error)));
                }
                _ => {}
            },
            NetworkEvent::Closed => return Err(TuiClientError::Closed),
            NetworkEvent::ReceiveError(err) | NetworkEvent::ProtocolError(err) => {
                return Err(TuiClientError::SessionInitialization(err));
            }
        }
    }

    Err(TuiClientError::Closed)
}

async fn recv_server_envelope(reader: &mut WsReader) -> Result<ServerEnvelope, TuiClientError> {
    loop {
        let Some(next_message) = reader.next().await else {
            return Err(TuiClientError::Closed);
        };

        let message = next_message.map_err(TuiClientError::Receive)?;
        match message {
            WsMessage::Text(text) => {
                return serde_json::from_str(text.as_str()).map_err(TuiClientError::Protocol);
            }
            WsMessage::Binary(_) | WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Frame(_) => {}
            WsMessage::Close(_) => return Err(TuiClientError::Closed),
        }
    }
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    fn new() -> Result<Self, TuiClientError> {
        enable_raw_mode().map_err(TuiClientError::Io)?;

        let mut stdout = stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture).map_err(TuiClientError::Io)?;

        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend).map_err(TuiClientError::Io)?;
        Ok(Self { terminal })
    }

    fn terminal_mut(&mut self) -> &mut Terminal<CrosstermBackend<Stdout>> {
        &mut self.terminal
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        );
        let _ = self.terminal.show_cursor();
    }
}

fn char_to_byte_index(text: &str, char_index: usize) -> usize {
    if char_index == 0 {
        return 0;
    }

    text.char_indices()
        .nth(char_index)
        .map(|(index, _)| index)
        .unwrap_or(text.len())
}

fn format_tool_started(call: &rho_core::ToolCall) -> String {
    format!(
        "started call_id={} name={} input={}",
        call.call_id, call.name, call.input
    )
}

fn format_tool_completed(result: &rho_core::ToolResult) -> String {
    let status = if result.is_error { "error" } else { "ok" };
    format!(
        "completed call_id={} status={} output={}",
        result.call_id, status, result.output
    )
}

fn format_error(error: &ErrorEvent) -> String {
    match &error.session_id {
        Some(session_id) => format!(
            "session_id={} code={} message={}",
            session_id, error.code, error.message
        ),
        None => format!("code={} message={}", error.code, error.message),
    }
}

#[cfg(test)]
mod tests {
    use rho_core::tool::{ToolCall, ToolResult};
    use serde_json::json;

    use super::*;

    #[test]
    fn tool_started_format_includes_call_id_name_and_input() {
        let call = ToolCall {
            id: None,
            call_id: "call-1".to_string(),
            name: "read".to_string(),
            input: json!({"path":"/tmp/demo.txt"}),
        };

        let rendered = format_tool_started(&call);
        assert!(rendered.contains("call_id=call-1"));
        assert!(rendered.contains("name=read"));
        assert!(rendered.contains("\"path\":\"/tmp/demo.txt\""));
    }

    #[test]
    fn tool_completed_format_marks_error_status() {
        let result = ToolResult {
            call_id: "call-2".to_string(),
            output: "boom".to_string(),
            is_error: true,
        };

        let rendered = format_tool_completed(&result);
        assert!(rendered.contains("call_id=call-2"));
        assert!(rendered.contains("status=error"));
        assert!(rendered.contains("output=boom"));
    }

    #[test]
    fn error_format_with_session_id_includes_session_prefix() {
        let error = ErrorEvent {
            session_id: Some("session-z".to_string()),
            code: "runtime_error".to_string(),
            message: "boom".to_string(),
        };

        let rendered = format_error(&error);
        assert_eq!(
            rendered,
            "session_id=session-z code=runtime_error message=boom"
        );
    }

    #[test]
    fn render_input_keeps_cursor_visible_when_clipped() {
        let app = AppState {
            url: "ws://localhost:8787/ws".to_string(),
            session_id: "session-1".to_string(),
            log_lines: Vec::new(),
            active_assistant_line: None,
            input: "abcdefghij".to_string(),
            cursor_chars: 9,
            should_quit: false,
        };

        let (visible, col) = app.render_input(5);
        assert_eq!(visible, "fghij");
        assert_eq!(col, 4);
    }
}
