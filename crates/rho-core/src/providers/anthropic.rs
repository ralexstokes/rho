use std::sync::{Arc, OnceLock};

use rig::{
    client::completion::CompletionClient, completion::CompletionModel, providers::anthropic,
};

use crate::providers::{
    CancellationToken, Provider, ProviderError, ProviderKind, ProviderRequest, ProviderStream,
    apply_common_request_options, map_rig_completion_error, map_rig_http_error,
    stream_from_response, to_rig_chat_request, validate_api_key,
};

const ANTHROPIC_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";

#[derive(Debug, Clone, Default)]
pub struct AnthropicProvider {
    client: Arc<OnceLock<anthropic::Client>>,
}

impl AnthropicProvider {
    pub fn new() -> Self {
        Self::default()
    }

    fn client(&self) -> Result<anthropic::Client, ProviderError> {
        if let Some(client) = self.client.get() {
            return Ok(client.clone());
        }

        let api_key = anthropic_api_key()?;
        let client = anthropic_client(api_key)?;
        let _ = self.client.set(client);

        Ok(self
            .client
            .get()
            .expect("anthropic client should be initialized")
            .clone())
    }
}

impl Provider for AnthropicProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Anthropic
    }

    fn stream(&self, request: ProviderRequest<'_>, cancel: CancellationToken) -> ProviderStream {
        let rig_request = to_rig_chat_request(request);
        let client = self.client();
        Box::pin(async_stream::try_stream! {
            let client = client?;
            let rig_request = rig_request?;

            let model = client
                .completion_model(rig_request.model.clone())
                .with_prompt_caching();
            let builder = model
                .completion_request(rig_request.prompt)
                .messages(rig_request.history);
            let builder =
                apply_common_request_options(builder, rig_request.preamble, rig_request.tools);

            let stream = builder
                .stream()
                .await
                .map_err(|error| map_rig_completion_error(ANTHROPIC_API_KEY_ENV, error))?;

            for await event in stream_from_response(stream, ANTHROPIC_API_KEY_ENV, cancel) {
                yield event?;
            }
        })
    }
}

fn anthropic_api_key() -> Result<String, ProviderError> {
    validate_api_key(
        ANTHROPIC_API_KEY_ENV,
        std::env::var(ANTHROPIC_API_KEY_ENV).ok(),
    )
}

fn anthropic_client(api_key: String) -> Result<anthropic::Client, ProviderError> {
    anthropic::Client::builder()
        .api_key(api_key)
        .build()
        .map_err(|error| map_rig_http_error(ANTHROPIC_API_KEY_ENV, error))
}
