//! OpenAI provider adapter and quarantine boundary for Nanocodex.
//!
//! The adapter creates a fresh lower-level Nanocodex session for every rho
//! request and supplies the complete transcript as typed input. Nanocodex's
//! agent loop and retained-history API never cross this crate boundary.

use std::num::NonZeroU32;

use async_stream::stream;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use futures_util::StreamExt as _;
use nanocodex_oai_api::{
    OpenAi, ResponseError, ResponseErrorKind, ResponseEvent as OpenAiEvent,
    Thinking as OpenAiThinking,
    responses::{
        ContentItem, FunctionOutputBody, JsonSchema, MessageRole,
        ReasoningContent as OpenAiReasoningContent, ReasoningSummary, ResponseItem,
        ToolDefinition as OpenAiToolDefinition,
    },
    session::ResponseInput,
    transport::ResponsesHistory,
};
use rho_ai::{
    AssistantMessage, CancellationToken, ContentBlock, DeltaKind, ErrorKind, Message, ModelId,
    ModelInfo, OpaqueBlob, Provider, ProviderError, ProviderId, ProviderStream, Request,
    StopReason, StreamEvent, ThinkingLevel, ToolArgumentError, ToolCallId, ToolDefinition, Usage,
    validate_tool_arguments, validate_tool_definition,
};

const PROVIDER: &str = "openai";
const MODEL: &str = nanocodex_oai_api::MODEL;

/// OpenAI adapter construction failure.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    /// API key was empty.
    #[error("OpenAI API key must not be empty")]
    EmptyApiKey,
}

/// Stateless OpenAI Responses adapter built on `nanocodex-oai-api`.
#[derive(Clone)]
pub struct OpenAiProvider {
    api_key: String,
    models: Vec<ModelInfo>,
}

impl std::fmt::Debug for OpenAiProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenAiProvider")
            .field("api_key", &"[REDACTED]")
            .field("models", &self.models)
            .finish()
    }
}

impl OpenAiProvider {
    /// Creates an adapter using API-key authentication.
    pub fn new(api_key: impl Into<String>) -> Result<Self, BuildError> {
        let api_key = api_key.into();
        if api_key.is_empty() {
            return Err(BuildError::EmptyApiKey);
        }
        Ok(Self {
            api_key,
            models: vec![ModelInfo {
                id: ModelId::from(MODEL),
                display_name: "GPT-5.6 Sol".to_owned(),
                context_tokens: Some(nanocodex_oai_api::CONTEXT_WINDOW_TOKENS),
                max_output_tokens: None,
            }],
        })
    }
}

impl Provider for OpenAiProvider {
    fn models(&self) -> &[ModelInfo] {
        &self.models
    }

    fn stream(&self, request: Request, cancellation: CancellationToken) -> ProviderStream {
        let api_key = self.api_key.clone();
        Box::pin(stream! {
            if request.model.as_str() != MODEL {
                yield StreamEvent::Error(invalid_request(format!(
                    "nanocodex-oai-api 0.3.0 supports {MODEL}, not {}",
                    request.model
                )));
                return;
            }
            if request.system.trim().is_empty() {
                yield StreamEvent::Error(invalid_request("system instructions must not be empty"));
                return;
            }
            if request.max_output_tokens == 0 {
                yield StreamEvent::Error(invalid_request(
                    "max_output_tokens must be greater than zero",
                ));
                return;
            }
            for tool in &request.tools {
                if let Err(error) = validate_tool_definition(tool) {
                    yield StreamEvent::Error(invalid_request(format!(
                        "tool {}: {error}", tool.name
                    )));
                    return;
                }
            }
            let items = match request_items(&request) {
                Ok(items) => items,
                Err(error) => {
                    yield StreamEvent::Error(error);
                    return;
                }
            };
            let tools = request.tools.iter().map(openai_tool).collect::<Vec<_>>();
            let openai = match OpenAi::builder(api_key)
                .max_attempts(NonZeroU32::MIN)
                .store(false)
                .history(ResponsesHistory::FullReplay)
                .thinking(openai_thinking(request.thinking))
                .build()
            {
                Ok(openai) => openai,
                Err(error) => {
                    yield StreamEvent::Error(ProviderError {
                        retryable: false,
                        kind: ErrorKind::Authentication,
                        message: error.to_string(),
                    });
                    return;
                }
            };
            let mut session = match openai
                .instructions(request.system.clone())
                .tool_definitions(tools)
                .build()
            {
                Ok(session) => session,
                Err(error) => {
                    yield StreamEvent::Error(invalid_request(error.to_string()));
                    return;
                }
            };
            let mut turn = session.turn();
            let mut response = turn.create(ResponseInput::items(items));
            let mut block_index = 0usize;
            loop {
                let next = tokio::select! {
                    () = cancellation.cancelled() => {
                        yield StreamEvent::Error(ProviderError::cancelled());
                        return;
                    }
                    next = response.next() => next,
                };
                let Some(next) = next else { break };
                let event = match next {
                    Ok(event) => event,
                    Err(error) => {
                        yield StreamEvent::Error(map_error(&error));
                        return;
                    }
                };
                match event {
                    OpenAiEvent::Created => yield StreamEvent::Start,
                    OpenAiEvent::OutputTextDelta(delta) => yield StreamEvent::Delta {
                        index: block_index,
                        kind: DeltaKind::Text,
                        delta,
                    },
                    OpenAiEvent::ToolCallInputDelta { delta, .. } => {
                        yield StreamEvent::Delta {
                            index: block_index,
                            kind: DeltaKind::ToolArguments,
                            delta,
                        };
                    }
                    OpenAiEvent::ReasoningSummaryDelta { delta, .. }
                    | OpenAiEvent::ReasoningContentDelta { delta, .. } => {
                        yield StreamEvent::Delta {
                            index: block_index,
                            kind: DeltaKind::Thinking,
                            delta,
                        };
                    }
                    OpenAiEvent::OutputItemDone(item) => {
                        let blocks = match response_item_blocks(&item, &request.tools) {
                            Ok(blocks) => blocks,
                            Err(error) => {
                                yield StreamEvent::Error(error);
                                return;
                            }
                        };
                        for block in blocks {
                            yield StreamEvent::BlockDone {
                                index: block_index,
                                block,
                            };
                            block_index += 1;
                        }
                    }
                    OpenAiEvent::OutputItemAdded(_)
                    | OpenAiEvent::ReasoningSummaryDone { .. }
                    | OpenAiEvent::ReasoningSummaryPartAdded { .. }
                    | OpenAiEvent::Completed { .. } => {}
                    _ => {}
                }
            }
            let completed = match response.await {
                Ok(completed) => completed,
                Err(error) => {
                    yield StreamEvent::Error(map_error(&error));
                    return;
                }
            };
            match completed_message(&completed, &request.tools) {
                Ok(message) => yield StreamEvent::Done(message),
                Err(error) => yield StreamEvent::Error(error),
            }
        })
    }
}

fn request_items(request: &Request) -> Result<Vec<ResponseItem>, ProviderError> {
    let mut items = Vec::new();
    for message in &request.messages {
        match message {
            Message::User { content } => {
                let content = content
                    .iter()
                    .map(openai_user_content)
                    .collect::<Result<Vec<_>, _>>()?;
                items.push(ResponseItem::message(MessageRole::User, content));
            }
            Message::Assistant(message) => {
                for block in &message.blocks {
                    match block {
                        ContentBlock::Text { text } => items.push(ResponseItem::message(
                            MessageRole::Assistant,
                            [ContentItem::output_text(text.clone())],
                        )),
                        ContentBlock::Thinking { text, opaque } => {
                            if let Some(opaque) = opaque.as_ref().filter(|opaque| {
                                opaque.provider == ProviderId::from(PROVIDER)
                                    && opaque.kind == "encrypted_content"
                            }) {
                                items.push(ResponseItem::Reasoning {
                                    id: None,
                                    summary: if text.is_empty() {
                                        Vec::new()
                                    } else {
                                        vec![ReasoningSummary::SummaryText {
                                            text: text.clone().into_boxed_str(),
                                        }]
                                    },
                                    content: None,
                                    encrypted_content: Some(opaque.data.clone().into_boxed_str()),
                                    status: None,
                                    internal_chat_message_metadata_passthrough: None,
                                });
                            } else if !text.is_empty() {
                                items.push(ResponseItem::message(
                                    MessageRole::Assistant,
                                    [ContentItem::output_text(text.clone())],
                                ));
                            }
                        }
                        ContentBlock::ToolCall { id, name, args } => {
                            items.push(function_call_item(id, name, args))
                        }
                        ContentBlock::RejectedToolCall {
                            id,
                            name,
                            args: Some(args),
                            ..
                        } => items.push(function_call_item(id, name, args)),
                        ContentBlock::RejectedToolCall { args: None, .. } => {}
                        ContentBlock::Image { .. } => {
                            return Err(invalid_request(
                                "OpenAI assistant history cannot contain image blocks",
                            ));
                        }
                        _ => {
                            return Err(invalid_request(
                                "unsupported assistant content block variant",
                            ));
                        }
                    }
                }
            }
            Message::ToolResult(result) => {
                items.push(ResponseItem::function_call_output(
                    result.call_id.as_str().to_owned(),
                    FunctionOutputBody::Text(result.content.clone().into_boxed_str()),
                ));
            }
            _ => return Err(invalid_request("unsupported transcript message variant")),
        }
    }
    Ok(items)
}

fn function_call_item(id: &ToolCallId, name: &str, args: &serde_json::Value) -> ResponseItem {
    ResponseItem::FunctionCall {
        id: None,
        name: name.to_owned().into_boxed_str(),
        namespace: None,
        arguments: args.to_string().into_boxed_str(),
        call_id: id.as_str().to_owned().into_boxed_str(),
        caller: None,
        status: None,
        created_by: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn openai_user_content(block: &ContentBlock) -> Result<ContentItem, ProviderError> {
    match block {
        ContentBlock::Text { text } => Ok(ContentItem::input_text(text.clone())),
        ContentBlock::Image { data, mime } => Ok(ContentItem::input_image(format!(
            "data:{mime};base64,{}",
            BASE64.encode(data)
        ))),
        _ => Err(invalid_request(
            "user messages may contain only text and image blocks",
        )),
    }
}

fn openai_tool(tool: &ToolDefinition) -> OpenAiToolDefinition {
    OpenAiToolDefinition::function(
        tool.name.clone(),
        tool.description.clone(),
        JsonSchema::from(tool.parameters.clone()),
    )
}

fn completed_message(
    completed: &nanocodex_oai_api::CompletedResponse,
    tools: &[ToolDefinition],
) -> Result<AssistantMessage, ProviderError> {
    let mut blocks = Vec::new();
    for item in completed.output() {
        blocks.extend(response_item_blocks(item, tools)?);
    }
    let tool_use = blocks.iter().any(|block| {
        matches!(
            block,
            ContentBlock::ToolCall { .. } | ContentBlock::RejectedToolCall { .. }
        )
    });
    let stop = if tool_use {
        StopReason::ToolUse
    } else if completed.end_turn() == Some(false) {
        StopReason::Paused
    } else {
        StopReason::Stop
    };
    let usage = completed
        .usage()
        .map_or_else(Usage::default, |usage| Usage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: usage
                .input_tokens_details
                .as_ref()
                .map_or(0, |details| details.cached_tokens),
            cache_write_tokens: usage
                .input_tokens_details
                .as_ref()
                .map_or(0, |details| details.cache_write_tokens),
        });
    Ok(AssistantMessage {
        blocks,
        stop,
        usage,
        provider: ProviderId::from(PROVIDER),
        model: ModelId::from(MODEL),
    })
}

fn response_item_blocks(
    item: &ResponseItem,
    tools: &[ToolDefinition],
) -> Result<Vec<ContentBlock>, ProviderError> {
    match item {
        ResponseItem::Message {
            role: MessageRole::Assistant,
            content,
            ..
        } => content
            .iter()
            .filter_map(|content| match content {
                ContentItem::OutputText { text, .. } => Some(Ok(ContentBlock::Text {
                    text: text.to_string(),
                })),
                _ => None,
            })
            .collect(),
        ResponseItem::Reasoning {
            summary,
            content,
            encrypted_content,
            ..
        } => {
            let mut text = summary
                .iter()
                .map(|summary| match summary {
                    ReasoningSummary::SummaryText { text } => text.as_ref(),
                })
                .collect::<Vec<_>>()
                .join("\n");
            if let Some(content) = content {
                for content in content {
                    let value = match content {
                        OpenAiReasoningContent::ReasoningText { text }
                        | OpenAiReasoningContent::Text { text } => text,
                    };
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(value);
                }
            }
            Ok(vec![ContentBlock::Thinking {
                text,
                opaque: encrypted_content.as_ref().map(|data| OpaqueBlob {
                    provider: ProviderId::from(PROVIDER),
                    kind: "encrypted_content".to_owned(),
                    data: data.to_string(),
                }),
            }])
        }
        ResponseItem::FunctionCall {
            name,
            arguments,
            call_id,
            ..
        } => {
            let arguments: serde_json::Value =
                serde_json::from_str(arguments).map_err(|error| {
                    ProviderError::invalid_response(format!(
                        "tool {name} emitted malformed arguments: {error}"
                    ))
                })?;
            let id = ToolCallId::from(call_id.as_ref());
            let Some(tool) = tools.iter().find(|tool| tool.name == name.as_ref()) else {
                return Ok(vec![ContentBlock::RejectedToolCall {
                    id,
                    name: name.to_string(),
                    args: Some(arguments),
                    error: ToolArgumentError {
                        kind: "unknown_tool".to_owned(),
                        message: "provider requested a tool that was not declared".to_owned(),
                    },
                }]);
            };
            match validate_tool_arguments(tool, &arguments) {
                Ok(()) => Ok(vec![ContentBlock::ToolCall {
                    id,
                    name: name.to_string(),
                    args: arguments,
                }]),
                Err(error) => Ok(vec![ContentBlock::RejectedToolCall {
                    id,
                    name: name.to_string(),
                    args: Some(arguments),
                    error,
                }]),
            }
        }
        ResponseItem::Other(value) => Err(ProviderError::invalid_response(format!(
            "OpenAI returned an unknown output item: {value:?}"
        ))),
        ResponseItem::AdditionalTools { .. }
        | ResponseItem::Message { .. }
        | ResponseItem::AgentMessage { .. }
        | ResponseItem::LocalShellCall { .. }
        | ResponseItem::FunctionCallOutput { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::CustomToolCallOutput { .. }
        | ResponseItem::ToolSearchOutput { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ImageGenerationCall { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::CompactionTrigger {}
        | ResponseItem::ContextCompaction { .. } => Ok(Vec::new()),
    }
}

fn map_error(error: &ResponseError) -> ProviderError {
    let (kind, retryable) = match error.kind() {
        ResponseErrorKind::ContextWindowExceeded => (ErrorKind::ContextWindowExceeded, false),
        ResponseErrorKind::Protocol => (ErrorKind::InvalidResponse, false),
        ResponseErrorKind::Service => {
            let retryable = error
                .responses_error()
                .and_then(nanocodex_oai_api::transport::ResponsesError::retry_advice)
                .is_some();
            let kind = error
                .responses_error()
                .map_or(ErrorKind::Transport, |error| match error.class() {
                    "authorization" | "invalid_authorization" => ErrorKind::Authentication,
                    "https_rate_limit" | "handshake_rate_limit" => ErrorKind::RateLimited,
                    "context_window_exceeded" => ErrorKind::ContextWindowExceeded,
                    "invalid_json" | "invalid_payload" | "invalid_sse_utf8" => {
                        ErrorKind::InvalidResponse
                    }
                    _ => ErrorKind::Transport,
                });
            (kind, retryable)
        }
        _ => (ErrorKind::Other, false),
    };
    ProviderError {
        retryable,
        kind,
        message: error.to_string(),
    }
}

fn openai_thinking(level: ThinkingLevel) -> OpenAiThinking {
    match level {
        ThinkingLevel::None => OpenAiThinking::None,
        ThinkingLevel::Low => OpenAiThinking::Low,
        ThinkingLevel::Medium => OpenAiThinking::Medium,
        ThinkingLevel::High => OpenAiThinking::High,
        ThinkingLevel::Xhigh => OpenAiThinking::Xhigh,
        ThinkingLevel::Max => OpenAiThinking::Max,
    }
}

fn invalid_request(message: impl Into<String>) -> ProviderError {
    ProviderError {
        retryable: false,
        kind: ErrorKind::InvalidRequest,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn bash_tool() -> ToolDefinition {
        ToolDefinition::new(
            "bash",
            "Run one shell command.",
            json!({
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"],
                "additionalProperties": false
            }),
        )
    }

    #[test]
    fn recorded_function_call_is_parsed_and_schema_validated() {
        let item: ResponseItem = serde_json::from_value(json!({
            "type": "function_call",
            "name": "bash",
            "arguments": "{\"command\":\"pwd\"}",
            "call_id": "call-1"
        }))
        .unwrap();
        let blocks = response_item_blocks(&item, &[bash_tool()]).unwrap();
        assert!(matches!(
            &blocks[0],
            ContentBlock::ToolCall { id, name, args }
                if id.as_str() == "call-1" && name == "bash" && args["command"] == "pwd"
        ));
    }

    #[test]
    fn recorded_nonconforming_call_is_not_coerced() {
        let item: ResponseItem = serde_json::from_value(json!({
            "type": "function_call",
            "name": "bash",
            "arguments": "{\"command\":5}",
            "call_id": "call-1"
        }))
        .unwrap();
        let blocks = response_item_blocks(&item, &[bash_tool()]).unwrap();
        assert!(matches!(
            &blocks[0],
            ContentBlock::RejectedToolCall { error, .. }
                if error.kind == "schema_validation"
        ));
    }

    #[test]
    fn fresh_session_input_contains_complete_transcript_and_drops_foreign_opaque() {
        let request = Request {
            system: "test".to_owned(),
            messages: vec![
                Message::user("first"),
                Message::Assistant(AssistantMessage {
                    blocks: vec![ContentBlock::Thinking {
                        text: "summary".to_owned(),
                        opaque: Some(OpaqueBlob {
                            provider: ProviderId::from("anthropic"),
                            kind: "signature".to_owned(),
                            data: "foreign-secret".to_owned(),
                        }),
                    }],
                    stop: StopReason::Stop,
                    usage: Usage::default(),
                    provider: ProviderId::from("anthropic"),
                    model: ModelId::from("claude-sonnet-5"),
                }),
                Message::user("second"),
            ],
            tools: Vec::new(),
            model: ModelId::from(MODEL),
            max_output_tokens: 1024,
            thinking: ThinkingLevel::High,
        };
        let encoded = serde_json::to_string(&request_items(&request).unwrap()).unwrap();
        assert!(encoded.contains("first"));
        assert!(encoded.contains("second"));
        assert!(encoded.contains("summary"));
        assert!(!encoded.contains("foreign-secret"));
    }

    #[test]
    fn provider_debug_redacts_api_key() {
        let provider = OpenAiProvider::new("secret-key").unwrap();
        let debug = format!("{provider:?}");
        assert!(!debug.contains("secret-key"));
        assert!(debug.contains("REDACTED"));
    }
}
