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
    message::{Message, MessageRole},
    stream::ProviderEvent,
    tool::ToolDefinition,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
}

impl ProviderRequest {
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            messages,
            tools: Vec::new(),
        }
    }

    pub fn with_tools(mut self, tools: Vec<ToolDefinition>) -> Self {
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
    request: ProviderRequest,
) -> Result<RigChatRequest, ProviderError> {
    let mut system_sections = Vec::new();
    let mut chat_messages = Vec::new();

    for message in request.messages {
        match message.role {
            MessageRole::System => system_sections.push(message.content),
            MessageRole::User => {
                chat_messages.push(RigMessage::from(RigUserContent::text(message.content)))
            }
            MessageRole::Assistant => {
                chat_messages.push(RigMessage::from(RigAssistantContent::text(message.content)))
            }
            MessageRole::Tool => chat_messages.push(to_rig_tool_result_message(message.content)),
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
        model: request.model,
        prompt,
        history: chat_messages,
        preamble,
        tools: request
            .tools
            .into_iter()
            .map(|tool| RigToolDefinition {
                name: tool.name,
                description: tool.description,
                parameters: tool.parameters,
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
        StreamedAssistantContent::ToolCall { tool_call, .. } => Some(ProviderEvent::ToolCall {
            call: crate::tool::ToolCall {
                call_id: tool_call.call_id.unwrap_or(tool_call.id),
                name: tool_call.function.name,
                input: tool_call.function.arguments,
            },
        }),
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

    RigMessage::from(RigUserContent::tool_result(
        call_id,
        RigToolResultContent::from_tool_output(output),
    ))
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

    fn stream(&self, request: ProviderRequest) -> ProviderStream;
}

#[cfg(test)]
mod tests {
    use rig::completion::{Message as RigMessage, message::UserContent as RigUserContent};
    use serde_json::json;

    use super::*;
    use crate::Message;

    #[test]
    fn to_rig_chat_request_extracts_preamble_prompt_history_and_tools() {
        let request = ProviderRequest::new(
            "test-model",
            vec![
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
            ],
        )
        .with_tools(vec![ToolDefinition {
            name: "read".to_string(),
            description: "read file".to_string(),
            parameters: json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
            }),
        }]);

        let rig_request = to_rig_chat_request(request).expect("request conversion should succeed");

        assert_eq!(rig_request.model, "test-model");
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
        let request = ProviderRequest::new(
            "test-model",
            vec![Message::new(MessageRole::System, "only system")],
        );

        let error = to_rig_chat_request(request).expect_err("conversion should fail");
        assert!(matches!(error, ProviderError::Transport(_)));
    }
}
