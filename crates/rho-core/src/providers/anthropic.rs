use std::sync::{Arc, OnceLock};

use rig::{
    client::completion::CompletionClient, completion::CompletionModel, providers::anthropic,
};

use crate::providers::{
    CancellationToken, PreparedRequest, Provider, ProviderError, ProviderKind, ProviderStream,
    apply_common_request_options, map_rig_completion_error, map_rig_http_error,
    stream_from_response, validate_api_key,
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

    fn stream(&self, request: &PreparedRequest, cancel: CancellationToken) -> ProviderStream {
        let model_name = request.inner.model.clone();
        let prompt = request.inner.prompt.clone();
        let history = request.inner.history.clone();
        let preamble = request.inner.preamble.clone();
        let tools = request.inner.tools.clone();
        let client = self.client();
        Box::pin(async_stream::try_stream! {
            let client = client?;

            let model = client
                .completion_model(model_name)
                .with_prompt_caching();
            let builder = model
                .completion_request(prompt)
                .messages(history);
            let builder =
                apply_common_request_options(builder, preamble, tools);

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
