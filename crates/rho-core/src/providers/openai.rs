use futures_util::StreamExt;
use rig::{client::completion::CompletionClient, completion::CompletionModel, providers::openai};

use crate::{
    Message, MessageRole,
    providers::{
        Provider, ProviderError, ProviderKind, ProviderRequest, ProviderStream,
        apply_common_request_options, map_rig_completion_error, map_rig_http_error,
        map_streamed_assistant_chunk, rig_choice_text, to_rig_chat_request, validate_api_key,
    },
    stream::ProviderEvent,
};

const OPENAI_API_KEY_ENV: &str = "OPENAI_API_KEY";

#[derive(Debug, Clone, Default)]
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

    fn stream(&self, request: ProviderRequest) -> ProviderStream {
        Box::pin(async_stream::try_stream! {
            let api_key = openai_api_key()?;
            let client = openai_client(api_key)?;
            let rig_request = to_rig_chat_request(request)?;
            let builder = client
                .completion_model(rig_request.model.clone())
                .completion_request(rig_request.prompt)
                .messages(rig_request.history);
            let builder =
                apply_common_request_options(builder, rig_request.preamble, rig_request.tools);

            let mut stream = builder
                .stream()
                .await
                .map_err(|error| map_rig_completion_error(OPENAI_API_KEY_ENV, error))?;

            while let Some(next_chunk) = stream.next().await {
                let chunk = next_chunk
                    .map_err(|error| map_rig_completion_error(OPENAI_API_KEY_ENV, error))?;
                if let Some(event) = map_streamed_assistant_chunk(chunk) {
                    yield event;
                }
            }

            let message = Message::new(MessageRole::Assistant, rig_choice_text(&stream.choice));
            yield ProviderEvent::Message { message };
            yield ProviderEvent::Finished;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::validate_api_key;

    #[test]
    fn validate_api_key_rejects_missing_value() {
        let error =
            validate_api_key(OPENAI_API_KEY_ENV, None).expect_err("missing API key should fail");
        assert!(matches!(
            error,
            ProviderError::MissingApiKey(OPENAI_API_KEY_ENV)
        ));
    }

    #[test]
    fn validate_api_key_rejects_blank_value() {
        let error = validate_api_key(OPENAI_API_KEY_ENV, Some("   ".to_string()))
            .expect_err("blank API key should fail");
        assert!(matches!(
            error,
            ProviderError::InvalidApiKey(OPENAI_API_KEY_ENV)
        ));
    }
}
