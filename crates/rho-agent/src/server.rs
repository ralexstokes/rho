use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

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
        ModelKind, Provider, ProviderKind, ProviderStream, anthropic::AnthropicProvider,
        openai::OpenAiProvider,
    },
};
use thiserror::Error;
use tokio::{
    net::TcpListener,
    sync::{Mutex, mpsc},
    task::JoinHandle,
};
use uuid::Uuid;

use crate::{AgentRuntime, AgentSession};

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
    request_generation: u64,
    in_flight: Option<InFlightRequest>,
}

impl SessionState {
    fn new(session: AgentSession) -> Self {
        Self {
            session: Arc::new(Mutex::new(session)),
            request_generation: 0,
            in_flight: None,
        }
    }

    fn next_generation(&mut self) -> u64 {
        self.request_generation = self.request_generation.wrapping_add(1);
        self.request_generation
    }

    fn invalidate_generation(&mut self) {
        self.request_generation = self.request_generation.wrapping_add(1);
    }
}

struct InFlightRequest {
    generation: u64,
    task: JoinHandle<()>,
    cancel: RequestCancellation,
}

#[derive(Clone, Default)]
struct RequestCancellation {
    cancelled: Arc<AtomicBool>,
    provider_cancel: Arc<std::sync::Mutex<Option<rho_core::providers::ProviderCancelHandle>>>,
}

impl RequestCancellation {
    fn register_provider_stream(&self, stream: &ProviderStream) {
        let Some(cancel_handle) = stream.cancel_handle() else {
            return;
        };

        if self.cancelled.load(Ordering::SeqCst) {
            cancel_handle.cancel();
            return;
        }

        let mut provider_cancel = self
            .provider_cancel
            .lock()
            .expect("provider cancel mutex should be available");
        if self.cancelled.load(Ordering::SeqCst) {
            drop(provider_cancel);
            cancel_handle.cancel();
            return;
        }
        *provider_cancel = Some(cancel_handle);
    }

    fn cancel_provider(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Some(cancel_handle) = self
            .provider_cancel
            .lock()
            .expect("provider cancel mutex should be available")
            .as_ref()
        {
            cancel_handle.cancel();
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EventScope {
    session_id: String,
    generation: u64,
}

#[derive(Debug, Clone, PartialEq)]
struct OutboundEnvelope {
    envelope: ServerEnvelope,
    scope: Option<EventScope>,
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_websocket(socket, state))
}

async fn handle_websocket(stream: WebSocket, state: AppState) {
    let (mut sink, mut source) = stream.split();
    let (outbound_sender, mut outbound_receiver) = mpsc::unbounded_channel::<OutboundEnvelope>();
    let sessions = Arc::clone(&state.sessions);

    let writer_task = tokio::spawn(async move {
        while let Some(outbound) = outbound_receiver.recv().await {
            if let Some(scope) = outbound.scope.as_ref() {
                let sessions_guard = sessions.lock().await;
                if !is_scope_current(&sessions_guard, scope) {
                    continue;
                }
            }

            let payload = match serde_json::to_string(&outbound.envelope) {
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
    outbound_sender: &mpsc::UnboundedSender<OutboundEnvelope>,
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
    outbound_sender: &mpsc::UnboundedSender<OutboundEnvelope>,
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
            let request_generation = session_state.next_generation();
            let request_cancellation = RequestCancellation::default();

            let runtime = state.runtime.clone();
            let model = state.model;
            let provider = Arc::clone(&state.provider);
            let session = Arc::clone(&session_state.session);
            let outbound_sender = outbound_sender.clone();
            let sessions = Arc::clone(&state.sessions);
            let cleanup_session_id = session_id.clone();
            let task_cancellation = request_cancellation.clone();

            let task = tokio::spawn(async move {
                let request_scope = EventScope {
                    session_id: cleanup_session_id.clone(),
                    generation: request_generation,
                };
                let run_result = {
                    let mut session_guard = session.lock().await;
                    runtime
                        .run_user_message_streaming_with_observer(
                            &mut session_guard,
                            provider.as_ref(),
                            model,
                            user_content,
                            |event| {
                                send_scoped_event(&outbound_sender, request_scope.clone(), event)
                            },
                            |provider_stream| {
                                task_cancellation.register_provider_stream(provider_stream);
                            },
                        )
                        .await
                };

                if let Err(error) = run_result {
                    send_scoped_error(
                        &outbound_sender,
                        request_scope.clone(),
                        Some(cleanup_session_id.clone()),
                        "runtime_error",
                        error.to_string(),
                    );
                }

                let mut sessions_guard = sessions.lock().await;
                if let Some(session_state) = sessions_guard.get_mut(&cleanup_session_id)
                    && session_state
                        .in_flight
                        .as_ref()
                        .is_some_and(|in_flight| in_flight.generation == request_generation)
                {
                    session_state.in_flight = None;
                }
            });

            session_state.in_flight = Some(InFlightRequest {
                generation: request_generation,
                task,
                cancel: request_cancellation,
            });
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
                if in_flight.task.is_finished() {
                    send_error(
                        outbound_sender,
                        Some(session_id),
                        "request_not_in_flight",
                        "session has no in-flight request to cancel",
                    );
                } else {
                    in_flight.cancel.cancel_provider();
                    in_flight.task.abort();
                    session_state.invalidate_generation();
                    send_error(
                        outbound_sender,
                        Some(session_id),
                        "cancelled",
                        "in-flight request cancelled",
                    );
                }
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
    outbound_sender: &mpsc::UnboundedSender<OutboundEnvelope>,
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

fn is_scope_current(sessions: &HashMap<String, SessionState>, scope: &EventScope) -> bool {
    sessions
        .get(scope.session_id.as_str())
        .is_some_and(|session_state| session_state.request_generation == scope.generation)
}

fn send_event(outbound_sender: &mpsc::UnboundedSender<OutboundEnvelope>, event: ServerEvent) {
    let _ = outbound_sender.send(OutboundEnvelope {
        envelope: ServerEnvelope::new(event),
        scope: None,
    });
}

fn send_scoped_event(
    outbound_sender: &mpsc::UnboundedSender<OutboundEnvelope>,
    scope: EventScope,
    event: ServerEvent,
) {
    let _ = outbound_sender.send(OutboundEnvelope {
        envelope: ServerEnvelope::new(event),
        scope: Some(scope),
    });
}

fn send_error(
    outbound_sender: &mpsc::UnboundedSender<OutboundEnvelope>,
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

fn send_scoped_error(
    outbound_sender: &mpsc::UnboundedSender<OutboundEnvelope>,
    scope: EventScope,
    session_id: Option<String>,
    code: impl Into<String>,
    message: impl Into<String>,
) {
    send_scoped_event(
        outbound_sender,
        scope,
        ServerEvent::Error(ErrorEvent {
            session_id,
            code: code.into(),
            message: message.into(),
        }),
    );
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };

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
        let session_id = match outbound_event(envelope) {
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
            outbound_event(envelope),
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
            outbound_event(envelope),
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
            outbound_event(first),
            ServerEvent::AssistantDelta(_)
                | ServerEvent::ToolStarted(_)
                | ServerEvent::ToolCompleted(_)
        ));
        assert!(matches!(outbound_event(second), ServerEvent::Final(_)));
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

            if let ServerEvent::Error(error) = outbound_event(envelope) {
                assert_eq!(error.code, "cancelled");
                break;
            }
        }
    }

    #[tokio::test]
    async fn cancel_invokes_provider_cancel_handle() {
        let stream_started = Arc::new(AtomicBool::new(false));
        let stream_cancelled = Arc::new(AtomicBool::new(false));
        let provider = Arc::new(CancellablePendingProvider {
            stream_started: Arc::clone(&stream_started),
            stream_cancelled: Arc::clone(&stream_cancelled),
        });
        let state = test_state(provider);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let session_id = "00000000-0000-4000-8000-00000000000c".to_string();

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

        tokio::time::timeout(Duration::from_secs(1), async {
            while !stream_started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("provider stream should start");

        handle_client_event(
            &state,
            &tx,
            ClientEvent::Cancel(rho_core::protocol::CancelRequest { session_id }),
        )
        .await;

        assert!(
            stream_cancelled.load(Ordering::SeqCst),
            "cancel should invoke provider stream cancel handle"
        );
    }

    #[test]
    fn scope_matching_requires_current_session_generation() {
        let session = AgentRuntime::new().start_session("session-1");
        let mut sessions = HashMap::new();
        sessions.insert(
            "session-1".to_string(),
            SessionState {
                session: Arc::new(Mutex::new(session)),
                request_generation: 7,
                in_flight: None,
            },
        );

        assert!(is_scope_current(
            &sessions,
            &EventScope {
                session_id: "session-1".to_string(),
                generation: 7,
            }
        ));
        assert!(!is_scope_current(
            &sessions,
            &EventScope {
                session_id: "session-1".to_string(),
                generation: 6,
            }
        ));
        assert!(!is_scope_current(
            &sessions,
            &EventScope {
                session_id: "session-missing".to_string(),
                generation: 7,
            }
        ));
    }

    fn test_state(provider: Arc<dyn Provider>) -> AppState {
        AppState {
            runtime: AgentRuntime::new(),
            provider,
            model: ModelKind::Gpt52,
            sessions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }

    fn outbound_event(outbound: OutboundEnvelope) -> ServerEvent {
        outbound.envelope.event
    }

    #[derive(Clone)]
    struct CancellablePendingProvider {
        stream_started: Arc<AtomicBool>,
        stream_cancelled: Arc<AtomicBool>,
    }

    impl Provider for CancellablePendingProvider {
        fn kind(&self) -> ProviderKind {
            ProviderKind::OpenAi
        }

        fn stream(&self, _request: rho_core::providers::ProviderRequest<'_>) -> ProviderStream {
            self.stream_started.store(true, Ordering::SeqCst);
            let stream_cancelled = Arc::clone(&self.stream_cancelled);
            let cancel_handle = rho_core::providers::ProviderCancelHandle::new(move || {
                stream_cancelled.store(true, Ordering::SeqCst);
            });
            ProviderStream::with_cancel_handle_from_stream(
                futures_util::stream::pending(),
                cancel_handle,
            )
        }
    }
}
