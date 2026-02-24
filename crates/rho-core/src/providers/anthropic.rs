use std::sync::{Arc, OnceLock};

use futures_util::StreamExt;
use rig::{
    client::completion::CompletionClient, completion::CompletionModel, providers::anthropic,
};
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
            trace!("reusing cached Anthropic client");
            return Ok(client.clone());
        }

        let api_key = anthropic_api_key()?;
        let client = anthropic_client(api_key)?;
        let _ = self.client.set(client);
        info!("initialized Anthropic client");

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

    fn stream(&self, request: ProviderRequest<'_>) -> ProviderStream {
        let span = info_span!(
            "provider.anthropic.stream",
            model = %request.model,
            message_count = request.messages.len(),
            tool_count = request.tools.len(),
        );
        let rig_request = to_rig_chat_request(request);
        let client = self.client();
        Box::pin(async_stream::try_stream! {
            info!(parent: &span, "starting Anthropic provider stream");
            let client = client.map_err(|error| {
                warn!(parent: &span, %error, "Anthropic client unavailable");
                error
            })?;
            let rig_request = rig_request.map_err(|error| {
                warn!(parent: &span, %error, "invalid Anthropic provider request");
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

            let model = client
                .completion_model(rig_request.model.clone())
                .with_prompt_caching();
            let builder = model
                .completion_request(rig_request.prompt)
                .messages(rig_request.history);
            let builder =
                apply_common_request_options(builder, rig_request.preamble, rig_request.tools);

            let mut stream = builder
                .stream()
                .await
                .map_err(|error| {
                    let mapped = map_rig_completion_error(ANTHROPIC_API_KEY_ENV, error);
                    warn!(parent: &span, %mapped, "Anthropic stream setup failed");
                    mapped
                })?;
            debug!(parent: &span, "Anthropic upstream stream established");

            while let Some(next_chunk) = stream.next().await {
                let chunk = next_chunk
                    .map_err(|error| {
                        let mapped = map_rig_completion_error(ANTHROPIC_API_KEY_ENV, error);
                        warn!(parent: &span, %mapped, "Anthropic stream chunk failed");
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
                "Anthropic provider stream completed"
            );
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
