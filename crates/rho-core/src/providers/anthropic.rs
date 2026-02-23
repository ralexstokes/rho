use futures_util::stream;

use crate::providers::{Provider, ProviderError, ProviderKind, ProviderRequest, ProviderStream};

#[derive(Debug, Default)]
pub struct AnthropicProvider;

impl AnthropicProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Provider for AnthropicProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Anthropic
    }

    fn stream(&self, _request: ProviderRequest) -> ProviderStream {
        Box::pin(stream::once(async {
            Err(ProviderError::NotImplemented("anthropic"))
        }))
    }
}
