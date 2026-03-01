use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use rho_core::{
    Message, MessageRole,
    protocol::{ClientEvent, UserMessage},
};
use tokio::sync::mpsc;

use crate::keys::{Key, is_key_release, is_key_repeat, matches_key};

use super::{TuiClientError, app::AppState, network::send_outbound};

pub(super) fn handle_terminal_event(
    event: Event,
    app: &mut AppState,
    outbound_tx: &mpsc::UnboundedSender<ClientEvent>,
) -> Result<bool, TuiClientError> {
    match event {
        Event::Key(key) => handle_key_event(key, app, outbound_tx),
        Event::Paste(text) => {
            app.editor.handle_paste(text);
            app.scroll_to_bottom();
            app.refresh_autocomplete(false);
            Ok(true)
        }
        Event::Mouse(mouse) => handle_mouse_event(mouse, app),
        Event::Resize(_, _) => Ok(true),
        _ => Ok(false),
    }
}

fn handle_mouse_event(mouse: MouseEvent, app: &mut AppState) -> Result<bool, TuiClientError> {
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            app.scroll_up_lines(3);
            Ok(true)
        }
        MouseEventKind::ScrollDown => {
            app.scroll_down_lines(3);
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn handle_key_event(
    key: KeyEvent,
    app: &mut AppState,
    outbound_tx: &mpsc::UnboundedSender<ClientEvent>,
) -> Result<bool, TuiClientError> {
    if is_key_release(&key) {
        return Ok(false);
    }
    if is_key_repeat(&key) && matches_key(&key, Key::TAB) {
        return Ok(false);
    }

    if matches_key(&key, Key::CTRL_C) {
        app.should_quit = true;
        return Ok(true);
    }
    if matches!(
        key.code,
        KeyCode::Char('?') if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    ) && app.editor.text().is_empty()
    {
        app.clear_autocomplete();
        app.toggle_help_overlay();
        return Ok(true);
    }
    if matches_key(&key, Key::CTRL_T) {
        app.collapse_tool_calls = !app.collapse_tool_calls;
        return Ok(true);
    }

    let mut edited = false;
    let mut state_changed = false;
    match key.code {
        KeyCode::Esc => {
            if app.has_autocomplete() {
                app.clear_autocomplete();
                state_changed = true;
            } else if app.overlays.hide_topmost() {
                app.sync_overlay_ids();
                state_changed = true;
            } else {
                app.should_quit = true;
                state_changed = true;
            }
        }
        KeyCode::Enter => {
            if app.has_autocomplete()
                && !key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
            {
                state_changed |= app.apply_autocomplete_selection();
            } else if key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL)
            {
                app.editor.insert_newline();
                edited = true;
            } else {
                submit_input(app, outbound_tx)?;
                app.clear_autocomplete();
                state_changed = true;
            }
        }
        KeyCode::Tab => {
            if app.has_autocomplete() {
                state_changed |= app.apply_autocomplete_selection();
            } else {
                app.refresh_autocomplete(true);
                state_changed = true;
            }
        }
        KeyCode::Backspace => {
            app.editor.backspace();
            edited = true;
        }
        KeyCode::Delete => {
            app.editor.delete();
            edited = true;
        }
        KeyCode::Left => {
            app.editor.move_left();
            app.scroll_to_bottom();
            state_changed = true;
        }
        KeyCode::Right => {
            app.editor.move_right();
            app.scroll_to_bottom();
            state_changed = true;
        }
        KeyCode::Up => {
            if app.has_autocomplete() {
                app.autocomplete_up();
            } else {
                app.scroll_up_lines(1);
            }
            state_changed = true;
        }
        KeyCode::Down => {
            if app.has_autocomplete() {
                app.autocomplete_down();
            } else {
                app.scroll_down_lines(1);
            }
            state_changed = true;
        }
        KeyCode::Home => {
            app.editor.move_home();
            app.scroll_to_bottom();
            state_changed = true;
        }
        KeyCode::End => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                app.scroll_to_bottom();
            } else {
                app.editor.move_end();
                app.scroll_to_bottom();
            }
            state_changed = true;
        }
        KeyCode::Char(ch)
            if key.modifiers.contains(KeyModifiers::CONTROL) && ch.eq_ignore_ascii_case(&'p') =>
        {
            if app.has_autocomplete() {
                app.autocomplete_up();
            } else if app.editor.is_multiline() {
                app.editor.move_up();
            } else {
                app.editor.history_up();
            }
            app.scroll_to_bottom();
            state_changed = true;
        }
        KeyCode::Char(ch)
            if key.modifiers.contains(KeyModifiers::CONTROL) && ch.eq_ignore_ascii_case(&'n') =>
        {
            if app.has_autocomplete() {
                app.autocomplete_down();
            } else if app.editor.is_multiline() {
                app.editor.move_down();
            } else {
                app.editor.history_down();
            }
            app.scroll_to_bottom();
            state_changed = true;
        }
        KeyCode::Char(ch)
            if key.modifiers.contains(KeyModifiers::CONTROL) && ch.eq_ignore_ascii_case(&'b') =>
        {
            app.editor.move_left();
            app.scroll_to_bottom();
            state_changed = true;
        }
        KeyCode::Char(ch)
            if key.modifiers.contains(KeyModifiers::CONTROL) && ch.eq_ignore_ascii_case(&'f') =>
        {
            app.editor.move_right();
            app.scroll_to_bottom();
            state_changed = true;
        }
        KeyCode::Char(ch)
            if key.modifiers.contains(KeyModifiers::CONTROL) && ch.eq_ignore_ascii_case(&'a') =>
        {
            app.editor.move_home();
            app.scroll_to_bottom();
            state_changed = true;
        }
        KeyCode::Char(ch)
            if key.modifiers.contains(KeyModifiers::CONTROL) && ch.eq_ignore_ascii_case(&'e') =>
        {
            app.editor.move_end();
            app.scroll_to_bottom();
            state_changed = true;
        }
        KeyCode::Char(ch) if key.modifiers.contains(KeyModifiers::CONTROL) && ch == 'w' => {
            app.editor.delete_word_backward();
            edited = true;
        }
        KeyCode::Char(ch) if key.modifiers.contains(KeyModifiers::CONTROL) && ch == 'u' => {
            app.editor.delete_to_line_start();
            edited = true;
        }
        KeyCode::Char(ch) if key.modifiers.contains(KeyModifiers::CONTROL) && ch == 'k' => {
            app.editor.delete_to_line_end();
            edited = true;
        }
        KeyCode::Char(ch) if key.modifiers.contains(KeyModifiers::CONTROL) && ch == 'y' => {
            app.editor.yank();
            edited = true;
        }
        KeyCode::Char(ch) if key.modifiers.contains(KeyModifiers::ALT) && ch == 'y' => {
            app.editor.yank_pop();
            edited = true;
        }
        KeyCode::Char(ch) if key.modifiers.contains(KeyModifiers::CONTROL) && ch == '-' => {
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
        app.scroll_to_bottom();
        app.refresh_autocomplete(false);
        state_changed = true;
    }

    Ok(state_changed)
}

fn submit_input(
    app: &mut AppState,
    outbound_tx: &mpsc::UnboundedSender<ClientEvent>,
) -> Result<(), TuiClientError> {
    app.scroll_to_bottom();
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
        app.tool_call_names.clear();
        app.transcript_cache.clear();
        app.awaiting_assistant = false;
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
    app.awaiting_assistant = true;

    Ok(())
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use super::*;

    #[test]
    fn submit_input_sets_optimistic_assistant_spinner_state() {
        let mut app = AppState::new(
            "ws://localhost:8787/ws".to_string(),
            "session-1".to_string(),
        );
        app.editor.insert_text("hello");
        let (outbound_tx, _outbound_rx) = mpsc::unbounded_channel();

        submit_input(&mut app, &outbound_tx).expect("submit should succeed");

        assert!(app.awaiting_assistant);
        assert!(app.active_assistant_line.is_none());
    }
}
