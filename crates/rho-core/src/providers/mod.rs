use std::{pin::Pin, sync::OnceLock};

use futures_core::Stream;
use serde::{Deserialize, Serialize};
use thiserror::Error;
pub use tokio_util::sync::CancellationToken;

use crate::{
    message::{Message, MessageRole},
    stream::ProviderEvent,
    tool::ToolDefinition,
};

pub mod anthropic;
pub mod openai;

const DEFAULT_TOOL_RESULT_CALL_ID: &str = "tool-result";
const MAX_ERROR_BODY_LEN: usize = 512;

pub type ProviderStream = Pin<Box<dyn Stream<Item = Result<ProviderEvent, ProviderError>> + Send>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProviderKind {
    OpenAi,
    Anthropic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ModelKind {
    Gpt52,
    ClaudeSonnet46,
}

impl ModelKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gpt52 => "gpt-5.2-2025-12-11",
            Self::ClaudeSonnet46 => "claude-sonnet-4-6",
        }
    }

    pub fn provider_kind(self) -> ProviderKind {
        match self {
            Self::Gpt52 => ProviderKind::OpenAi,
            Self::ClaudeSonnet46 => ProviderKind::Anthropic,
        }
    }
}

impl std::fmt::Display for ModelKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderRequest<'a> {
    pub model: ModelKind,
    pub messages: &'a [Message],
    pub tools: &'a [ToolDefinition],
}

impl<'a> ProviderRequest<'a> {
    pub fn new(model: ModelKind, messages: &'a [Message]) -> Self {
        Self {
            model,
            messages,
            tools: &[],
        }
    }

    pub fn with_tools(mut self, tools: &'a [ToolDefinition]) -> Self {
        self.tools = tools;
        self
    }
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("missing API key in environment variable {0}")]
    MissingApiKey(&'static str),
    #[error("invalid API key in environment variable {0}")]
    InvalidApiKey(&'static str),
    #[error("provider {0} is not implemented")]
    NotImplemented(&'static str),
    #[error("provider transport error: {0}")]
    Transport(String),
}

pub(crate) fn validate_api_key(
    env_var: &'static str,
    api_key: Option<String>,
) -> Result<String, ProviderError> {
    let api_key = api_key.ok_or(ProviderError::MissingApiKey(env_var))?;
    if api_key.trim().is_empty() {
        return Err(ProviderError::InvalidApiKey(env_var));
    }
    Ok(api_key)
}

pub(crate) fn shared_http_client() -> Result<reqwest::Client, ProviderError> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

    if let Some(client) = CLIENT.get() {
        return Ok(client.clone());
    }

    let client = reqwest::Client::builder()
        .build()
        .map_err(map_reqwest_error)?;
    let _ = CLIENT.set(client);

    Ok(CLIENT
        .get()
        .expect("shared HTTP client should be initialized")
        .clone())
}

#[derive(Debug)]
pub(crate) struct NormalizedProviderRequest<'a> {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<&'a Message>,
    pub tools: &'a [ToolDefinition],
}

pub(crate) fn normalize_provider_request(
    request: ProviderRequest<'_>,
) -> Result<NormalizedProviderRequest<'_>, ProviderError> {
    let mut system_sections = Vec::new();
    let mut chat_messages = Vec::new();

    for message in request.messages {
        match message.role {
            MessageRole::System => system_sections.push(message.content.clone()),
            _ => chat_messages.push(message),
        }
    }

    if chat_messages.is_empty() {
        return Err(ProviderError::Transport(
            "provider request must include at least one non-system message".to_string(),
        ));
    }

    let system = if system_sections.is_empty() {
        None
    } else {
        Some(system_sections.join("\n\n"))
    };

    Ok(NormalizedProviderRequest {
        model: request.model.to_string(),
        system,
        messages: chat_messages,
        tools: request.tools,
    })
}

pub(crate) fn map_reqwest_error(error: reqwest::Error) -> ProviderError {
    ProviderError::Transport(error.to_string())
}

pub(crate) fn map_status_error(
    env_var: &'static str,
    status_code: u16,
    response_body: &str,
) -> ProviderError {
    if is_auth_status(status_code) || looks_like_auth_error(response_body) {
        return ProviderError::InvalidApiKey(env_var);
    }

    let response_body = sanitized_error_body(response_body);
    if response_body.is_empty() {
        ProviderError::Transport(format!("provider returned HTTP status {status_code}"))
    } else {
        ProviderError::Transport(format!(
            "provider returned HTTP status {status_code}: {response_body}"
        ))
    }
}

pub(crate) async fn ensure_success_status(
    env_var: &'static str,
    response: reqwest::Response,
) -> Result<reqwest::Response, ProviderError> {
    if response.status().is_success() {
        return Ok(response);
    }

    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    Err(map_status_error(env_var, status, &body))
}

pub(crate) fn looks_like_auth_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("unauthorized")
        || lower.contains("forbidden")
        || lower.contains("invalid_api_key")
        || lower.contains("invalid api key")
        || lower.contains("status code: 401")
        || lower.contains("status code: 403")
        || lower.contains("authentication_error")
}

#[derive(Debug, Clone)]
pub(crate) struct NormalizedToolResultMessage {
    pub call_id: String,
    pub output: String,
    pub is_error: bool,
}

pub(crate) fn normalize_tool_result_message(content: &str) -> NormalizedToolResultMessage {
    match serde_json::from_str::<ToolMessagePayload>(content) {
        Ok(payload) => {
            let call_id = normalize_tool_result_call_id(payload.call_id);
            let output = serde_json::to_string(&ModelToolMessagePayload {
                is_error: payload.is_error,
                output: payload.output.as_str(),
            })
            .unwrap_or_else(|_| content.to_string());

            NormalizedToolResultMessage {
                call_id,
                output,
                is_error: payload.is_error,
            }
        }
        Err(_) => NormalizedToolResultMessage {
            call_id: DEFAULT_TOOL_RESULT_CALL_ID.to_string(),
            output: content.to_string(),
            is_error: false,
        },
    }
}

#[derive(Debug, Deserialize)]
struct ToolMessagePayload {
    call_id: String,
    #[serde(default)]
    is_error: bool,
    output: String,
}

#[derive(Debug, Serialize)]
struct ModelToolMessagePayload<'a> {
    is_error: bool,
    output: &'a str,
}

fn normalize_tool_result_call_id(call_id: String) -> String {
    if call_id.trim().is_empty() {
        DEFAULT_TOOL_RESULT_CALL_ID.to_string()
    } else {
        call_id
    }
}

fn sanitized_error_body(body: &str) -> String {
    let collapsed = body.trim().replace('\n', " ");
    if collapsed.is_empty() {
        return String::new();
    }

    let mut truncated: String = collapsed.chars().take(MAX_ERROR_BODY_LEN).collect();
    if collapsed.chars().count() > MAX_ERROR_BODY_LEN {
        truncated.push_str("...");
    }

    truncated
}

fn is_auth_status(status_code: u16) -> bool {
    status_code == 401 || status_code == 403
}

pub trait Provider: Send + Sync {
    fn kind(&self) -> ProviderKind;

    fn stream(&self, request: ProviderRequest<'_>, cancel: CancellationToken) -> ProviderStream;
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn normalize_provider_request_extracts_system_and_chat_messages() {
        let messages = vec![
            Message::new(MessageRole::System, "system-a"),
            Message::new(MessageRole::System, "system-b"),
            Message::new(MessageRole::User, "first"),
            Message::new(MessageRole::Assistant, "second"),
        ];
        let tools = vec![ToolDefinition {
            name: "read".to_string(),
            description: "read file".to_string(),
            parameters: json!({ "type": "object" }),
        }];

        let request = ProviderRequest::new(ModelKind::Gpt52, &messages).with_tools(&tools);
        let normalized = normalize_provider_request(request).expect("request should normalize");

        assert_eq!(normalized.model, ModelKind::Gpt52.as_str());
        assert_eq!(normalized.system, Some("system-a\n\nsystem-b".to_string()));
        assert_eq!(normalized.messages.len(), 2);
        assert_eq!(normalized.messages[0].content, "first");
        assert_eq!(normalized.messages[1].content, "second");
        assert_eq!(normalized.tools.len(), 1);
        assert_eq!(normalized.tools[0].name, "read");
    }

    #[test]
    fn normalize_provider_request_requires_non_system_message() {
        let messages = vec![Message::new(MessageRole::System, "system-only")];
        let request = ProviderRequest::new(ModelKind::Gpt52, &messages);

        let error = normalize_provider_request(request).expect_err("request should fail");
        assert!(matches!(error, ProviderError::Transport(_)));
    }

    #[test]
    fn normalize_tool_result_message_preserves_call_id_and_output() {
        let normalized = normalize_tool_result_message(
            &json!({
                "call_id": "call-123",
                "is_error": true,
                "output": "result",
            })
            .to_string(),
        );

        assert_eq!(normalized.call_id, "call-123");
        assert!(normalized.is_error);
        assert_eq!(
            normalized.output,
            json!({ "is_error": true, "output": "result" }).to_string()
        );
    }

    #[test]
    fn normalize_tool_result_message_uses_fallback_for_non_json_content() {
        let normalized = normalize_tool_result_message("raw output");
        assert_eq!(normalized.call_id, "tool-result");
        assert_eq!(normalized.output, "raw output");
        assert!(!normalized.is_error);
    }

    #[test]
    fn validate_api_key_rejects_missing_value() {
        let error = validate_api_key("TEST_KEY", None).expect_err("missing key should fail");
        assert!(matches!(error, ProviderError::MissingApiKey("TEST_KEY")));
    }

    #[test]
    fn validate_api_key_rejects_blank_value() {
        let error = validate_api_key("TEST_KEY", Some("   ".to_string()))
            .expect_err("blank key should fail");
        assert!(matches!(error, ProviderError::InvalidApiKey("TEST_KEY")));
    }

    #[test]
    fn map_status_error_maps_auth_status_to_invalid_api_key() {
        let error = map_status_error("OPENAI_API_KEY", 401, "Unauthorized");
        assert!(matches!(
            error,
            ProviderError::InvalidApiKey("OPENAI_API_KEY")
        ));
    }
}
