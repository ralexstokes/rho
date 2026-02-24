mod app;
mod input;
mod network;
mod render;

use std::time::Duration;

use crossterm::event::EventStream;
use futures_util::StreamExt;
use rho_core::protocol::{ClientEvent, StartSession};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Error as WsError};

use crate::terminal::ProcessTerminal;

use self::{
    app::AppState,
    input::handle_terminal_event,
    network::{run_reader, run_writer, send_outbound, wait_for_session_ack},
    render::{RenderState, draw_ui},
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
        let mut needs_redraw = true;

        while !app.should_quit {
            if needs_redraw {
                render_state.prepare_draw(&mut terminal, app.estimated_rendered_lines())?;
                if let Err(err) = terminal
                    .terminal_mut()
                    .draw(|frame| draw_ui(frame, &mut app))
                {
                    let _ = render_state.finish_draw(&mut terminal);
                    return Err(TuiClientError::Io(err));
                }
                render_state.finish_draw(&mut terminal)?;
            }

            tokio::select! {
                maybe_inbound = inbound_rx.recv() => {
                    match maybe_inbound {
                        Some(event) => {
                            needs_redraw = app.handle_network_event(event);
                        }
                        None => {
                            app.push_system("connection closed".to_string());
                            app.should_quit = true;
                            needs_redraw = true;
                        }
                    }
                }
                maybe_event = events.next() => {
                    match maybe_event {
                        Some(Ok(event)) => {
                            needs_redraw = handle_terminal_event(event, &mut app, &outbound_tx)?;
                        }
                        Some(Err(err)) => return Err(TuiClientError::Io(err)),
                        None => {
                            app.should_quit = true;
                            needs_redraw = true;
                        }
                    }
                }
                _ = tick.tick(), if app.should_animate() => {
                    app.frame_tick = app.frame_tick.wrapping_add(1);
                    needs_redraw = true;
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
