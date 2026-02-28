use std::{collections::HashMap, net::SocketAddr, sync::Arc};

use axum::{
    Router,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::IntoResponse,
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
use rho_core::{
    MessageRole,
    protocol::{
        ClientEnvelope, ClientEvent, ErrorEvent, PROTOCOL_VERSION, ServerEnvelope, ServerEvent,
        SessionAck,
    },
    providers::{
        ModelKind, Provider, ProviderKind, anthropic::AnthropicProvider, openai::OpenAiProvider,
    },
};
use thiserror::Error;
use tokio::{
    net::TcpListener,
    sync::{Mutex, mpsc},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{AgentError, AgentRuntime, AgentSession};

#[derive(Clone)]
pub struct AgentServer {
    state: AppState,
}

impl AgentServer {
    pub fn new(runtime: AgentRuntime, provider: Arc<dyn Provider>, model: ModelKind) -> Self {
        let state = AppState {
            runtime,
            provider,
            model,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        };
        Self { state }
    }

    pub async fn serve(self, bind: impl AsRef<str>) -> Result<(), AgentServerError> {
        let bind_text = bind.as_ref().to_string();
        let bind_addr = bind_text.parse::<SocketAddr>().map_err(|error| {
            AgentServerError::InvalidBindAddress {
                bind: bind_text,
                error,
            }
        })?;
        let listener = TcpListener::bind(bind_addr)
            .await
            .map_err(AgentServerError::Bind)?;
        self.serve_with_listener(listener).await
    }

    pub async fn serve_with_listener(self, listener: TcpListener) -> Result<(), AgentServerError> {
        let router = Router::new()
            .route("/ws", get(ws_handler))
            .with_state(self.state);
        axum::serve(listener, router)
            .await
            .map_err(AgentServerError::Serve)
    }
}

#[derive(Debug, Error)]
pub enum AgentServerError {
    #[error("invalid bind address `{bind}`: {error}")]
    InvalidBindAddress {
        bind: String,
        #[source]
        error: std::net::AddrParseError,
    },
    #[error("failed to bind listener: {0}")]
    Bind(#[source] std::io::Error),
    #[error("websocket server failed: {0}")]
    Serve(#[source] std::io::Error),
    #[error("unsupported provider kind `{0}`")]
    UnsupportedProviderKind(String),
    #[error("model `{model}` is not supported for provider `{provider:?}`")]
    UnsupportedModelForProvider {
        provider: ProviderKind,
        model: ModelKind,
    },
}

pub fn build_provider(kind: ProviderKind) -> Result<Arc<dyn Provider>, AgentServerError> {
    match kind {
        ProviderKind::OpenAi => Ok(Arc::new(OpenAiProvider::new())),
        ProviderKind::Anthropic => Ok(Arc::new(AnthropicProvider::new())),
        _ => Err(AgentServerError::UnsupportedProviderKind(format!(
            "{kind:?}"
        ))),
    }
}

#[derive(Clone)]
struct AppState {
    runtime: AgentRuntime,
    provider: Arc<dyn Provider>,
    model: ModelKind,
    sessions: Arc<Mutex<HashMap<String, SessionState>>>,
}

struct SessionState {
    session: Arc<Mutex<AgentSession>>,
    in_flight: Option<InFlightRequest>,
}

struct InFlightRequest {
    task: JoinHandle<()>,
    cancel: CancellationToken,
}

impl SessionState {
    fn new(session: AgentSession) -> Self {
        Self {
            session: Arc::new(Mutex::new(session)),
            in_flight: None,
        }
    }
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_websocket(socket, state))
}

async fn handle_websocket(stream: WebSocket, state: AppState) {
    let (mut sink, mut source) = stream.split();
    let (outbound_sender, mut outbound_receiver) = mpsc::unbounded_channel::<ServerEnvelope>();

    let writer_task = tokio::spawn(async move {
        while let Some(envelope) = outbound_receiver.recv().await {
            let payload = match serde_json::to_string(&envelope) {
                Ok(payload) => payload,
                Err(_) => break,
            };

            if sink.send(Message::Text(payload.into())).await.is_err() {
                break;
            }
        }
    });

    while let Some(next_message) = source.next().await {
        match next_message {
            Ok(Message::Text(text)) => {
                handle_text_message(&state, &outbound_sender, text.as_str()).await;
            }
            Ok(Message::Binary(_)) => send_error(
                &outbound_sender,
                None,
                "invalid_message",
                "binary websocket frames are not supported",
            ),
            Ok(Message::Ping(_) | Message::Pong(_)) => {}
            Ok(Message::Close(_)) => break,
            Err(error) => {
                send_error(
                    &outbound_sender,
                    None,
                    "websocket_receive_error",
                    format!("failed to receive websocket frame: {error}"),
                );
                break;
            }
        }
    }

    writer_task.abort();
}

async fn handle_text_message(
    state: &AppState,
    outbound_sender: &mpsc::UnboundedSender<ServerEnvelope>,
    text: &str,
) {
    let envelope: ClientEnvelope = match serde_json::from_str(text) {
        Ok(envelope) => envelope,
        Err(error) => {
            send_error(
                outbound_sender,
                None,
                "invalid_json",
                format!("invalid client envelope JSON: {error}"),
            );
            return;
        }
    };

    if envelope.version != PROTOCOL_VERSION {
        send_error(
            outbound_sender,
            None,
            "invalid_protocol_version",
            format!(
                "unsupported protocol version {}; expected {}",
                envelope.version, PROTOCOL_VERSION
            ),
        );
        return;
    }

    handle_client_event(state, outbound_sender, envelope.event).await;
}

async fn handle_client_event(
    state: &AppState,
    outbound_sender: &mpsc::UnboundedSender<ServerEnvelope>,
    event: ClientEvent,
) {
    match event {
        ClientEvent::StartSession(start_session) => {
            let mut sessions = state.sessions.lock().await;
            let session_id = start_session
                .session_id
                .unwrap_or_else(|| allocate_session_id(&sessions));

            if !is_valid_session_id(&session_id) {
                send_error(
                    outbound_sender,
                    Some(session_id),
                    "invalid_session_id",
                    "session_id must be a UUID",
                );
                return;
            }

            if sessions.contains_key(&session_id) {
                send_error(
                    outbound_sender,
                    Some(session_id),
                    "session_exists",
                    "session already exists",
                );
                return;
            }

            sessions.insert(
                session_id.clone(),
                SessionState::new(state.runtime.start_session(session_id.clone())),
            );
            send_event(
                outbound_sender,
                ServerEvent::SessionAck(SessionAck { session_id }),
            );
        }
        ClientEvent::UserMessage(user_message) => {
            if user_message.message.role != MessageRole::User {
                send_error(
                    outbound_sender,
                    Some(user_message.session_id),
                    "invalid_message_role",
                    "user_message payload must contain a message with role `user`",
                );
                return;
            }

            let session_id = user_message.session_id;
            if !is_valid_session_id(&session_id) {
                send_error(
                    outbound_sender,
                    Some(session_id),
                    "invalid_session_id",
                    "session_id must be a UUID",
                );
                return;
            }
            let user_content = user_message.message.content;
            let mut sessions = state.sessions.lock().await;

            let Some(session_state) =
                get_session_state_mut(&mut sessions, outbound_sender, &session_id)
            else {
                return;
            };

            if let Some(in_flight) = &session_state.in_flight
                && !in_flight.task.is_finished()
            {
                send_error(
                    outbound_sender,
                    Some(session_id),
                    "request_in_flight",
                    "session already has an in-flight request",
                );
                return;
            }

            session_state.in_flight = None;

            let runtime = state.runtime.clone();
            let model = state.model;
            let provider = Arc::clone(&state.provider);
            let session = Arc::clone(&session_state.session);
            let outbound_sender = outbound_sender.clone();
            let sessions = Arc::clone(&state.sessions);
            let cleanup_session_id = session_id.clone();
            let cancel = CancellationToken::new();
            let cancel_for_task = cancel.clone();
            let cancel_for_emit = cancel.clone();

            let task = tokio::spawn(async move {
                let run_result = {
                    let mut session_guard = session.lock().await;
                    runtime
                        .run_user_message_streaming(
                            &mut session_guard,
                            provider.as_ref(),
                            model,
                            user_content,
                            cancel_for_task,
                            |event| {
                                if !cancel_for_emit.is_cancelled() {
                                    send_event(&outbound_sender, event);
                                }
                            },
                        )
                        .await
                };

                match run_result {
                    Err(AgentError::Cancelled) => {}
                    Err(error) => {
                        send_error(
                            &outbound_sender,
                            Some(cleanup_session_id.clone()),
                            "runtime_error",
                            error.to_string(),
                        );
                    }
                    Ok(()) => {}
                }

                let mut sessions_guard = sessions.lock().await;
                if let Some(session_state) = sessions_guard.get_mut(&cleanup_session_id) {
                    session_state.in_flight = None;
                }
            });

            session_state.in_flight = Some(InFlightRequest { task, cancel });
        }
        ClientEvent::Cancel(cancel_request) => {
            let session_id = cancel_request.session_id;
            if !is_valid_session_id(&session_id) {
                send_error(
                    outbound_sender,
                    Some(session_id),
                    "invalid_session_id",
                    "session_id must be a UUID",
                );
                return;
            }
            let mut sessions = state.sessions.lock().await;

            let Some(session_state) =
                get_session_state_mut(&mut sessions, outbound_sender, &session_id)
            else {
                return;
            };

            if let Some(in_flight) = session_state.in_flight.take() {
                in_flight.cancel.cancel();
                in_flight.task.abort();
                send_error(
                    outbound_sender,
                    Some(session_id),
                    "cancelled",
                    "in-flight request cancelled",
                );
            } else {
                send_error(
                    outbound_sender,
                    Some(session_id),
                    "request_not_in_flight",
                    "session has no in-flight request to cancel",
                );
            }
        }
    }
}

fn get_session_state_mut<'a>(
    sessions: &'a mut HashMap<String, SessionState>,
    outbound_sender: &mpsc::UnboundedSender<ServerEnvelope>,
    session_id: &str,
) -> Option<&'a mut SessionState> {
    if let Some(session_state) = sessions.get_mut(session_id) {
        Some(session_state)
    } else {
        send_error(
            outbound_sender,
            Some(session_id.to_string()),
            "session_not_found",
            "session was not found",
        );
        None
    }
}

fn allocate_session_id(sessions: &HashMap<String, SessionState>) -> String {
    loop {
        let id = Uuid::new_v4().to_string();
        if !sessions.contains_key(&id) {
            return id;
        }
    }
}

fn is_valid_session_id(session_id: &str) -> bool {
    Uuid::try_parse(session_id).is_ok()
}

fn send_event(outbound_sender: &mpsc::UnboundedSender<ServerEnvelope>, event: ServerEvent) {
    let _ = outbound_sender.send(ServerEnvelope::new(event));
}

fn send_error(
    outbound_sender: &mpsc::UnboundedSender<ServerEnvelope>,
    session_id: Option<String>,
    code: impl Into<String>,
    message: impl Into<String>,
) {
    send_event(
        outbound_sender,
        ServerEvent::Error(ErrorEvent {
            session_id,
            code: code.into(),
            message: message.into(),
        }),
    );
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use rho_core::{Message, providers::ModelKind, stream::ProviderEvent};
    use tokio::sync::mpsc;
    use uuid::Uuid;

    use super::*;
    use crate::test_helpers::{FakeProvider, PendingProvider};

    #[tokio::test]
    async fn start_session_emits_session_ack() {
        let state = test_state(Arc::new(FakeProvider::new(vec![])));
        let (tx, mut rx) = mpsc::unbounded_channel();

        handle_client_event(
            &state,
            &tx,
            ClientEvent::StartSession(rho_core::protocol::StartSession { session_id: None }),
        )
        .await;

        let envelope = rx.recv().await.expect("expected outbound envelope");
        let session_id = match envelope.event {
            ServerEvent::SessionAck(SessionAck { session_id }) => session_id,
            _ => panic!("expected session ack"),
        };
        assert!(Uuid::try_parse(&session_id).is_ok());
        assert_eq!(state.sessions.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn start_session_with_invalid_id_emits_error() {
        let state = test_state(Arc::new(FakeProvider::new(vec![])));
        let (tx, mut rx) = mpsc::unbounded_channel();

        handle_client_event(
            &state,
            &tx,
            ClientEvent::StartSession(rho_core::protocol::StartSession {
                session_id: Some("not-a-uuid".to_string()),
            }),
        )
        .await;

        let envelope = rx.recv().await.expect("expected outbound envelope");
        assert!(matches!(
            envelope.event,
            ServerEvent::Error(ErrorEvent { code, .. }) if code == "invalid_session_id"
        ));
        assert!(state.sessions.lock().await.is_empty());
    }

    #[tokio::test]
    async fn user_message_for_unknown_session_emits_error() {
        let state = test_state(Arc::new(FakeProvider::new(vec![])));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let missing_session_id = "00000000-0000-4000-8000-0000000000ff".to_string();

        handle_client_event(
            &state,
            &tx,
            ClientEvent::UserMessage(rho_core::protocol::UserMessage {
                session_id: missing_session_id,
                message: Message::new(MessageRole::User, "hi"),
            }),
        )
        .await;

        let envelope = rx.recv().await.expect("expected outbound envelope");
        assert!(matches!(
            envelope.event,
            ServerEvent::Error(ErrorEvent { code, .. }) if code == "session_not_found"
        ));
    }

    #[tokio::test]
    async fn user_message_streams_assistant_events() {
        let provider = Arc::new(FakeProvider::new(vec![vec![
            Ok(ProviderEvent::AssistantDelta {
                delta: "hello".to_string(),
            }),
            Ok(ProviderEvent::Message {
                message: Message::new(MessageRole::Assistant, "hello"),
            }),
            Ok(ProviderEvent::Finished),
        ]]));
        let state = test_state(provider);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let session_id = "00000000-0000-4000-8000-00000000000a".to_string();

        handle_client_event(
            &state,
            &tx,
            ClientEvent::StartSession(rho_core::protocol::StartSession {
                session_id: Some(session_id.clone()),
            }),
        )
        .await;
        let _ = rx.recv().await;

        handle_client_event(
            &state,
            &tx,
            ClientEvent::UserMessage(rho_core::protocol::UserMessage {
                session_id: session_id.clone(),
                message: Message::new(MessageRole::User, "hello"),
            }),
        )
        .await;

        let first = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out waiting for first response")
            .expect("missing first response");
        let second = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out waiting for second response")
            .expect("missing second response");

        assert!(matches!(
            first.event,
            ServerEvent::AssistantDelta(_)
                | ServerEvent::ToolStarted(_)
                | ServerEvent::ToolCompleted(_)
        ));
        assert!(matches!(second.event, ServerEvent::Final(_)));
    }

    #[tokio::test]
    async fn cancel_in_flight_request_emits_cancelled_error() {
        let state = test_state(Arc::new(PendingProvider));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let session_id = "00000000-0000-4000-8000-00000000000b".to_string();

        handle_client_event(
            &state,
            &tx,
            ClientEvent::StartSession(rho_core::protocol::StartSession {
                session_id: Some(session_id.clone()),
            }),
        )
        .await;
        let _ = rx.recv().await;

        handle_client_event(
            &state,
            &tx,
            ClientEvent::UserMessage(rho_core::protocol::UserMessage {
                session_id: session_id.clone(),
                message: Message::new(MessageRole::User, "hello"),
            }),
        )
        .await;

        handle_client_event(
            &state,
            &tx,
            ClientEvent::Cancel(rho_core::protocol::CancelRequest { session_id }),
        )
        .await;

        loop {
            let envelope = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("timed out waiting for cancel response")
                .expect("missing cancel response");

            if let ServerEvent::Error(error) = envelope.event {
                assert_eq!(error.code, "cancelled");
                break;
            }
        }
    }

    fn test_state(provider: Arc<dyn Provider>) -> AppState {
        AppState {
            runtime: AgentRuntime::new(),
            provider,
            model: ModelKind::Gpt52,
            sessions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }
}
