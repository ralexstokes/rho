use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use rho_core::providers::{
    CancellationToken, Provider, ProviderKind, ProviderRequest, ProviderStream,
};

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
    pub fn from_request(request: ProviderRequest<'_>) -> Self {
        Self {
            messages: request.messages.to_vec(),
            tools: request.tools.to_vec(),
        }
    }
}

impl Provider for FakeProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenAi
    }

    fn stream(&self, request: ProviderRequest<'_>, _cancel: CancellationToken) -> ProviderStream {
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

    fn stream(&self, _request: ProviderRequest<'_>, _cancel: CancellationToken) -> ProviderStream {
        Box::pin(futures_util::stream::pending())
    }
}
