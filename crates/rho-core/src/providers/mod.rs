use std::{
    pin::Pin,
    sync::{Arc, OnceLock},
};

use futures_core::Stream;
use futures_util::StreamExt;
use rig::{
    OneOrMany,
    client::completion::CompletionClient,
    completion::{
        AssistantContent as RigAssistantContent, CompletionError as RigCompletionError,
        CompletionModel as RigCompletionModel,
        CompletionRequestBuilder as RigCompletionRequestBuilder, GetTokenUsage,
        Message as RigMessage, ToolDefinition as RigToolDefinition,
        message::{ToolResultContent as RigToolResultContent, UserContent as RigUserContent},
    },
    http_client::Error as RigHttpError,
    providers::{anthropic, openai},
    streaming::{StreamedAssistantContent, StreamingCompletionResponse},
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
pub use tokio_util::sync::CancellationToken;

use crate::{
    message::{Message, MessageRole, decode_assistant_message_content},
    stream::ProviderEvent,
    tool::{ToolCall, ToolDefinition},
};

const DEFAULT_TOOL_RESULT_CALL_ID: &str = "tool-result";
const OPENAI_API_KEY_ENV: &str = "OPENAI_API_KEY";
const OPENAI_BASE_URL_ENV: &str = "OPENAI_BASE_URL";
const ANTHROPIC_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";

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

#[derive(Debug, Clone)]
pub struct RigProvider {
    kind: ProviderKind,
    client: Arc<OnceLock<RigClient>>,
}

impl RigProvider {
    pub fn new(kind: ProviderKind) -> Self {
        Self {
            kind,
            client: Arc::new(OnceLock::new()),
        }
    }

    fn client(&self) -> Result<RigClient, ProviderError> {
        if let Some(client) = self.client.get() {
            return Ok(client.clone());
        }

        let client = build_rig_client(self.kind)?;
        let _ = self.client.set(client);

        Ok(self
            .client
            .get()
            .expect("provider client should be initialized")
            .clone())
    }
}

impl Provider for RigProvider {
    fn kind(&self) -> ProviderKind {
        self.kind
    }

    fn stream(&self, request: ProviderRequest<'_>, cancel: CancellationToken) -> ProviderStream {
        let rig_request = to_rig_chat_request(request);
        let client = self.client();

        Box::pin(async_stream::try_stream! {
            let client = client?;
            let rig_request = rig_request?;

            match client {
                RigClient::OpenAi(client) => {
                    let builder = client
                        .completion_model(rig_request.model.clone())
                        .completion_request(rig_request.prompt)
                        .messages(rig_request.history);
                    let builder = apply_common_request_options(
                        builder,
                        rig_request.preamble,
                        rig_request.tools,
                    );

                    let stream = builder
                        .stream()
                        .await
                        .map_err(|error| map_rig_completion_error(OPENAI_API_KEY_ENV, error))?;

                    for await event in
                        stream_from_response(stream, OPENAI_API_KEY_ENV, cancel)
                    {
                        yield event?;
                    }
                }
                RigClient::Anthropic(client) => {
                    let model = client
                        .completion_model(rig_request.model.clone())
                        .with_prompt_caching();
                    let builder = model
                        .completion_request(rig_request.prompt)
                        .messages(rig_request.history);
                    let builder = apply_common_request_options(
                        builder,
                        rig_request.preamble,
                        rig_request.tools,
                    );

                    let stream = builder
                        .stream()
                        .await
                        .map_err(|error| map_rig_completion_error(ANTHROPIC_API_KEY_ENV, error))?;

                    for await event in
                        stream_from_response(stream, ANTHROPIC_API_KEY_ENV, cancel)
                    {
                        yield event?;
                    }
                }
            }
        })
    }
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

#[derive(Debug, Clone)]
enum RigClient {
    OpenAi(openai::Client),
    Anthropic(anthropic::Client),
}

fn build_rig_client(kind: ProviderKind) -> Result<RigClient, ProviderError> {
    match kind {
        ProviderKind::OpenAi => {
            let api_key = openai_api_key()?;
            Ok(RigClient::OpenAi(openai_client(api_key)?))
        }
        ProviderKind::Anthropic => {
            let api_key = anthropic_api_key()?;
            Ok(RigClient::Anthropic(anthropic_client(api_key)?))
        }
    }
}

fn openai_api_key() -> Result<String, ProviderError> {
    validate_api_key(OPENAI_API_KEY_ENV, std::env::var(OPENAI_API_KEY_ENV).ok())
}

fn openai_client(api_key: String) -> Result<openai::Client, ProviderError> {
    let mut builder = openai::Client::builder().api_key(api_key);

    if let Ok(base_url) = std::env::var(OPENAI_BASE_URL_ENV)
        && !base_url.trim().is_empty()
    {
        builder = builder.base_url(&base_url);
    }

    builder
        .build()
        .map_err(|error| map_rig_http_error(OPENAI_API_KEY_ENV, error))
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
                chat_messages.push(to_rig_assistant_message(&message.content));
            }
            MessageRole::Tool => chat_messages.push(to_rig_tool_result_message(&message.content)),
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

pub(crate) fn stream_from_response<R>(
    mut stream: StreamingCompletionResponse<R>,
    env_var: &'static str,
    cancel: CancellationToken,
) -> ProviderStream
where
    R: Clone + Unpin + GetTokenUsage + Send + 'static,
{
    Box::pin(async_stream::try_stream! {
        loop {
            let next = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    stream.cancel();
                    return;
                }
                next = stream.next() => next,
            };
            match next {
                Some(Ok(chunk)) => {
                    if let Some(event) = map_streamed_assistant_chunk(chunk) {
                        yield event;
                    }
                }
                Some(Err(error)) => {
                    Err(map_rig_completion_error(env_var, error))?;
                }
                None => break,
            }
        }

        let message = Message::new(MessageRole::Assistant, rig_choice_text(&stream.choice));
        yield ProviderEvent::Message { message };
        yield ProviderEvent::Finished;
    })
}

fn to_rig_tool_result_message(content: &str) -> RigMessage {
    let (call_id, output) = match serde_json::from_str::<ToolMessagePayload>(content) {
        Ok(payload) => {
            let call_id = normalize_tool_result_call_id(payload.call_id);
            let output = serde_json::to_string(&RigToolMessagePayload {
                is_error: payload.is_error,
                output: payload.output.as_str(),
            })
            .unwrap_or_else(|_| content.to_string());
            (call_id, output)
        }
        Err(_) => (DEFAULT_TOOL_RESULT_CALL_ID.to_string(), content.to_string()),
    };

    RigMessage::from(RigUserContent::tool_result_with_call_id(
        call_id.clone(),
        call_id,
        RigToolResultContent::from_tool_output(output),
    ))
}

fn to_rig_assistant_message(content: &str) -> RigMessage {
    let parsed = decode_assistant_message_content(content);
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

#[derive(Debug, Serialize)]
struct RigToolMessagePayload<'a> {
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

fn is_auth_status(status_code: u16) -> bool {
    status_code == 401 || status_code == 403
}

pub trait Provider: Send + Sync {
    fn kind(&self) -> ProviderKind;

    fn stream(&self, request: ProviderRequest<'_>, cancel: CancellationToken) -> ProviderStream;
}

#[cfg(test)]
mod tests {
    use rig::{
        completion::{
            AssistantContent as RigAssistantContent, Message as RigMessage,
            message::{ToolResultContent as RigToolResultContent, UserContent as RigUserContent},
        },
        message::{ToolCall as RigToolCall, ToolFunction as RigToolFunction},
        streaming::StreamedAssistantContent,
    };
    use serde_json::{Value, json};

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
    fn to_rig_chat_request_normalizes_blank_tool_result_call_id() {
        let messages = vec![
            Message::new(MessageRole::User, "first"),
            Message::new(
                MessageRole::Tool,
                json!({
                    "call_id": "   ",
                    "is_error": true,
                    "output": "boom",
                })
                .to_string(),
            ),
            Message::new(MessageRole::User, "next"),
        ];
        let request = ProviderRequest::new(ModelKind::Gpt52, &messages);

        let rig_request = to_rig_chat_request(request).expect("request conversion should succeed");

        let RigMessage::User { content } = &rig_request.history[1] else {
            panic!("expected tool result user message");
        };
        let RigUserContent::ToolResult(tool_result) = content.first_ref() else {
            panic!("expected tool result payload");
        };
        assert_eq!(tool_result.id, DEFAULT_TOOL_RESULT_CALL_ID);
        assert_eq!(
            tool_result.call_id.as_deref(),
            Some(DEFAULT_TOOL_RESULT_CALL_ID)
        );

        let RigToolResultContent::Text(text) = tool_result.content.first_ref() else {
            panic!("expected text tool result content");
        };
        let payload: Value =
            serde_json::from_str(&text.text).expect("tool result payload should be JSON");
        assert_eq!(payload, json!({ "is_error": true, "output": "boom" }));
    }

    #[test]
    fn map_streamed_assistant_chunk_preserves_distinct_tool_call_id_and_call_id() {
        let chunk = StreamedAssistantContent::<()>::ToolCall {
            tool_call: RigToolCall::new(
                "fc_123".to_string(),
                RigToolFunction::new("read".to_string(), json!({ "path": "README.md" })),
            )
            .with_call_id("call_123".to_string()),
            internal_call_id: "internal-call-1".to_string(),
        };

        let event = map_streamed_assistant_chunk(chunk).expect("tool call chunk should map");
        let ProviderEvent::ToolCall { call } = event else {
            panic!("expected tool call event");
        };
        assert_eq!(call.id.as_deref(), Some("fc_123"));
        assert_eq!(call.call_id, "call_123");
        assert_eq!(call.name, "read");
        assert_eq!(call.input, json!({ "path": "README.md" }));
    }

    #[test]
    fn map_streamed_assistant_chunk_uses_tool_call_id_when_call_id_missing() {
        let chunk = StreamedAssistantContent::<()>::ToolCall {
            tool_call: RigToolCall::new(
                "fc_456".to_string(),
                RigToolFunction::new("write".to_string(), json!({ "path": "README.md" })),
            ),
            internal_call_id: "internal-call-2".to_string(),
        };

        let event = map_streamed_assistant_chunk(chunk).expect("tool call chunk should map");
        let ProviderEvent::ToolCall { call } = event else {
            panic!("expected tool call event");
        };
        assert_eq!(call.id.as_deref(), Some("fc_456"));
        assert_eq!(call.call_id, "fc_456");
        assert_eq!(call.name, "write");
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
    fn validate_api_key_rejects_missing_value() {
        let error = validate_api_key("TEST_KEY", None).expect_err("missing API key should fail");
        assert!(matches!(error, ProviderError::MissingApiKey("TEST_KEY")));
    }

    #[test]
    fn validate_api_key_rejects_blank_value() {
        let error = validate_api_key("TEST_KEY", Some("   ".to_string()))
            .expect_err("blank API key should fail");
        assert!(matches!(error, ProviderError::InvalidApiKey("TEST_KEY")));
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
