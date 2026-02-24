use std::sync::{Arc, OnceLock};

use futures_util::StreamExt;
use rig::{client::completion::CompletionClient, completion::CompletionModel, providers::openai};
use tracing::{debug, info, info_span, trace, warn};

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
pub struct OpenAiProvider {
    client: Arc<OnceLock<openai::Client>>,
}

impl OpenAiProvider {
    pub fn new() -> Self {
        Self::default()
    }

    fn client(&self) -> Result<openai::Client, ProviderError> {
        if let Some(client) = self.client.get() {
            trace!("reusing cached OpenAI client");
            return Ok(client.clone());
        }

        let api_key = openai_api_key()?;
        let client = openai_client(api_key)?;
        let _ = self.client.set(client);
        info!("initialized OpenAI client");

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

    fn stream(&self, request: ProviderRequest<'_>) -> ProviderStream {
        let span = info_span!(
            "provider.openai.stream",
            model = %request.model,
            message_count = request.messages.len(),
            tool_count = request.tools.len(),
        );
        let rig_request = to_rig_chat_request(request);
        let client = self.client();
        Box::pin(async_stream::try_stream! {
            info!(parent: &span, "starting OpenAI provider stream");
            let client = client.map_err(|error| {
                warn!(parent: &span, %error, "OpenAI client unavailable");
                error
            })?;
            let rig_request = rig_request.map_err(|error| {
                warn!(parent: &span, %error, "invalid OpenAI provider request");
                error
            })?;
            debug!(
                parent: &span,
                request_model = %rig_request.model,
                history_count = rig_request.history.len(),
                has_preamble = rig_request.preamble.is_some(),
                rig_tool_count = rig_request.tools.len(),
                "converted provider request to rig request"
            );
            let builder = client
                .completion_model(rig_request.model.clone())
                .completion_request(rig_request.prompt)
                .messages(rig_request.history);
            let builder =
                apply_common_request_options(builder, rig_request.preamble, rig_request.tools);

            let mut stream = builder
                .stream()
                .await
                .map_err(|error| {
                    let mapped = map_rig_completion_error(OPENAI_API_KEY_ENV, error);
                    warn!(parent: &span, %mapped, "OpenAI stream setup failed");
                    mapped
                })?;
            debug!(parent: &span, "OpenAI upstream stream established");

            while let Some(next_chunk) = stream.next().await {
                let chunk = next_chunk
                    .map_err(|error| {
                        let mapped = map_rig_completion_error(OPENAI_API_KEY_ENV, error);
                        warn!(parent: &span, %mapped, "OpenAI stream chunk failed");
                        mapped
                    })?;
                if let Some(event) = map_streamed_assistant_chunk(chunk) {
                    trace!(
                        parent: &span,
                        event = provider_event_name(&event),
                        "emitting provider event from streamed chunk"
                    );
                    yield event;
                }
            }

            let message = Message::new(MessageRole::Assistant, rig_choice_text(&stream.choice));
            info!(
                parent: &span,
                assistant_chars = message.content.chars().count(),
                "OpenAI provider stream completed"
            );
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
        debug!(base_url = %base_url, "using OpenAI base URL override");
        builder = builder.base_url(&base_url);
    }

    builder
        .build()
        .map_err(|error| map_rig_http_error(OPENAI_API_KEY_ENV, error))
}
fn provider_event_name(event: &ProviderEvent) -> &'static str {
    match event {
        ProviderEvent::AssistantDelta { .. } => "assistant_delta",
        ProviderEvent::ToolCall { .. } => "tool_call",
        ProviderEvent::ToolResult { .. } => "tool_result",
        ProviderEvent::Message { .. } => "message",
        ProviderEvent::Finished => "finished",
    }
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
