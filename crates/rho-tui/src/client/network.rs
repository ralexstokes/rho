use futures_util::{SinkExt, StreamExt};
use rho_core::protocol::{ClientEnvelope, ClientEvent, ServerEnvelope, ServerEvent};
use tokio::sync::mpsc;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message as WsMessage};

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
    while let Some(event) = outbound_rx.recv().await {
        send_client_event(&mut writer, event).await?;
    }
    Ok(())
}

pub(super) async fn run_reader(
    mut reader: WsReader,
    inbound_tx: mpsc::UnboundedSender<NetworkEvent>,
) -> Result<(), TuiClientError> {
    loop {
        match recv_server_envelope(&mut reader).await {
            Ok(envelope) => {
                if inbound_tx
                    .send(NetworkEvent::Server(envelope.event))
                    .is_err()
                {
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

pub(super) fn send_outbound(
    outbound_tx: &mpsc::UnboundedSender<ClientEvent>,
    event: ClientEvent,
) -> Result<(), TuiClientError> {
    outbound_tx
        .send(event)
        .map_err(|_| TuiClientError::OutboundChannelClosed)
}

async fn send_client_event(
    writer: &mut WsWriter,
    event: ClientEvent,
) -> Result<(), TuiClientError> {
    let text =
        serde_json::to_string(&ClientEnvelope::new(event)).map_err(TuiClientError::Protocol)?;
    writer
        .send(WsMessage::Text(text.into()))
        .await
        .map_err(TuiClientError::Send)
}

pub(super) async fn wait_for_session_ack(
    inbound_rx: &mut mpsc::UnboundedReceiver<NetworkEvent>,
) -> Result<String, TuiClientError> {
    while let Some(network_event) = inbound_rx.recv().await {
        match network_event {
            NetworkEvent::Server(server_event) => match server_event {
                ServerEvent::SessionAck(ack) => return Ok(ack.session_id),
                ServerEvent::Error(error) => {
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
            return Err(TuiClientError::Closed);
        };

        let message = next_message.map_err(TuiClientError::Receive)?;
        match message {
            WsMessage::Text(text) => {
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
