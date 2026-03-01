use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use rho_core::providers::{
    CancellationToken, PreparedRequest, Provider, ProviderKind, ProviderStream,
};
use tokio::sync::Notify;

pub type FakeResponse =
    Vec<Result<rho_core::stream::ProviderEvent, rho_core::providers::ProviderError>>;
pub type FakeResponseQueue = VecDeque<FakeResponse>;

#[derive(Debug, Clone)]
pub struct FakeProvider {
    responses: Arc<Mutex<FakeResponseQueue>>,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl FakeProvider {
    pub fn new(responses: Vec<FakeResponse>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses.into_iter().collect())),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests
            .lock()
            .expect("requests mutex should be available")
            .clone()
    }
}

#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub messages: Vec<rho_core::Message>,
    pub tools: Vec<rho_core::tool::ToolDefinition>,
}

impl RecordedRequest {
    pub fn from_request(request: &PreparedRequest) -> Self {
        Self {
            messages: request.messages().to_vec(),
            tools: request.tools().to_vec(),
        }
    }
}

impl Provider for FakeProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenAi
    }

    fn stream(&self, request: &PreparedRequest, _cancel: CancellationToken) -> ProviderStream {
        self.requests
            .lock()
            .expect("requests mutex should be available")
            .push(RecordedRequest::from_request(request));

        let events = self
            .responses
            .lock()
            .expect("responses mutex should be available")
            .pop_front()
            .unwrap_or_default();

        Box::pin(futures_util::stream::iter(events))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PendingProvider;

impl Provider for PendingProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenAi
    }

    fn stream(&self, _request: &PreparedRequest, _cancel: CancellationToken) -> ProviderStream {
        Box::pin(futures_util::stream::pending())
    }
}

/// A provider that yields queued events then blocks until the cancellation
/// token fires, allowing tests to verify cancellation behavior deterministically.
///
/// When all events for a stream call have been yielded, the provider signals
/// via [`blocked()`](Self::blocked) and waits for cancellation.
#[derive(Debug, Clone)]
pub struct CancellableProvider {
    responses: Arc<Mutex<FakeResponseQueue>>,
    blocked: Arc<Notify>,
}

impl CancellableProvider {
    pub fn new(responses: Vec<FakeResponse>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses.into_iter().collect())),
            blocked: Arc::new(Notify::new()),
        }
    }

    /// Returns a handle that is notified each time the provider exhausts its
    /// queued events and begins waiting for cancellation.
    pub fn blocked(&self) -> Arc<Notify> {
        self.blocked.clone()
    }
}

impl Provider for CancellableProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenAi
    }

    fn stream(&self, _request: &PreparedRequest, cancel: CancellationToken) -> ProviderStream {
        let events = self
            .responses
            .lock()
            .expect("responses mutex should be available")
            .pop_front()
            .unwrap_or_default();

        let blocked = self.blocked.clone();
        Box::pin(futures_util::stream::unfold(
            (VecDeque::from(events), cancel, blocked),
            |(mut events, cancel, blocked)| async {
                if let Some(event) = events.pop_front() {
                    Some((event, (events, cancel, blocked)))
                } else {
                    blocked.notify_one();
                    cancel.cancelled().await;
                    None
                }
            },
        ))
    }
}
