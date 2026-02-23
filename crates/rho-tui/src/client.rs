use std::io::{Write, stdout};

use futures_util::{SinkExt, StreamExt};
use rho_core::{
    Message, MessageRole,
    protocol::{
        ClientEnvelope, ClientEvent, ErrorEvent, ServerEnvelope, ServerEvent, StartSession,
        UserMessage,
    },
};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, BufReader, stdin};
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
        let (mut writer, mut reader) = socket.split();

        send_client_event(
            &mut writer,
            ClientEvent::StartSession(StartSession { session_id: None }),
        )
        .await?;

        let session_id = wait_for_session_ack(&mut reader).await?;
        println!("connected: session_id={session_id}");
        println!("type a prompt and press Enter (`/quit` to exit, `/cancel` to cancel in-flight)");

        let mut input = BufReader::new(stdin()).lines();
        loop {
            print!("> ");
            stdout().flush().map_err(TuiClientError::Io)?;

            let Some(line) = input.next_line().await.map_err(TuiClientError::Io)? else {
                break;
            };

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            if trimmed == "/quit" {
                break;
            }

            if trimmed == "/cancel" {
                send_client_event(
                    &mut writer,
                    ClientEvent::Cancel(rho_core::protocol::CancelRequest {
                        session_id: session_id.clone(),
                    }),
                )
                .await?;
                continue;
            }

            send_client_event(
                &mut writer,
                ClientEvent::UserMessage(UserMessage {
                    session_id: session_id.clone(),
                    message: Message::new(MessageRole::User, line),
                }),
            )
            .await?;

            let mut streaming_line_open = false;
            loop {
                let envelope = recv_server_envelope(&mut reader).await?;
                match envelope.event {
                    ServerEvent::AssistantDelta(delta) => {
                        streaming_line_open = true;
                        print!("{}", delta.delta);
                        stdout().flush().map_err(TuiClientError::Io)?;
                    }
                    ServerEvent::ToolStarted(tool_started) => {
                        close_streaming_line(&mut streaming_line_open);
                        println!("[tool:start] {}", format_tool_started(&tool_started.call));
                    }
                    ServerEvent::ToolCompleted(tool_completed) => {
                        close_streaming_line(&mut streaming_line_open);
                        println!(
                            "[tool:done] {}",
                            format_tool_completed(&tool_completed.result)
                        );
                    }
                    ServerEvent::Final(final_message) => {
                        if streaming_line_open {
                            close_streaming_line(&mut streaming_line_open);
                        } else {
                            println!("{}", final_message.message.content);
                        }
                        break;
                    }
                    ServerEvent::Error(error) => {
                        close_streaming_line(&mut streaming_line_open);
                        println!("{}", format_error(&error));
                        break;
                    }
                    ServerEvent::SessionAck(_) => {}
                }
            }
        }

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
    Io(#[source] std::io::Error),
    #[error("session initialization failed: {0}")]
    SessionInitialization(String),
}

async fn send_client_event(
    writer: &mut futures_util::stream::SplitSink<
        WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
        WsMessage,
    >,
    event: ClientEvent,
) -> Result<(), TuiClientError> {
    let text =
        serde_json::to_string(&ClientEnvelope::new(event)).map_err(TuiClientError::Protocol)?;
    writer
        .send(WsMessage::Text(text.into()))
        .await
        .map_err(TuiClientError::Send)
}

async fn wait_for_session_ack(
    reader: &mut futures_util::stream::SplitStream<
        WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    >,
) -> Result<String, TuiClientError> {
    loop {
        let envelope = recv_server_envelope(reader).await?;
        match envelope.event {
            ServerEvent::SessionAck(ack) => return Ok(ack.session_id),
            ServerEvent::Error(error) => {
                return Err(TuiClientError::SessionInitialization(format_error(&error)));
            }
            _ => {}
        }
    }
}

async fn recv_server_envelope(
    reader: &mut futures_util::stream::SplitStream<
        WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    >,
) -> Result<ServerEnvelope, TuiClientError> {
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

fn close_streaming_line(streaming_line_open: &mut bool) {
    if *streaming_line_open {
        println!();
        *streaming_line_open = false;
    }
}

fn format_tool_started(call: &rho_core::ToolCall) -> String {
    format!(
        "call_id={} name={} input={}",
        call.call_id, call.name, call.input
    )
}

fn format_tool_completed(result: &rho_core::ToolResult) -> String {
    let status = if result.is_error { "error" } else { "ok" };
    format!(
        "call_id={} status={} output={}",
        result.call_id, status, result.output
    )
}

fn format_error(error: &ErrorEvent) -> String {
    match &error.session_id {
        Some(session_id) => format!(
            "[error] session_id={} code={} message={}",
            session_id, error.code, error.message
        ),
        None => format!("[error] code={} message={}", error.code, error.message),
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
            "[error] session_id=session-z code=runtime_error message=boom"
        );
    }
}
