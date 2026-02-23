use futures_util::StreamExt;
use rig::{
    client::completion::CompletionClient, completion::CompletionModel, providers::anthropic,
};

use crate::{
    Message, MessageRole,
    providers::{
        Provider, ProviderError, ProviderKind, ProviderRequest, ProviderStream,
        map_rig_completion_error, map_rig_http_error, map_streamed_assistant_chunk,
        rig_choice_text, to_rig_chat_request, validate_api_key,
    },
    stream::ProviderEvent,
};

const ANTHROPIC_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";

#[derive(Debug, Clone, Default)]
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

    fn stream(&self, request: ProviderRequest) -> ProviderStream {
        Box::pin(async_stream::try_stream! {
            let api_key = anthropic_api_key()?;
            let client = anthropic_client(api_key)?;
            let rig_request = to_rig_chat_request(request)?;

            let model = client
                .completion_model(rig_request.model.clone())
                .with_prompt_caching();

            let mut builder = model
                .completion_request(rig_request.prompt)
                .messages(rig_request.history);

            if let Some(preamble) = rig_request.preamble {
                builder = builder.preamble(preamble);
            }

            if !rig_request.tools.is_empty() {
                builder = builder.tools(rig_request.tools);
            }

            let mut stream = builder
                .stream()
                .await
                .map_err(|error| map_rig_completion_error(ANTHROPIC_API_KEY_ENV, error))?;

            while let Some(next_chunk) = stream.next().await {
                let chunk = next_chunk
                    .map_err(|error| map_rig_completion_error(ANTHROPIC_API_KEY_ENV, error))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::validate_api_key;

    #[test]
    fn validate_api_key_rejects_missing_value() {
        let error =
            validate_api_key(ANTHROPIC_API_KEY_ENV, None).expect_err("missing API key should fail");
        assert!(matches!(
            error,
            ProviderError::MissingApiKey(ANTHROPIC_API_KEY_ENV)
        ));
    }

    #[test]
    fn validate_api_key_rejects_blank_value() {
        let error = validate_api_key(ANTHROPIC_API_KEY_ENV, Some("   ".to_string()))
            .expect_err("blank API key should fail");
        assert!(matches!(
            error,
            ProviderError::InvalidApiKey(ANTHROPIC_API_KEY_ENV)
        ));
    }
}
