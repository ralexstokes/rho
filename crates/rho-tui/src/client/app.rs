use std::collections::HashMap;

use crate::{
    autocomplete::{AutocompleteItem, AutocompleteProvider, CombinedAutocompleteProvider},
    editor::EditorState,
    overlay::{OverlayAnchor, OverlayMargin, OverlayOptions, OverlayStack, SizeValue},
    select_list::SelectList,
    theme::UiTheme,
    widgets::{TextBlock, render_markdown},
};
use ratatui::text::{Line, Span};
use rho_core::protocol::{ErrorEvent, ServerEvent};

use super::network::NetworkEvent;

#[derive(Debug, Clone)]
pub(super) enum LogKind {
    User,
    Assistant,
    Tool,
    System,
    Error,
}

#[derive(Debug, Clone)]
pub(super) struct LogLine {
    pub(super) kind: LogKind,
    pub(super) text: String,
    pub(super) tool_call_id: Option<String>,
    pub(super) tool_name: Option<String>,
    pub(super) tool_running: bool,
}

#[derive(Debug)]
pub(super) struct AppState {
    pub(super) url: String,
    pub(super) session_id: String,
    pub(super) theme: UiTheme,
    pub(super) log_lines: Vec<LogLine>,
    pub(super) active_assistant_line: Option<usize>,
    pub(super) editor: EditorState,
    pub(super) autocomplete_provider: CombinedAutocompleteProvider,
    pub(super) autocomplete_list: Option<SelectList>,
    pub(super) autocomplete_prefix: String,
    pub(super) overlays: OverlayStack,
    pub(super) autocomplete_overlay_id: Option<u64>,
    pub(super) help_overlay_id: Option<u64>,
    pub(super) tool_call_names: HashMap<String, String>,
    pub(super) collapse_tool_calls: bool,
    pub(super) awaiting_assistant: bool,
    pub(super) transcript_scroll_up: usize,
    pub(super) transcript_viewport_height: usize,
    pub(super) transcript_cache: TranscriptRenderCache,
    pub(super) frame_tick: u64,
    pub(super) should_quit: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum TranscriptRenderMode {
    Plain,
    Markdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TranscriptRenderKey {
    width: u16,
    mode: TranscriptRenderMode,
    text: String,
}

#[derive(Debug, Clone, Default)]
pub(super) struct TranscriptRenderCache {
    entries: HashMap<TranscriptRenderKey, Vec<Line<'static>>>,
}

impl TranscriptRenderCache {
    const MAX_ENTRIES: usize = 512;

    pub(super) fn render_lines(
        &mut self,
        text: &str,
        width: u16,
        mode: TranscriptRenderMode,
        theme: &UiTheme,
        text_block: &TextBlock,
    ) -> Vec<Line<'static>> {
        let key = TranscriptRenderKey {
            width,
            mode: mode.clone(),
            text: text.to_string(),
        };

        if let Some(lines) = self.entries.get(&key) {
            return lines.clone();
        }

        let lines = match mode {
            TranscriptRenderMode::Plain => text_block.render_lines(text, width),
            TranscriptRenderMode::Markdown => render_markdown(text, width, theme),
        };

        if self.entries.len() >= Self::MAX_ENTRIES {
            let mut keep = false;
            self.entries.retain(|_, _| {
                keep = !keep;
                keep
            });
        }
        self.entries.insert(key, lines.clone());
        lines
    }

    pub(super) fn clear(&mut self) {
        self.entries.clear();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CollapsedToolState {
    pub(super) name: String,
    pub(super) running: bool,
}

impl AppState {
    pub(super) fn new(url: String, session_id: String) -> Self {
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
            help_overlay_id: None,
            tool_call_names: HashMap::new(),
            collapse_tool_calls: true,
            awaiting_assistant: false,
            transcript_scroll_up: 0,
            transcript_viewport_height: 0,
            transcript_cache: TranscriptRenderCache::default(),
            frame_tick: 0,
            should_quit: false,
        };
        app.push_system("connected".to_string());
        app
    }

    pub(super) fn push_line(&mut self, kind: LogKind, text: String) {
        self.log_lines.push(LogLine {
            kind,
            text,
            tool_call_id: None,
            tool_name: None,
            tool_running: false,
        });
    }

    pub(super) fn push_tool_line(
        &mut self,
        text: String,
        tool_call_id: String,
        tool_name: String,
        tool_running: bool,
    ) {
        self.log_lines.push(LogLine {
            kind: LogKind::Tool,
            text,
            tool_call_id: Some(tool_call_id),
            tool_name: Some(tool_name),
            tool_running,
        });
    }

    pub(super) fn push_system(&mut self, text: String) {
        self.push_line(LogKind::System, text);
        self.active_assistant_line = None;
    }

    pub(super) fn push_error(&mut self, text: String) {
        self.push_line(LogKind::Error, text);
        self.active_assistant_line = None;
    }

    pub(super) fn push_user(&mut self, text: String) {
        self.push_line(LogKind::User, text);
        self.active_assistant_line = None;
    }

    fn append_assistant_delta(&mut self, delta: &str) {
        if let Some(index) = self.active_assistant_line
            && let Some(line) = self.log_lines.get_mut(index)
        {
            line.text.push_str(delta);
            return;
        }

        self.log_lines.push(LogLine {
            kind: LogKind::Assistant,
            text: delta.to_string(),
            tool_call_id: None,
            tool_name: None,
            tool_running: false,
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
                tool_call_id: None,
                tool_name: None,
                tool_running: false,
            });
        }
        self.active_assistant_line = None;
    }

    pub(super) fn handle_network_event(&mut self, event: NetworkEvent) -> bool {
        match event {
            NetworkEvent::Server(server_event) => match server_event {
                ServerEvent::SessionAck(_) => false,
                ServerEvent::AssistantDelta(delta) => {
                    self.awaiting_assistant = false;
                    self.append_assistant_delta(delta.delta.as_str());
                    true
                }
                ServerEvent::ToolStarted(tool_started) => {
                    let call = tool_started.call;
                    self.tool_call_names
                        .insert(call.call_id.clone(), call.name.clone());
                    self.push_tool_line(format_tool_started(&call), call.call_id, call.name, true);
                    true
                }
                ServerEvent::ToolCompleted(tool_completed) => {
                    let result = tool_completed.result;
                    let tool_name = self
                        .tool_call_names
                        .remove(result.call_id.as_str())
                        .unwrap_or_else(|| "tool".to_string());
                    self.push_tool_line(
                        format_tool_completed(&result),
                        result.call_id,
                        tool_name,
                        false,
                    );
                    true
                }
                ServerEvent::Final(final_message) => {
                    self.awaiting_assistant = false;
                    self.finalize_assistant(final_message.message.content);
                    true
                }
                ServerEvent::Error(error) => {
                    self.awaiting_assistant = false;
                    self.push_error(format_error(&error));
                    true
                }
            },
            NetworkEvent::Closed => {
                self.awaiting_assistant = false;
                self.push_error("websocket closed unexpectedly".to_string());
                self.should_quit = true;
                true
            }
            NetworkEvent::ReceiveError(err) => {
                self.awaiting_assistant = false;
                self.push_error(err);
                self.should_quit = true;
                true
            }
            NetworkEvent::ProtocolError(err) => {
                self.awaiting_assistant = false;
                self.push_error(err);
                self.should_quit = true;
                true
            }
        }
    }

    pub(super) fn clear_autocomplete(&mut self) {
        self.autocomplete_list = None;
        self.autocomplete_prefix.clear();
        if let Some(id) = self.autocomplete_overlay_id.take() {
            let _ = self.overlays.hide(id);
        }
    }

    pub(super) fn sync_overlay_ids(&mut self) {
        if let Some(id) = self.autocomplete_overlay_id
            && !self.overlays.has(id)
        {
            self.autocomplete_overlay_id = None;
        }
        if let Some(id) = self.help_overlay_id
            && !self.overlays.has(id)
        {
            self.help_overlay_id = None;
        }
    }

    pub(super) fn has_autocomplete(&self) -> bool {
        self.autocomplete_list.is_some()
    }

    pub(super) fn refresh_autocomplete(&mut self, force_file: bool) {
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

    pub(super) fn autocomplete_up(&mut self) {
        if let Some(list) = &mut self.autocomplete_list {
            list.move_up();
        }
        self.sync_autocomplete_overlay();
    }

    pub(super) fn autocomplete_down(&mut self) {
        if let Some(list) = &mut self.autocomplete_list {
            list.move_down();
        }
        self.sync_autocomplete_overlay();
    }

    pub(super) fn apply_autocomplete_selection(&mut self) -> bool {
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

        if let Some(id) = self.autocomplete_overlay_id
            && self.overlays.update(id, lines.clone())
        {
            let _ = self.overlays.set_hidden(id, false);
            return;
        }

        let id = self.overlays.show(lines, options);
        self.autocomplete_overlay_id = Some(id);
    }

    fn scroll_state_label(&self) -> String {
        if self.transcript_scroll_up == 0 {
            "follow".to_string()
        } else {
            format!("scroll+{}", self.transcript_scroll_up)
        }
    }

    fn tool_state_label(&self) -> &'static str {
        if self.collapse_tool_calls {
            "tools=collapsed"
        } else {
            "tools=expanded"
        }
    }

    fn help_overlay_lines(&self) -> Vec<Line<'static>> {
        vec![
            Line::from(vec![Span::styled("Keybindings", self.theme.heading)]),
            Line::from("enter = send"),
            Line::from("alt+enter = newline"),
            Line::from("tab = complete"),
            Line::from("up/down = scroll transcript"),
            Line::from("ctrl+p/n = history"),
            Line::from("ctrl+b/f/a/e = move"),
            Line::from("ctrl+t = toggle tool view"),
            Line::from("esc = close overlay / quit"),
            Line::from(""),
            Line::from(format!("current {}", self.tool_state_label())),
            Line::from(format!("current {}", self.scroll_state_label())),
            Line::from("press ? to close"),
        ]
    }

    pub(super) fn toggle_help_overlay(&mut self) {
        if let Some(id) = self.help_overlay_id.take() {
            let _ = self.overlays.hide(id);
            return;
        }

        let options = OverlayOptions {
            width: Some(SizeValue::Percent(55)),
            min_width: Some(42),
            max_height: Some(SizeValue::Absolute(18)),
            anchor: OverlayAnchor::Center,
            margin: OverlayMargin {
                top: 1,
                right: 1,
                bottom: 1,
                left: 1,
            },
            min_terminal_width: Some(40),
            title: Some("help".to_string()),
            ..OverlayOptions::default()
        };

        let id = self.overlays.show(self.help_overlay_lines(), options);
        self.help_overlay_id = Some(id);
    }

    pub(super) fn sync_help_overlay(&mut self) {
        let Some(id) = self.help_overlay_id else {
            return;
        };
        if !self.overlays.update(id, self.help_overlay_lines()) {
            self.help_overlay_id = None;
        }
    }

    pub(super) fn scroll_to_bottom(&mut self) {
        self.transcript_scroll_up = 0;
    }

    pub(super) fn scroll_up_lines(&mut self, lines: usize) {
        if lines == 0 {
            return;
        }
        self.transcript_scroll_up = self.transcript_scroll_up.saturating_add(lines);
    }

    pub(super) fn scroll_down_lines(&mut self, lines: usize) {
        if lines == 0 {
            return;
        }
        self.transcript_scroll_up = self.transcript_scroll_up.saturating_sub(lines);
    }

    pub(super) fn estimated_rendered_lines(&self) -> usize {
        let history = self.log_lines.len();
        let editor = self.editor.lines().len().max(1);
        history + editor + 3
    }

    pub(super) fn should_animate(&self) -> bool {
        if self.awaiting_assistant || self.active_assistant_line.is_some() {
            return true;
        }

        self.collapse_tool_calls
            && self
                .log_lines
                .iter()
                .any(|entry| matches!(entry.kind, LogKind::Tool) && entry.tool_running)
    }
}

pub(super) fn format_tool_started(call: &rho_core::ToolCall) -> String {
    format!(
        "started call_id={} name={} input={}",
        call.call_id, call.name, call.input
    )
}

pub(super) fn format_tool_completed(result: &rho_core::ToolResult) -> String {
    let status = if result.is_error { "error" } else { "ok" };
    format!(
        "completed call_id={} status={} output={}",
        result.call_id, status, result.output
    )
}

pub(super) fn format_error(error: &ErrorEvent) -> String {
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
    use rho_core::{
        protocol::{AssistantDelta, ErrorEvent, ServerEvent},
        tool::{ToolCall, ToolResult},
    };
    use serde_json::json;

    use super::*;
    use crate::widgets::TextBlock;

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
    fn first_assistant_delta_clears_optimistic_spinner_state() {
        let mut app = AppState::new(
            "ws://localhost:8787/ws".to_string(),
            "session-1".to_string(),
        );
        app.awaiting_assistant = true;

        app.handle_network_event(NetworkEvent::Server(ServerEvent::AssistantDelta(
            AssistantDelta {
                session_id: app.session_id.clone(),
                delta: "hi".to_string(),
            },
        )));

        assert!(!app.awaiting_assistant);
        assert!(app.active_assistant_line.is_some());
    }

    #[test]
    fn app_should_animate_for_assistant_spinner() {
        let mut app = AppState::new(
            "ws://localhost:8787/ws".to_string(),
            "session-1".to_string(),
        );
        app.awaiting_assistant = true;

        assert!(app.should_animate());
    }

    #[test]
    fn app_should_animate_for_running_collapsed_tool() {
        let mut app = AppState::new(
            "ws://localhost:8787/ws".to_string(),
            "session-1".to_string(),
        );
        app.push_tool_line(
            "started".to_string(),
            "call-1".to_string(),
            "bash".to_string(),
            true,
        );

        assert!(app.should_animate());
    }

    #[test]
    fn app_should_not_animate_for_running_expanded_tool() {
        let mut app = AppState::new(
            "ws://localhost:8787/ws".to_string(),
            "session-1".to_string(),
        );
        app.collapse_tool_calls = false;
        app.push_tool_line(
            "started".to_string(),
            "call-1".to_string(),
            "bash".to_string(),
            true,
        );

        assert!(!app.should_animate());
    }

    #[test]
    fn transcript_cache_reuses_rendered_entry() {
        let mut cache = TranscriptRenderCache::default();
        let theme = UiTheme::default();
        let text_block = TextBlock::new(0, 0);
        let width = 20;

        let _ = cache.render_lines(
            "hello world",
            width,
            TranscriptRenderMode::Plain,
            &theme,
            &text_block,
        );
        assert_eq!(cache.entries.len(), 1);

        let _ = cache.render_lines(
            "hello world",
            width,
            TranscriptRenderMode::Plain,
            &theme,
            &text_block,
        );
        assert_eq!(cache.entries.len(), 1);
    }
}
