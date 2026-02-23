use std::pin::Pin;

use futures_core::Stream;
use rig::{
    OneOrMany,
    completion::{
        AssistantContent as RigAssistantContent, CompletionError as RigCompletionError,
        CompletionModel as RigCompletionModel,
        CompletionRequestBuilder as RigCompletionRequestBuilder, Message as RigMessage,
        ToolDefinition as RigToolDefinition,
        message::{ToolResultContent as RigToolResultContent, UserContent as RigUserContent},
    },
    http_client::Error as RigHttpError,
    streaming::StreamedAssistantContent,
};
use serde::Deserialize;
use thiserror::Error;

use crate::{
    message::{Message, MessageRole, decode_assistant_message_content},
    stream::ProviderEvent,
    tool::{ToolCall, ToolDefinition},
};

pub mod anthropic;
pub mod openai;

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

#[derive(Debug)]
pub(crate) struct RigChatRequest {
    pub model: String,
    pub prompt: RigMessage,
    pub history: Vec<RigMessage>,
    pub preamble: Option<String>,
    pub tools: Vec<RigToolDefinition>,
}

pub(crate) fn to_rig_chat_request(
    request: ProviderRequest<'_>,
) -> Result<RigChatRequest, ProviderError> {
    let mut system_sections = Vec::new();
    let mut chat_messages = Vec::new();

    for message in request.messages {
        match message.role {
            MessageRole::System => system_sections.push(message.content.clone()),
            MessageRole::User => chat_messages.push(RigMessage::from(RigUserContent::text(
                message.content.clone(),
            ))),
            MessageRole::Assistant => {
                chat_messages.push(to_rig_assistant_message(message.content.clone()));
            }
            MessageRole::Tool => {
                chat_messages.push(to_rig_tool_result_message(message.content.clone()))
            }
        }
    }

    let Some(prompt) = chat_messages.pop() else {
        return Err(ProviderError::Transport(
            "provider request must include at least one non-system message".to_string(),
        ));
    };

    let preamble = if system_sections.is_empty() {
        None
    } else {
        Some(system_sections.join("\n\n"))
    };

    Ok(RigChatRequest {
        model: request.model.to_string(),
        prompt,
        history: chat_messages,
        preamble,
        tools: request
            .tools
            .iter()
            .map(|tool| RigToolDefinition {
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters: tool.parameters.clone(),
            })
            .collect(),
    })
}

pub(crate) fn rig_choice_text(choice: &OneOrMany<RigAssistantContent>) -> String {
    choice
        .iter()
        .filter_map(|item| match item {
            RigAssistantContent::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

pub(crate) fn map_rig_completion_error(
    env_var: &'static str,
    error: RigCompletionError,
) -> ProviderError {
    match error {
        RigCompletionError::HttpError(http_error) => map_rig_http_error(env_var, http_error),
        RigCompletionError::ProviderError(message) if looks_like_auth_error(&message) => {
            ProviderError::InvalidApiKey(env_var)
        }
        other => ProviderError::Transport(other.to_string()),
    }
}

pub(crate) fn map_rig_http_error(env_var: &'static str, error: RigHttpError) -> ProviderError {
    match error {
        RigHttpError::InvalidStatusCode(status)
        | RigHttpError::InvalidStatusCodeWithMessage(status, _)
            if is_auth_status(status.as_u16()) =>
        {
            ProviderError::InvalidApiKey(env_var)
        }
        other => ProviderError::Transport(other.to_string()),
    }
}

fn looks_like_auth_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("unauthorized")
        || lower.contains("forbidden")
        || lower.contains("invalid_api_key")
        || lower.contains("status code: 401")
        || lower.contains("status code: 403")
}

pub(crate) fn map_streamed_assistant_chunk<R>(
    chunk: StreamedAssistantContent<R>,
) -> Option<ProviderEvent> {
    match chunk {
        StreamedAssistantContent::Text(text) if !text.text.is_empty() => {
            Some(ProviderEvent::AssistantDelta { delta: text.text })
        }
        StreamedAssistantContent::ToolCall { tool_call, .. } => {
            let id = tool_call.id;
            let call_id = tool_call.call_id.unwrap_or_else(|| id.clone());
            Some(ProviderEvent::ToolCall {
                call: crate::tool::ToolCall {
                    id: Some(id),
                    call_id,
                    name: tool_call.function.name,
                    input: tool_call.function.arguments,
                },
            })
        }
        _ => None,
    }
}

pub(crate) fn apply_common_request_options<M>(
    builder: RigCompletionRequestBuilder<M>,
    preamble: Option<String>,
    tools: Vec<RigToolDefinition>,
) -> RigCompletionRequestBuilder<M>
where
    M: RigCompletionModel,
{
    let builder = if let Some(preamble) = preamble {
        builder.preamble(preamble)
    } else {
        builder
    };

    if tools.is_empty() {
        builder
    } else {
        builder.tools(tools)
    }
}

fn to_rig_tool_result_message(content: String) -> RigMessage {
    let (call_id, output) = match serde_json::from_str::<ToolMessagePayload>(&content) {
        Ok(payload) => {
            let call_id = if payload.call_id.trim().is_empty() {
                "tool-result".to_string()
            } else {
                payload.call_id
            };
            let output = serde_json::json!({
                "is_error": payload.is_error,
                "output": payload.output,
            })
            .to_string();
            (call_id, output)
        }
        Err(_) => ("tool-result".to_string(), content),
    };

    RigMessage::from(RigUserContent::tool_result_with_call_id(
        call_id.clone(),
        call_id,
        RigToolResultContent::from_tool_output(output),
    ))
}

fn to_rig_assistant_message(content: String) -> RigMessage {
    let parsed = decode_assistant_message_content(&content);
    if parsed.tool_calls.is_empty() {
        return RigMessage::from(RigAssistantContent::text(parsed.text));
    }

    let mut assistant_content = Vec::new();
    if !parsed.text.is_empty() {
        assistant_content.push(RigAssistantContent::text(parsed.text));
    }
    assistant_content.extend(
        parsed
            .tool_calls
            .into_iter()
            .map(to_rig_assistant_tool_call),
    );

    RigMessage::Assistant {
        id: None,
        content: OneOrMany::many(assistant_content)
            .expect("assistant message with tool calls should not be empty"),
    }
}

fn to_rig_assistant_tool_call(call: ToolCall) -> RigAssistantContent {
    let id = call.id.unwrap_or_else(|| call.call_id.clone());
    RigAssistantContent::tool_call_with_call_id(id, call.call_id, call.name, call.input)
}

#[derive(Debug, Deserialize)]
struct ToolMessagePayload {
    call_id: String,
    #[serde(default)]
    is_error: bool,
    output: String,
}

fn is_auth_status(status_code: u16) -> bool {
    status_code == 401 || status_code == 403
}

pub trait Provider: Send + Sync {
    fn kind(&self) -> ProviderKind;

    fn stream(&self, request: ProviderRequest<'_>) -> ProviderStream;
}

#[cfg(test)]
mod tests {
    use rig::completion::{
        AssistantContent as RigAssistantContent, Message as RigMessage,
        message::UserContent as RigUserContent,
    };
    use serde_json::json;

    use super::*;
    use crate::{Message, message::encode_assistant_message_content};

    #[test]
    fn to_rig_chat_request_extracts_preamble_prompt_history_and_tools() {
        let messages = vec![
            Message::new(MessageRole::System, "system-a"),
            Message::new(MessageRole::System, "system-b"),
            Message::new(MessageRole::User, "first"),
            Message::new(MessageRole::Assistant, "second"),
            Message::new(
                MessageRole::Tool,
                json!({
                    "call_id": "call-1",
                    "is_error": false,
                    "output": "file-content",
                })
                .to_string(),
            ),
            Message::new(MessageRole::User, "final-prompt"),
        ];
        let tools = vec![ToolDefinition {
            name: "read".to_string(),
            description: "read file".to_string(),
            parameters: json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
            }),
        }];
        let request = ProviderRequest::new(ModelKind::Gpt52, &messages).with_tools(&tools);

        let rig_request = to_rig_chat_request(request).expect("request conversion should succeed");

        assert_eq!(rig_request.model, ModelKind::Gpt52.as_str());
        assert_eq!(
            rig_request.preamble,
            Some("system-a\n\nsystem-b".to_string())
        );
        assert_eq!(rig_request.history.len(), 3);
        assert_eq!(rig_request.tools.len(), 1);
        assert_eq!(rig_request.tools[0].name, "read");

        assert!(matches!(
            &rig_request.prompt,
            RigMessage::User { content }
            if matches!(content.first_ref(), RigUserContent::Text(text) if text.text == "final-prompt")
        ));
    }

    #[test]
    fn to_rig_chat_request_requires_a_non_system_prompt() {
        let messages = vec![Message::new(MessageRole::System, "only system")];
        let request = ProviderRequest::new(ModelKind::Gpt52, &messages);

        let error = to_rig_chat_request(request).expect_err("conversion should fail");
        assert!(matches!(error, ProviderError::Transport(_)));
    }

    #[test]
    fn to_rig_chat_request_preserves_tool_result_call_id() {
        let messages = vec![
            Message::new(MessageRole::User, "first"),
            Message::new(
                MessageRole::Tool,
                json!({
                    "call_id": "call-123",
                    "is_error": false,
                    "output": "result",
                })
                .to_string(),
            ),
            Message::new(MessageRole::User, "next"),
        ];
        let request = ProviderRequest::new(ModelKind::Gpt52, &messages);

        let rig_request = to_rig_chat_request(request).expect("request conversion should succeed");
        assert!(matches!(
            &rig_request.history[1],
            RigMessage::User { content }
                if matches!(
                    content.first_ref(),
                    RigUserContent::ToolResult(tool_result)
                        if tool_result.id == "call-123"
                            && tool_result.call_id.as_deref() == Some("call-123")
                )
        ));
    }

    #[test]
    fn to_rig_chat_request_preserves_assistant_tool_calls() {
        let messages = vec![
            Message::new(MessageRole::User, "first"),
            Message::new(
                MessageRole::Assistant,
                encode_assistant_message_content(
                    "",
                    &[ToolCall {
                        id: Some("toolu_123".to_string()),
                        call_id: "toolu_123".to_string(),
                        name: "read".to_string(),
                        input: json!({ "path": "README.md" }),
                    }],
                ),
            ),
            Message::new(
                MessageRole::Tool,
                json!({
                    "call_id": "toolu_123",
                    "is_error": false,
                    "output": "ok",
                })
                .to_string(),
            ),
            Message::new(MessageRole::User, "next"),
        ];
        let request = ProviderRequest::new(ModelKind::Gpt52, &messages);

        let rig_request = to_rig_chat_request(request).expect("request conversion should succeed");
        assert!(matches!(
            &rig_request.history[1],
            RigMessage::Assistant { content, .. }
                if matches!(
                    content.first_ref(),
                    RigAssistantContent::ToolCall(tool_call)
                        if tool_call.id == "toolu_123"
                            && tool_call.call_id.as_deref() == Some("toolu_123")
                            && tool_call.function.name == "read"
                            && tool_call.function.arguments == json!({ "path": "README.md" })
                )
        ));
    }

    #[test]
    fn to_rig_chat_request_preserves_distinct_tool_call_id_and_call_id() {
        let messages = vec![
            Message::new(MessageRole::User, "first"),
            Message::new(
                MessageRole::Assistant,
                encode_assistant_message_content(
                    "",
                    &[ToolCall {
                        id: Some("fc_123".to_string()),
                        call_id: "call_123".to_string(),
                        name: "read".to_string(),
                        input: json!({ "path": "README.md" }),
                    }],
                ),
            ),
            Message::new(
                MessageRole::Tool,
                json!({
                    "call_id": "call_123",
                    "is_error": false,
                    "output": "ok",
                })
                .to_string(),
            ),
            Message::new(MessageRole::User, "next"),
        ];
        let request = ProviderRequest::new(ModelKind::Gpt52, &messages);

        let rig_request = to_rig_chat_request(request).expect("request conversion should succeed");
        assert!(matches!(
            &rig_request.history[1],
            RigMessage::Assistant { content, .. }
                if matches!(
                    content.first_ref(),
                    RigAssistantContent::ToolCall(tool_call)
                        if tool_call.id == "fc_123"
                            && tool_call.call_id.as_deref() == Some("call_123")
                )
        ));
    }
}
