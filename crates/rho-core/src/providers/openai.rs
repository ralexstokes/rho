use std::sync::{Arc, OnceLock};

use rig::{client::completion::CompletionClient, completion::CompletionModel, providers::openai};

use crate::providers::{
    CancellationToken, PreparedRequest, Provider, ProviderError, ProviderKind, ProviderStream,
    apply_common_request_options, map_rig_completion_error, map_rig_http_error,
    stream_from_response, validate_api_key,
};

const OPENAI_API_KEY_ENV: &str = "OPENAI_API_KEY";

#[derive(Debug, Clone, Default)]
pub struct OpenAiProvider {
    client: Arc<OnceLock<openai::Client>>,
}

impl OpenAiProvider {
    pub fn new() -> Self {
        Self::default()
    }

    fn client(&self) -> Result<openai::Client, ProviderError> {
        if let Some(client) = self.client.get() {
            return Ok(client.clone());
        }

        let api_key = openai_api_key()?;
        let client = openai_client(api_key)?;
        let _ = self.client.set(client);

        Ok(self
            .client
            .get()
            .expect("openai client should be initialized")
            .clone())
    }
}

impl Provider for OpenAiProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenAi
    }

    fn stream(&self, request: &PreparedRequest, cancel: CancellationToken) -> ProviderStream {
        let model = request.inner.model.clone();
        let prompt = request.inner.prompt.clone();
        let history = request.inner.history.clone();
        let preamble = request.inner.preamble.clone();
        let tools = request.inner.tools.clone();
        let client = self.client();
        Box::pin(async_stream::try_stream! {
            let client = client?;
            let builder = client
                .completion_model(model)
                .completion_request(prompt)
                .messages(history);
            let builder =
                apply_common_request_options(builder, preamble, tools);

            let stream = builder
                .stream()
                .await
                .map_err(|error| map_rig_completion_error(OPENAI_API_KEY_ENV, error))?;

            for await event in stream_from_response(stream, OPENAI_API_KEY_ENV, cancel) {
                yield event?;
            }
        })
    }
}

fn openai_api_key() -> Result<String, ProviderError> {
    validate_api_key(OPENAI_API_KEY_ENV, std::env::var(OPENAI_API_KEY_ENV).ok())
}

fn openai_client(api_key: String) -> Result<openai::Client, ProviderError> {
    let mut builder = openai::Client::builder().api_key(api_key);

    if let Ok(base_url) = std::env::var("OPENAI_BASE_URL")
        && !base_url.trim().is_empty()
    {
        builder = builder.base_url(&base_url);
    }

    builder
        .build()
        .map_err(|error| map_rig_http_error(OPENAI_API_KEY_ENV, error))
}
