use futures_util::stream;

use crate::providers::{Provider, ProviderError, ProviderKind, ProviderRequest, ProviderStream};

#[derive(Debug, Default)]
pub struct OpenAiProvider;

impl OpenAiProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Provider for OpenAiProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenAi
    }

    fn stream(&self, _request: ProviderRequest) -> ProviderStream {
        Box::pin(stream::once(async {
            Err(ProviderError::NotImplemented("openai"))
        }))
    }
}
