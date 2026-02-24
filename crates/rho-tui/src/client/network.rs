use futures_util::{SinkExt, StreamExt};
use rho_core::protocol::{ClientEnvelope, ClientEvent, ServerEnvelope, ServerEvent};
use tokio::sync::mpsc;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message as WsMessage};
use tracing::{Instrument, debug, info, info_span, trace, warn};

use super::TuiClientError;

pub(super) type WsWriter = futures_util::stream::SplitSink<
    WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    WsMessage,
>;
pub(super) type WsReader =
    futures_util::stream::SplitStream<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>>;

#[derive(Debug)]
pub(super) enum NetworkEvent {
    Server(ServerEvent),
    Closed,
    ReceiveError(String),
    ProtocolError(String),
}

pub(super) async fn run_writer(
    mut writer: WsWriter,
    mut outbound_rx: mpsc::UnboundedReceiver<ClientEvent>,
) -> Result<(), TuiClientError> {
    async {
        info!("writer task started");
        while let Some(event) = outbound_rx.recv().await {
            debug!(
                event = client_event_name(&event),
                "sending outbound client event"
            );
            send_client_event(&mut writer, event).await?;
        }
        info!("writer task stopped");
        Ok(())
    }
    .instrument(info_span!("tui.writer"))
    .await
}

pub(super) async fn run_reader(
    mut reader: WsReader,
    inbound_tx: mpsc::UnboundedSender<NetworkEvent>,
) -> Result<(), TuiClientError> {
    async {
        info!("reader task started");
        loop {
            match recv_server_envelope(&mut reader).await {
                Ok(envelope) => {
                    debug!(
                        event = server_event_name(&envelope.event),
                        "received server event"
                    );
                    if inbound_tx
                        .send(NetworkEvent::Server(envelope.event))
                        .is_err()
                    {
                        info!("inbound receiver dropped; stopping reader task");
                        return Ok(());
                    }
                }
                Err(TuiClientError::Closed) => {
                    info!("websocket reader closed");
                    let _ = inbound_tx.send(NetworkEvent::Closed);
                    return Ok(());
                }
                Err(TuiClientError::Receive(err)) => {
                    warn!(%err, "websocket receive error");
                    let _ = inbound_tx.send(NetworkEvent::ReceiveError(err.to_string()));
                    return Ok(());
                }
                Err(TuiClientError::Protocol(err)) => {
                    warn!(%err, "websocket protocol decode error");
                    let _ = inbound_tx.send(NetworkEvent::ProtocolError(err.to_string()));
                    return Ok(());
                }
                Err(other) => {
                    warn!(%other, "websocket reader failed");
                    let _ = inbound_tx.send(NetworkEvent::ReceiveError(other.to_string()));
                    return Ok(());
                }
            }
        }
    }
    .instrument(info_span!("tui.reader"))
    .await
}

pub(super) fn send_outbound(
    outbound_tx: &mpsc::UnboundedSender<ClientEvent>,
    event: ClientEvent,
) -> Result<(), TuiClientError> {
    trace!(event = client_event_name(&event), "queueing outbound event");
    outbound_tx
        .send(event)
        .map_err(|_| TuiClientError::OutboundChannelClosed)
}

async fn send_client_event(
    writer: &mut WsWriter,
    event: ClientEvent,
) -> Result<(), TuiClientError> {
    let event_name = client_event_name(&event);
    let text =
        serde_json::to_string(&ClientEnvelope::new(event)).map_err(TuiClientError::Protocol)?;
    trace!(
        event = event_name,
        payload_chars = text.chars().count(),
        "writing websocket client event"
    );
    writer
        .send(WsMessage::Text(text.into()))
        .await
        .map_err(TuiClientError::Send)
}

pub(super) async fn wait_for_session_ack(
    inbound_rx: &mut mpsc::UnboundedReceiver<NetworkEvent>,
) -> Result<String, TuiClientError> {
    debug!("waiting for session ack");
    while let Some(network_event) = inbound_rx.recv().await {
        match network_event {
            NetworkEvent::Server(server_event) => match server_event {
                ServerEvent::SessionAck(ack) => {
                    info!(session_id = %ack.session_id, "received session ack");
                    return Ok(ack.session_id);
                }
                ServerEvent::Error(error) => {
                    warn!(
                        session_id = ?error.session_id,
                        code = %error.code,
                        "session initialization returned error"
                    );
                    return Err(TuiClientError::SessionInitialization(
                        super::app::format_error(&error),
                    ));
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
            debug!("websocket stream ended while waiting for server envelope");
            return Err(TuiClientError::Closed);
        };

        let message = next_message.map_err(TuiClientError::Receive)?;
        match message {
            WsMessage::Text(text) => {
                trace!(
                    payload_chars = text.chars().count(),
                    "received websocket server text frame"
                );
                return serde_json::from_str(text.as_str()).map_err(TuiClientError::Protocol);
            }
            WsMessage::Binary(_)
            | WsMessage::Ping(_)
            | WsMessage::Pong(_)
            | WsMessage::Frame(_) => {}
            WsMessage::Close(_) => return Err(TuiClientError::Closed),
        }
    }
}

pub(super) fn client_event_name(event: &ClientEvent) -> &'static str {
    match event {
        ClientEvent::StartSession(_) => "start_session",
        ClientEvent::UserMessage(_) => "user_message",
        ClientEvent::Cancel(_) => "cancel",
    }
}

fn server_event_name(event: &ServerEvent) -> &'static str {
    match event {
        ServerEvent::SessionAck(_) => "session_ack",
        ServerEvent::AssistantDelta(_) => "assistant_delta",
        ServerEvent::ToolStarted(_) => "tool_started",
        ServerEvent::ToolCompleted(_) => "tool_completed",
        ServerEvent::Final(_) => "final",
        ServerEvent::Error(_) => "error",
    }
}

pub(super) fn network_event_name(event: &NetworkEvent) -> &'static str {
    match event {
        NetworkEvent::Server(server_event) => server_event_name(server_event),
        NetworkEvent::Closed => "closed",
        NetworkEvent::ReceiveError(_) => "receive_error",
        NetworkEvent::ProtocolError(_) => "protocol_error",
    }
}
