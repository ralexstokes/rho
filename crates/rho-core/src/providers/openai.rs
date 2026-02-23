use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

use crate::{
    message::{Message, MessageRole},
    providers::{
        Provider, ProviderError, ProviderKind, ProviderRequest, ProviderStream, validate_api_key,
    },
    stream::ProviderEvent,
};

const OPENAI_API_KEY_ENV: &str = "OPENAI_API_KEY";
const OPENAI_API_BASE: &str = "https://api.openai.com/v1";

#[derive(Debug, Clone)]
pub struct OpenAiProvider {
    client: reqwest::Client,
    base_url: String,
}

impl Default for OpenAiProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenAiProvider {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: OPENAI_API_BASE.to_string(),
        }
    }
}

impl Provider for OpenAiProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenAi
    }

    fn stream(&self, request: ProviderRequest) -> ProviderStream {
        let client = self.client.clone();
        let base_url = self.base_url.clone();

        Box::pin(async_stream::try_stream! {
            let api_key = openai_api_key()?;

            let response = client
                .post(format!("{base_url}/chat/completions"))
                .bearer_auth(api_key)
                .json(&OpenAiRequest::from(request))
                .send()
                .await
                .map_err(|error| ProviderError::Transport(format!("openai request failed: {error}")))?;

            let status = response.status();
            if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                Err(ProviderError::InvalidApiKey(OPENAI_API_KEY_ENV))?;
            }

            if !status.is_success() {
                Err(ProviderError::Transport(format!(
                    "openai request failed with status {status}"
                )))?;
            }

            let mut aggregate_text = String::new();
            let mut stream = response.bytes_stream().eventsource();

            while let Some(next_event) = stream.next().await {
                let event = next_event
                    .map_err(|error| ProviderError::Transport(format!("openai stream failed: {error}")))?;

                let data = event.data.trim();
                if data == "[DONE]" {
                    break;
                }

                let chunk: OpenAiChunk = serde_json::from_str(data).map_err(|error| {
                    ProviderError::Transport(format!("invalid openai stream payload: {error}"))
                })?;

                for choice in chunk.choices {
                    if let Some(delta) = choice.delta.content
                        && !delta.is_empty()
                    {
                        aggregate_text.push_str(&delta);
                        yield ProviderEvent::AssistantDelta { delta };
                    }
                }
            }

            let message = Message::new(MessageRole::Assistant, aggregate_text);
            yield ProviderEvent::Message { message };
            yield ProviderEvent::Finished;
        })
    }
}

fn openai_api_key() -> Result<String, ProviderError> {
    validate_api_key(OPENAI_API_KEY_ENV, std::env::var(OPENAI_API_KEY_ENV).ok())
}

#[derive(Debug, Serialize)]
struct OpenAiRequest {
    model: String,
    messages: Vec<OpenAiMessage>,
    stream: bool,
}

impl From<ProviderRequest> for OpenAiRequest {
    fn from(request: ProviderRequest) -> Self {
        let messages = request
            .messages
            .into_iter()
            .map(|message| OpenAiMessage {
                role: openai_role(message.role),
                content: message.content,
            })
            .collect();

        Self {
            model: request.model,
            messages,
            stream: true,
        }
    }
}

#[derive(Debug, Serialize)]
struct OpenAiMessage {
    role: &'static str,
    content: String,
}

fn openai_role(role: MessageRole) -> &'static str {
    match role {
        MessageRole::System => "system",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "tool",
    }
}

#[derive(Debug, Deserialize)]
struct OpenAiChunk {
    choices: Vec<OpenAiChoice>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChoice {
    delta: OpenAiDelta,
}

#[derive(Debug, Deserialize)]
struct OpenAiDelta {
    content: Option<String>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::providers::ProviderRequest;

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

    #[test]
    fn provider_request_maps_to_openai_body() {
        let request = ProviderRequest::new(
            "gpt-4o-mini",
            vec![
                Message::new(MessageRole::System, "be concise"),
                Message::new(MessageRole::User, "hi"),
            ],
        );

        let body = serde_json::to_value(OpenAiRequest::from(request)).expect("serialize request");

        assert_eq!(body["model"], json!("gpt-4o-mini"));
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["messages"][0]["role"], json!("system"));
        assert_eq!(body["messages"][0]["content"], json!("be concise"));
        assert_eq!(body["messages"][1]["role"], json!("user"));
    }
}
