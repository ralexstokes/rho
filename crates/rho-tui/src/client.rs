use std::{
    time::Duration,
};

use crate::{
    autocomplete::{AutocompleteItem, AutocompleteProvider, CombinedAutocompleteProvider},
    editor::EditorState,
    keys::{Key, is_key_release, is_key_repeat, matches_key},
    overlay::{OverlayAnchor, OverlayMargin, OverlayOptions, OverlayStack, SizeValue},
    select_list::SelectList,
    terminal::ProcessTerminal,
    theme::UiTheme,
    widgets::{
        TextBlock, loader_frame, render_markdown, section_block, spacer_lines, truncate_to_width,
    },
};
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use futures_util::{SinkExt, StreamExt};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
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

        let mut terminal = ProcessTerminal::start().map_err(TuiClientError::Io)?;
        let mut events = EventStream::new();
        let mut tick = tokio::time::interval(Duration::from_millis(33));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut render_state = RenderState::from_env();

        while !app.should_quit {
            render_state.prepare_draw(&mut terminal, app.estimated_rendered_lines())?;
            if let Err(err) = terminal
                .terminal_mut()
                .draw(|frame| draw_ui(frame, &app))
            {
                let _ = render_state.finish_draw(&mut terminal);
                return Err(TuiClientError::Io(err));
            }
            render_state.finish_draw(&mut terminal)?;

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
                _ = tick.tick() => {
                    app.frame_tick = app.frame_tick.wrapping_add(1);
                }
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
    theme: UiTheme,
    log_lines: Vec<LogLine>,
    active_assistant_line: Option<usize>,
    editor: EditorState,
    autocomplete_provider: CombinedAutocompleteProvider,
    autocomplete_list: Option<SelectList>,
    autocomplete_prefix: String,
    overlays: OverlayStack,
    autocomplete_overlay_id: Option<u64>,
    frame_tick: u64,
    should_quit: bool,
}

#[derive(Debug, Clone)]
struct RenderState {
    previous_width: Option<u16>,
    max_rendered_lines: usize,
    clear_on_shrink: bool,
    sync_output: bool,
}

impl RenderState {
    fn from_env() -> Self {
        Self {
            previous_width: None,
            max_rendered_lines: 0,
            clear_on_shrink: std::env::var("PI_CLEAR_ON_SHRINK").is_ok_and(|v| v == "1"),
            sync_output: std::env::var("PI_SYNC_OUTPUT").is_ok_and(|v| v == "1"),
        }
    }

    fn prepare_draw(
        &mut self,
        terminal: &mut ProcessTerminal,
        estimated_lines: usize,
    ) -> Result<(), TuiClientError> {
        let width = terminal.width().map_err(TuiClientError::Io)?;
        if self.previous_width.is_some_and(|previous| previous != width) {
            terminal.clear().map_err(TuiClientError::Io)?;
            self.max_rendered_lines = 0;
        }

        if self.clear_on_shrink && estimated_lines < self.max_rendered_lines {
            terminal.clear().map_err(TuiClientError::Io)?;
            self.max_rendered_lines = estimated_lines;
        } else {
            self.max_rendered_lines = self.max_rendered_lines.max(estimated_lines);
        }

        self.previous_width = Some(width);
        if self.sync_output {
            terminal
                .begin_synchronized_output()
                .map_err(TuiClientError::Io)?;
        }
        Ok(())
    }

    fn finish_draw(&self, terminal: &mut ProcessTerminal) -> Result<(), TuiClientError> {
        if self.sync_output {
            terminal.end_synchronized_output().map_err(TuiClientError::Io)?;
        }
        Ok(())
    }
}

impl AppState {
    fn new(url: String, session_id: String) -> Self {
        let mut app = Self {
            url,
            session_id,
            theme: UiTheme::default(),
            log_lines: Vec::new(),
            active_assistant_line: None,
            editor: EditorState::new(),
            autocomplete_provider: CombinedAutocompleteProvider::default_commands(
                std::env::current_dir().unwrap_or_else(|_| ".".into()),
            ),
            autocomplete_list: None,
            autocomplete_prefix: String::new(),
            overlays: OverlayStack::new(),
            autocomplete_overlay_id: None,
            frame_tick: 0,
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

    fn clear_autocomplete(&mut self) {
        self.autocomplete_list = None;
        self.autocomplete_prefix.clear();
        if let Some(id) = self.autocomplete_overlay_id.take() {
            let _ = self.overlays.hide(id);
        }
    }

    fn has_autocomplete(&self) -> bool {
        self.autocomplete_list.is_some()
    }

    fn refresh_autocomplete(&mut self, force_file: bool) {
        let (cursor_line, cursor_col) = self.editor.cursor_position();
        let lines = self.editor.lines();
        let suggestions = if force_file {
            self.autocomplete_provider
                .force_file_suggestions(lines, cursor_line, cursor_col)
        } else {
            self.autocomplete_provider
                .suggestions(lines, cursor_line, cursor_col)
        };

        let Some(suggestions) = suggestions else {
            self.clear_autocomplete();
            return;
        };

        if force_file && suggestions.items.len() == 1 {
            if let Some(item) = suggestions.items.first() {
                self.apply_autocomplete_item(item.clone(), suggestions.prefix);
            }
            return;
        }

        self.autocomplete_prefix = suggestions.prefix;
        self.autocomplete_list = Some(SelectList::new(suggestions.items, 6));
        self.sync_autocomplete_overlay();
    }

    fn autocomplete_up(&mut self) {
        if let Some(list) = &mut self.autocomplete_list {
            list.move_up();
        }
        self.sync_autocomplete_overlay();
    }

    fn autocomplete_down(&mut self) {
        if let Some(list) = &mut self.autocomplete_list {
            list.move_down();
        }
        self.sync_autocomplete_overlay();
    }

    fn apply_autocomplete_selection(&mut self) -> bool {
        let Some(list) = &self.autocomplete_list else {
            return false;
        };
        let Some(item) = list.selected_item().cloned() else {
            return false;
        };
        let prefix = self.autocomplete_prefix.clone();
        self.apply_autocomplete_item(item, prefix);
        true
    }

    fn apply_autocomplete_item(&mut self, item: AutocompleteItem, prefix: String) {
        let mut replacement = item.value;
        if item.trailing_space {
            replacement.push(' ');
        }
        let prefix_chars = prefix.chars().count();
        self.editor
            .replace_prefix_before_cursor(prefix_chars, replacement.as_str());
        self.clear_autocomplete();
    }

    fn sync_autocomplete_overlay(&mut self) {
        let Some(list) = &self.autocomplete_list else {
            if let Some(id) = self.autocomplete_overlay_id.take() {
                let _ = self.overlays.hide(id);
            }
            return;
        };

        let lines = list.render_lines(56);
        let options = OverlayOptions {
            width: Some(SizeValue::Percent(55)),
            min_width: Some(28),
            max_height: Some(SizeValue::Absolute(8)),
            anchor: OverlayAnchor::BottomLeft,
            offset_x: 1,
            offset_y: -8,
            margin: OverlayMargin {
                top: 1,
                right: 1,
                bottom: 2,
                left: 1,
            },
            min_terminal_width: Some(48),
            title: Some("completions".to_string()),
            ..OverlayOptions::default()
        };

        if let Some(id) = self.autocomplete_overlay_id {
            if self.overlays.update(id, lines.clone()) {
                let _ = self.overlays.set_hidden(id, false);
                return;
            }
        }

        let id = self.overlays.show(lines, options);
        self.autocomplete_overlay_id = Some(id);
    }

    fn estimated_rendered_lines(&self) -> usize {
        let history = self.log_lines.len();
        let editor = self.editor.lines().len().max(1);
        history + editor + 4
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
            Constraint::Length(6),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let text_block = TextBlock::new(0, 0);
    let history_width = layout[0].width.saturating_sub(2);
    let mut history_lines = Vec::new();

    for entry in &app.log_lines {
        let (prefix, prefix_style, markdown_mode) = match entry.kind {
            LogKind::User => ("you> ", app.theme.user_prefix, true),
            LogKind::Assistant => ("assistant> ", app.theme.assistant_prefix, true),
            LogKind::Tool => ("tool> ", app.theme.tool_prefix, false),
            LogKind::System => ("system> ", app.theme.system_prefix, false),
            LogKind::Error => ("error> ", app.theme.error_prefix, false),
        };

        let prefix_width = u16::try_from(prefix.chars().count()).unwrap_or(u16::MAX);
        let body_width = history_width.saturating_sub(prefix_width).max(1);
        let body_lines = if markdown_mode {
            render_markdown(entry.text.as_str(), body_width, &app.theme)
        } else {
            text_block.render_lines(entry.text.as_str(), body_width)
        };

        let indent = " ".repeat(prefix.chars().count());
        for (index, line) in body_lines.into_iter().enumerate() {
            let mut spans = Vec::new();
            if index == 0 {
                spans.push(Span::styled(prefix.to_string(), prefix_style));
            } else {
                spans.push(Span::styled(indent.clone(), prefix_style));
            }
            spans.extend(line.spans);
            history_lines.push(Line::from(spans));
        }
    }

    if app.active_assistant_line.is_some() {
        history_lines.extend(spacer_lines(1));
        history_lines.push(Line::from(vec![
            Span::styled("assistant> ", app.theme.assistant_prefix),
            Span::styled(
                format!("{} thinking...", loader_frame(app.frame_tick)),
                app.theme.loader,
            ),
        ]));
    }

    let viewport_height = usize::from(layout[0].height.saturating_sub(2));
    let history_scroll = history_lines.len().saturating_sub(viewport_height);
    let history_scroll = u16::try_from(history_scroll).unwrap_or(u16::MAX);

    let history = Paragraph::new(history_lines)
        .block(section_block("rho chat", app.theme.history_border))
        .scroll((history_scroll, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(history, layout[0]);

    let editor_height = usize::from(layout[1].height.saturating_sub(2));
    let editor_width = usize::from(layout[1].width.saturating_sub(2));
    let editor_render = app.editor.render(editor_width, editor_height);
    let input = Paragraph::new(editor_render.lines.join("\n"))
        .block(section_block("editor", app.theme.input_border))
        .wrap(Wrap { trim: false });
    frame.render_widget(input, layout[1]);

    let footer_text = format!(
        "url={} session_id={}  enter=send  shift+enter=newline  tab=complete  esc=quit",
        app.url, app.session_id
    );
    let status = Paragraph::new(truncate_to_width(
        footer_text.as_str(),
        usize::from(layout[2].width),
    ))
    .style(app.theme.footer);
    frame.render_widget(status, layout[2]);

    let input_x = layout[1]
        .x
        .saturating_add(1)
        .saturating_add(u16::try_from(editor_render.cursor_col).unwrap_or(u16::MAX));
    let input_y = layout[1]
        .y
        .saturating_add(1)
        .saturating_add(u16::try_from(editor_render.cursor_row).unwrap_or(u16::MAX));

    app.overlays.render(frame, frame.area());
    frame.set_cursor_position((input_x, input_y));
}

fn handle_terminal_event(
    event: Event,
    app: &mut AppState,
    outbound_tx: &mpsc::UnboundedSender<ClientEvent>,
) -> Result<(), TuiClientError> {
    match event {
        Event::Key(key) => handle_key_event(key, app, outbound_tx),
        Event::Paste(text) => {
            app.editor.handle_paste(text);
            app.refresh_autocomplete(false);
            Ok(())
        }
        Event::Resize(_, _) => Ok(()),
        _ => Ok(()),
    }
}

fn handle_key_event(
    key: KeyEvent,
    app: &mut AppState,
    outbound_tx: &mpsc::UnboundedSender<ClientEvent>,
) -> Result<(), TuiClientError> {
    if is_key_release(&key) {
        return Ok(());
    }
    if is_key_repeat(&key) && matches_key(&key, Key::TAB) {
        return Ok(());
    }

    if matches_key(&key, Key::ctrl('c').as_str()) {
        app.should_quit = true;
        return Ok(());
    }

    let mut edited = false;
    match key.code {
        KeyCode::Esc => {
            if app.has_autocomplete() {
                app.clear_autocomplete();
            } else if app.overlays.hide_topmost() {
                app.autocomplete_overlay_id = None;
            } else {
                app.should_quit = true;
            }
        }
        KeyCode::Enter => {
            if app.has_autocomplete() && !key.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) {
                app.apply_autocomplete_selection();
            } else if key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL)
            {
                app.editor.insert_newline();
                edited = true;
            } else {
                submit_input(app, outbound_tx)?;
                app.clear_autocomplete();
            }
        }
        KeyCode::Tab => {
            app.refresh_autocomplete(true);
        }
        KeyCode::Backspace => {
            app.editor.backspace();
            edited = true;
        }
        KeyCode::Delete => {
            app.editor.delete();
            edited = true;
        }
        KeyCode::Left => app.editor.move_left(),
        KeyCode::Right => app.editor.move_right(),
        KeyCode::Up => {
            if app.has_autocomplete() {
                app.autocomplete_up();
            } else if app.editor.text().contains('\n') {
                app.editor.move_up();
            } else {
                app.editor.history_up();
            }
        }
        KeyCode::Down => {
            if app.has_autocomplete() {
                app.autocomplete_down();
            } else if app.editor.text().contains('\n') {
                app.editor.move_down();
            } else {
                app.editor.history_down();
            }
        }
        KeyCode::Home => app.editor.move_home(),
        KeyCode::End => app.editor.move_end(),
        KeyCode::Char(ch)
            if key.modifiers.contains(KeyModifiers::CONTROL) && ch == 'w' =>
        {
            app.editor.delete_word_backward();
            edited = true;
        }
        KeyCode::Char(ch)
            if key.modifiers.contains(KeyModifiers::CONTROL) && ch == 'u' =>
        {
            app.editor.delete_to_line_start();
            edited = true;
        }
        KeyCode::Char(ch)
            if key.modifiers.contains(KeyModifiers::CONTROL) && ch == 'k' =>
        {
            app.editor.delete_to_line_end();
            edited = true;
        }
        KeyCode::Char(ch)
            if key.modifiers.contains(KeyModifiers::CONTROL) && ch == 'y' =>
        {
            app.editor.yank();
            edited = true;
        }
        KeyCode::Char(ch)
            if key.modifiers.contains(KeyModifiers::ALT) && ch == 'y' =>
        {
            app.editor.yank_pop();
            edited = true;
        }
        KeyCode::Char(ch)
            if key.modifiers.contains(KeyModifiers::CONTROL) && ch == '-' =>
        {
            app.editor.undo();
            edited = true;
        }
        KeyCode::Char(ch)
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT) =>
        {
            app.editor.insert_char(ch);
            edited = true;
        }
        _ => {}
    }

    if edited {
        app.refresh_autocomplete(false);
    }

    Ok(())
}

fn submit_input(
    app: &mut AppState,
    outbound_tx: &mpsc::UnboundedSender<ClientEvent>,
) -> Result<(), TuiClientError> {
    let submission = app.editor.submit();
    let line = submission.raw;
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

    if trimmed == "/clear" {
        app.log_lines.clear();
        app.active_assistant_line = None;
        app.push_system("cleared transcript".to_string());
        return Ok(());
    }

    app.push_user(line.clone());
    send_outbound(
        outbound_tx,
        ClientEvent::UserMessage(UserMessage {
            session_id: app.session_id.clone(),
            message: Message::new(MessageRole::User, submission.expanded),
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

}
