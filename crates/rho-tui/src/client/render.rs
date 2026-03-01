use std::collections::{HashMap, HashSet};

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
};

use crate::{
    terminal::ProcessTerminal,
    widgets::{loader_frame, section_block, truncate_to_width},
};

use super::{
    TuiClientError,
    app::{AppState, CollapsedToolState, LogKind, LogLine, TranscriptRenderMode},
};

#[derive(Debug, Clone)]
pub(super) struct RenderState {
    previous_width: Option<u16>,
    max_rendered_lines: usize,
    clear_on_shrink: bool,
    sync_output: bool,
}

impl RenderState {
    pub(super) fn from_env() -> Self {
        Self {
            previous_width: None,
            max_rendered_lines: 0,
            clear_on_shrink: std::env::var("RHO_CLEAR_ON_SHRINK").is_ok_and(|v| v == "1"),
            sync_output: std::env::var("RHO_SYNC_OUTPUT").is_ok_and(|v| v == "1"),
        }
    }

    pub(super) fn prepare_draw(
        &mut self,
        terminal: &mut ProcessTerminal,
        estimated_lines: usize,
    ) -> Result<(), TuiClientError> {
        let width = terminal.width().map_err(TuiClientError::Io)?;
        if self
            .previous_width
            .is_some_and(|previous| previous != width)
        {
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

    pub(super) fn finish_draw(&self, terminal: &mut ProcessTerminal) -> Result<(), TuiClientError> {
        if self.sync_output {
            terminal
                .end_synchronized_output()
                .map_err(TuiClientError::Io)?;
        }
        Ok(())
    }
}

pub(super) fn collapsed_tool_states(log_lines: &[LogLine]) -> HashMap<String, CollapsedToolState> {
    let mut states: HashMap<String, CollapsedToolState> = HashMap::new();
    for entry in log_lines {
        if !matches!(entry.kind, LogKind::Tool) {
            continue;
        }
        let Some(call_id) = entry.tool_call_id.as_ref() else {
            continue;
        };

        let tool_name = entry
            .tool_name
            .clone()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| "tool".to_string());

        if let Some(state) = states.get_mut(call_id) {
            state.running &= entry.tool_running;
            if state.name == "tool" && tool_name != "tool" {
                state.name = tool_name;
            }
        } else {
            states.insert(
                call_id.clone(),
                CollapsedToolState {
                    name: tool_name,
                    running: entry.tool_running,
                },
            );
        }
    }

    states
}

pub(super) fn draw_ui(frame: &mut Frame<'_>, app: &mut AppState) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(frame.area());

    let flow_area = layout[0];
    let footer_area = layout[1];

    let flow_block = section_block("rho", app.theme.history_border);
    let flow_inner = flow_block.inner(flow_area);
    let flow_width = flow_inner.width.max(1);
    let mut flow_lines = Vec::new();
    let tool_states = collapsed_tool_states(app.log_lines.as_slice());
    let mut rendered_collapsed_calls = HashSet::new();

    for entry in &app.log_lines {
        if !app.show_system_messages && matches!(entry.kind, LogKind::System) {
            continue;
        }
        if app.collapse_tool_calls && matches!(entry.kind, LogKind::Tool) {
            let Some(call_id) = entry.tool_call_id.as_ref() else {
                continue;
            };
            if !rendered_collapsed_calls.insert(call_id.clone()) {
                continue;
            }
            let state = tool_states.get(call_id);
            let tool_name = state
                .map(|state| state.name.as_str())
                .or(entry.tool_name.as_deref())
                .unwrap_or("tool");
            let mut spans = vec![Span::styled("tool> ", app.theme.tool_prefix)];
            if state.is_some_and(|state| state.running) {
                spans.push(Span::styled(
                    format!("{} ", loader_frame(app.frame_tick)),
                    app.theme.tool_prefix,
                ));
            }
            spans.push(Span::styled(tool_name.to_string(), app.theme.body));
            flow_lines.push(Line::from(spans));
            continue;
        }

        let (prefix, prefix_style, markdown_mode) = match entry.kind {
            LogKind::User => ("you> ", app.theme.user_prefix, true),
            LogKind::Assistant => ("assistant> ", app.theme.assistant_prefix, true),
            LogKind::Tool => ("tool> ", app.theme.tool_prefix, false),
            LogKind::System => ("system> ", app.theme.system_prefix, false),
            LogKind::Error => ("error> ", app.theme.error_prefix, false),
        };

        let prefix_width = u16::try_from(prefix.chars().count()).unwrap_or(u16::MAX);
        let body_width = flow_width.saturating_sub(prefix_width).max(1);
        let render_mode = if markdown_mode {
            TranscriptRenderMode::Markdown
        } else {
            TranscriptRenderMode::Plain
        };
        let body_lines = app.transcript_cache.render_lines(
            entry.text.as_str(),
            body_width,
            render_mode,
            &app.theme,
        );

        let body_style_override = match entry.kind {
            LogKind::System => Some(app.theme.system_body),
            _ => None,
        };

        let indent = " ".repeat(prefix.chars().count());
        for (index, line) in body_lines.into_iter().enumerate() {
            let mut spans = Vec::new();
            if index == 0 {
                spans.push(Span::styled(prefix.to_string(), prefix_style));
            } else {
                spans.push(Span::styled(indent.clone(), prefix_style));
            }
            if let Some(style) = body_style_override {
                spans.extend(
                    line.spans
                        .into_iter()
                        .map(|s| Span::styled(s.content, style)),
                );
            } else {
                spans.extend(line.spans);
            }
            flow_lines.push(Line::from(spans));
        }
    }

    if app.awaiting_assistant || app.active_assistant_line.is_some() {
        flow_lines.push(Line::from(String::new()));
        flow_lines.push(Line::from(vec![
            Span::styled("assistant> ", app.theme.assistant_prefix),
            Span::styled(
                format!("{} thinking...", loader_frame(app.frame_tick)),
                app.theme.loader,
            ),
        ]));
    }

    if !flow_lines.is_empty() {
        flow_lines.push(Line::from(String::new()));
    }

    let prompt = "you> ";
    let prompt_width = prompt.chars().count();
    let prompt_indent = " ".repeat(prompt_width);
    let editor_width = usize::from(
        flow_width
            .saturating_sub(u16::try_from(prompt_width).unwrap_or(1))
            .max(1),
    );
    let editor_height = app.editor.lines().len().max(1);
    let editor_render = app.editor.render(editor_width, editor_height);
    let editor_start_row = flow_lines.len();

    for (index, line) in editor_render.lines.iter().enumerate() {
        let prefix = if index == 0 {
            prompt.to_string()
        } else {
            prompt_indent.clone()
        };
        flow_lines.push(Line::from(vec![
            Span::styled(prefix, app.theme.user_prefix),
            Span::styled(line.clone(), app.theme.body),
        ]));
    }

    let cursor_flow_row = editor_start_row.saturating_add(editor_render.cursor_row);
    let cursor_flow_col = prompt_width.saturating_add(editor_render.cursor_col);

    let viewport_height = usize::from(flow_inner.height);
    app.transcript_viewport_height = viewport_height;
    let max_scroll_up = flow_lines.len().saturating_sub(viewport_height);
    app.transcript_scroll_up = app.transcript_scroll_up.min(max_scroll_up);
    let scroll_top = max_scroll_up.saturating_sub(app.transcript_scroll_up);
    let scroll_top = u16::try_from(scroll_top).unwrap_or(u16::MAX);

    let flow = Paragraph::new(flow_lines)
        .block(flow_block)
        .scroll((scroll_top, 0));
    frame.render_widget(Clear, flow_inner);
    frame.render_widget(flow, flow_area);

    let footer_left = if app.quit_pending {
        "press ctrl+c again to exit"
    } else if app.request_in_flight {
        "cancel with esc"
    } else {
        ""
    };
    let footer_right = "?=help  ctrl+s=status";
    let width = usize::from(footer_area.width);
    let left_len = footer_left.len();
    let right_len = footer_right.len();
    let padding = width.saturating_sub(left_len).saturating_sub(right_len);
    let footer_text = format!(
        "{}{:padding$}{}",
        footer_left,
        "",
        footer_right,
        padding = padding
    );
    let status =
        Paragraph::new(truncate_to_width(footer_text.as_str(), width)).style(app.theme.footer);
    frame.render_widget(status, footer_area);

    let cursor_visible = cursor_flow_row >= usize::from(scroll_top)
        && cursor_flow_row < usize::from(scroll_top).saturating_add(viewport_height);
    let fallback_x = flow_inner.x;
    let fallback_y = flow_inner
        .y
        .saturating_add(flow_inner.height.saturating_sub(1));
    let mut cursor_x = fallback_x;
    let mut cursor_y = fallback_y;
    if flow_inner.width > 0 && flow_inner.height > 0 && cursor_visible {
        let visible_row = cursor_flow_row.saturating_sub(usize::from(scroll_top));
        let visible_col = cursor_flow_col.min(usize::from(flow_inner.width.saturating_sub(1)));
        cursor_x = flow_inner
            .x
            .saturating_add(u16::try_from(visible_col).unwrap_or(u16::MAX));
        cursor_y = flow_inner
            .y
            .saturating_add(u16::try_from(visible_row).unwrap_or(u16::MAX));
    }

    app.sync_help_overlay();
    app.sync_status_overlay();
    app.overlays.render(frame, frame.area());
    frame.set_cursor_position((cursor_x, cursor_y));
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    #[test]
    fn collapsed_tool_states_stop_running_after_completion() {
        let lines = vec![
            LogLine {
                kind: LogKind::Tool,
                text: "started".to_string(),
                tool_call_id: Some("call-1".to_string()),
                tool_name: Some("bash".to_string()),
                tool_running: true,
            },
            LogLine {
                kind: LogKind::Tool,
                text: "completed".to_string(),
                tool_call_id: Some("call-1".to_string()),
                tool_name: Some("bash".to_string()),
                tool_running: false,
            },
        ];

        let states = collapsed_tool_states(lines.as_slice());
        assert_eq!(
            states.get("call-1"),
            Some(&CollapsedToolState {
                name: "bash".to_string(),
                running: false
            })
        );
    }

    #[test]
    fn collapsed_tool_states_keep_running_without_completion() {
        let lines = vec![LogLine {
            kind: LogKind::Tool,
            text: "started".to_string(),
            tool_call_id: Some("call-2".to_string()),
            tool_name: Some("read".to_string()),
            tool_running: true,
        }];

        let states = collapsed_tool_states(lines.as_slice());
        assert_eq!(
            states.get("call-2"),
            Some(&CollapsedToolState {
                name: "read".to_string(),
                running: true
            })
        );
    }

    #[test]
    fn scrolling_up_with_blank_line_above_heading_clears_stale_cell() {
        let backend = TestBackend::new(32, 5);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut app = AppState::new("in-process://rho".to_string(), "session-1".to_string());
        app.log_lines.clear();
        app.push_line(LogKind::Assistant, "\n# 123456789012345678)".to_string());
        app.scroll_up_lines(1);

        terminal
            .draw(|frame| draw_ui(frame, &mut app))
            .expect("first draw");

        let heading_y = 1u16;
        let heading_x = {
            let buffer = terminal.backend().buffer();
            (0..buffer.area.width)
                .find(|&x| buffer[(x, heading_y)].symbol() == ")")
                .expect("closing heading character should be visible")
        };

        app.scroll_up_lines(1);
        terminal
            .draw(|frame| draw_ui(frame, &mut app))
            .expect("second draw");

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(heading_x, heading_y)].symbol(), " ");
    }
}
