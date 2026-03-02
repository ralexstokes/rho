use std::collections::{BTreeMap, HashMap};

use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    message::{Message, MessageRole, decode_assistant_message_content},
    providers::{
        CancellationToken, Provider, ProviderError, ProviderKind, ProviderRequest, ProviderStream,
        looks_like_auth_error, map_reqwest_error, map_status_error, normalize_provider_request,
        normalize_tool_result_message, shared_http_client, validate_api_key,
    },
    stream::ProviderEvent,
    tool::{ToolCall, ToolDefinition},
};

const ANTHROPIC_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";

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

    fn stream(&self, request: ProviderRequest<'_>, cancel: CancellationToken) -> ProviderStream {
        let request_body = build_anthropic_request(request);
        let client = shared_http_client();
        let api_key = anthropic_api_key();

        Box::pin(async_stream::try_stream! {
            let request_body = request_body?;
            let client = client?;
            let api_key = api_key?;

            let response = client
                .post(ANTHROPIC_MESSAGES_URL)
                .header("x-api-key", api_key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header(reqwest::header::ACCEPT, "text/event-stream")
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .json(&request_body)
                .send()
                .await
                .map_err(map_reqwest_error)?;

            let mut event_stream = if response.status().is_success() {
                response.bytes_stream().eventsource()
            } else {
                let status = response.status().as_u16();
                let body = response.text().await.unwrap_or_default();
                Err(map_status_error(ANTHROPIC_API_KEY_ENV, status, &body))?;
                unreachable!("error path should have returned");
            };
            let mut pending_tool_calls = BTreeMap::<usize, PendingAnthropicToolCall>::new();
            let mut assistant_text = String::new();

            loop {
                let next = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return,
                    next = event_stream.next() => next,
                };

                let Some(next) = next else {
                    break;
                };

                let event = next.map_err(|error| ProviderError::Transport(error.to_string()))?;
                let data = event.data.trim();
                if data.is_empty() {
                    continue;
                }

                let stream_event = serde_json::from_str::<AnthropicStreamEvent>(data).map_err(|error| {
                    ProviderError::Transport(format!(
                        "failed to parse anthropic stream payload: {error}"
                    ))
                })?;

                match stream_event {
                    AnthropicStreamEvent::MessageStart { .. }
                    | AnthropicStreamEvent::Ping
                    | AnthropicStreamEvent::Unknown => {}
                    AnthropicStreamEvent::MessageDelta { delta } => {
                        if delta.stop_reason.is_some() {
                            break;
                        }
                    }
                    AnthropicStreamEvent::MessageStop => {
                        break;
                    }
                    AnthropicStreamEvent::ContentBlockStart {
                        index,
                        content_block,
                    } => match content_block {
                        AnthropicContentBlockStart::Text { text } => {
                            if !text.is_empty() {
                                assistant_text.push_str(&text);
                                yield ProviderEvent::AssistantDelta { delta: text };
                            }
                        }
                        AnthropicContentBlockStart::ToolUse { id, name, input } => {
                            pending_tool_calls.insert(
                                index,
                                PendingAnthropicToolCall {
                                    id,
                                    name,
                                    start_input: input,
                                    input_json: String::new(),
                                },
                            );
                        }
                        AnthropicContentBlockStart::Unknown => {}
                    },
                    AnthropicStreamEvent::ContentBlockDelta { index, delta } => match delta {
                        AnthropicContentBlockDelta::TextDelta { text } => {
                            if !text.is_empty() {
                                assistant_text.push_str(&text);
                                yield ProviderEvent::AssistantDelta { delta: text };
                            }
                        }
                        AnthropicContentBlockDelta::InputJsonDelta { partial_json } => {
                            if let Some(pending) = pending_tool_calls.get_mut(&index) {
                                pending.input_json.push_str(&partial_json);
                            }
                        }
                        AnthropicContentBlockDelta::Unknown => {}
                    },
                    AnthropicStreamEvent::ContentBlockStop { index } => {
                        if let Some(pending) = pending_tool_calls.remove(&index) {
                            let call = finalize_pending_anthropic_tool_call(index, pending)?;
                            yield ProviderEvent::ToolCall { call };
                        }
                    }
                    AnthropicStreamEvent::Error { error } => {
                        let message = if error.message.trim().is_empty() {
                            error.r#type
                        } else {
                            error.message
                        };

                        if looks_like_auth_error(&message) {
                            Err(ProviderError::InvalidApiKey(ANTHROPIC_API_KEY_ENV))?;
                        }

                        Err(ProviderError::Transport(message))?;
                    }
                }
            }

            for (index, pending) in pending_tool_calls {
                let call = finalize_pending_anthropic_tool_call(index, pending)?;
                yield ProviderEvent::ToolCall { call };
            }

            let message = Message::new(MessageRole::Assistant, assistant_text);
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

fn build_anthropic_request(
    request: ProviderRequest<'_>,
) -> Result<AnthropicRequest, ProviderError> {
    let normalized = normalize_provider_request(request)?;

    let mut system = if let Some(system) = normalized.system {
        vec![AnthropicSystemContent::Text {
            text: system,
            cache_control: None,
        }]
    } else {
        Vec::new()
    };

    let mut messages = Vec::new();
    let mut provider_tool_ids = HashMap::<String, String>::new();

    for message in normalized.messages {
        match message.role {
            MessageRole::System => {}
            MessageRole::User => {
                messages.push(AnthropicMessage {
                    role: "user",
                    content: vec![AnthropicRequestContent::Text {
                        text: message.content.clone(),
                        cache_control: None,
                    }],
                });
            }
            MessageRole::Assistant => {
                let parsed = decode_assistant_message_content(&message.content);
                let mut content = Vec::new();

                if !parsed.text.is_empty() || parsed.tool_calls.is_empty() {
                    content.push(AnthropicRequestContent::Text {
                        text: parsed.text,
                        cache_control: None,
                    });
                }

                for tool_call in parsed.tool_calls {
                    let tool_id = tool_call.id.unwrap_or_else(|| tool_call.call_id.clone());
                    provider_tool_ids.insert(tool_call.call_id, tool_id.clone());

                    content.push(AnthropicRequestContent::ToolUse {
                        id: tool_id,
                        name: tool_call.name,
                        input: tool_call.input,
                    });
                }

                messages.push(AnthropicMessage {
                    role: "assistant",
                    content,
                });
            }
            MessageRole::Tool => {
                let tool_result = normalize_tool_result_message(&message.content);
                let provider_tool_id = provider_tool_ids
                    .get(&tool_result.call_id)
                    .cloned()
                    .unwrap_or_else(|| tool_result.call_id.clone());

                messages.push(AnthropicMessage {
                    role: "user",
                    content: vec![AnthropicRequestContent::ToolResult {
                        tool_use_id: provider_tool_id,
                        content: tool_result.output,
                        is_error: tool_result.is_error,
                        cache_control: None,
                    }],
                });
            }
        }
    }

    apply_cache_control(&mut system, &mut messages);

    let tools: Vec<AnthropicToolDefinition> =
        normalized.tools.iter().map(to_anthropic_tool).collect();

    let tool_choice = if tools.is_empty() {
        None
    } else {
        Some(AnthropicToolChoice::Auto)
    };

    Ok(AnthropicRequest {
        model: normalized.model.clone(),
        messages,
        max_tokens: anthropic_default_max_tokens(&normalized.model),
        stream: true,
        system,
        tools,
        tool_choice,
    })
}

fn to_anthropic_tool(tool: &ToolDefinition) -> AnthropicToolDefinition {
    AnthropicToolDefinition {
        name: tool.name.clone(),
        description: tool.description.clone(),
        input_schema: tool.parameters.clone(),
    }
}

fn apply_cache_control(system: &mut [AnthropicSystemContent], messages: &mut [AnthropicMessage]) {
    if let Some(AnthropicSystemContent::Text { cache_control, .. }) = system.last_mut() {
        *cache_control = Some(AnthropicCacheControl::ephemeral());
    }

    for message in messages.iter_mut() {
        for content in message.content.iter_mut() {
            set_content_cache_control(content, None);
        }
    }

    if let Some(last_message) = messages.last_mut()
        && let Some(last_content) = last_message.content.last_mut()
    {
        set_content_cache_control(last_content, Some(AnthropicCacheControl::ephemeral()));
    }
}

fn set_content_cache_control(
    content: &mut AnthropicRequestContent,
    cache_control: Option<AnthropicCacheControl>,
) {
    match content {
        AnthropicRequestContent::Text {
            cache_control: current,
            ..
        }
        | AnthropicRequestContent::ToolResult {
            cache_control: current,
            ..
        } => {
            *current = cache_control;
        }
        AnthropicRequestContent::ToolUse { .. } => {}
    }
}

fn finalize_pending_anthropic_tool_call(
    index: usize,
    pending: PendingAnthropicToolCall,
) -> Result<ToolCall, ProviderError> {
    let input = if pending.input_json.trim().is_empty() {
        match pending.start_input {
            Value::Null => Value::Object(serde_json::Map::new()),
            other => other,
        }
    } else {
        serde_json::from_str::<Value>(&pending.input_json).map_err(|error| {
            ProviderError::Transport(format!(
                "failed to parse anthropic tool input for block {index}: {error}"
            ))
        })?
    };

    Ok(ToolCall {
        id: Some(pending.id.clone()),
        call_id: pending.id,
        name: pending.name,
        input,
    })
}

fn anthropic_default_max_tokens(model: &str) -> u64 {
    if model.starts_with("claude-opus-4") {
        32_000
    } else if model.starts_with("claude-sonnet-4") || model.starts_with("claude-3-7-sonnet") {
        64_000
    } else if model.starts_with("claude-3-5-sonnet") || model.starts_with("claude-3-5-haiku") {
        8_192
    } else if model.starts_with("claude-3-opus")
        || model.starts_with("claude-3-sonnet")
        || model.starts_with("claude-3-haiku")
    {
        4_096
    } else {
        2_048
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Serialize)]
struct AnthropicRequest {
    model: String,
    messages: Vec<AnthropicMessage>,
    max_tokens: u64,
    stream: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    system: Vec<AnthropicSystemContent>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<AnthropicToolDefinition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<AnthropicToolChoice>,
}

#[derive(Debug, Serialize)]
struct AnthropicMessage {
    role: &'static str,
    content: Vec<AnthropicRequestContent>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicSystemContent {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<AnthropicCacheControl>,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicRequestContent {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<AnthropicCacheControl>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "is_false")]
        is_error: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<AnthropicCacheControl>,
    },
}

#[derive(Debug, Clone, Serialize)]
struct AnthropicCacheControl {
    #[serde(rename = "type")]
    kind: &'static str,
}

impl AnthropicCacheControl {
    fn ephemeral() -> Self {
        Self { kind: "ephemeral" }
    }
}

#[derive(Debug, Serialize)]
struct AnthropicToolDefinition {
    name: String,
    description: String,
    #[serde(rename = "input_schema")]
    input_schema: Value,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicToolChoice {
    Auto,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicStreamEvent {
    MessageStart {},
    MessageDelta {
        delta: AnthropicMessageDelta,
    },
    MessageStop,
    ContentBlockStart {
        index: usize,
        content_block: AnthropicContentBlockStart,
    },
    ContentBlockDelta {
        index: usize,
        delta: AnthropicContentBlockDelta,
    },
    ContentBlockStop {
        index: usize,
    },
    Ping,
    Error {
        error: AnthropicStreamErrorPayload,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Default, Deserialize)]
struct AnthropicMessageDelta {
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicContentBlockStart {
    Text {
        #[serde(default)]
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: Value,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicContentBlockDelta {
    TextDelta {
        #[serde(default)]
        text: String,
    },
    InputJsonDelta {
        #[serde(default)]
        partial_json: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Default, Deserialize)]
struct AnthropicStreamErrorPayload {
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    message: String,
}

#[derive(Debug)]
struct PendingAnthropicToolCall {
    id: String,
    name: String,
    start_input: Value,
    input_json: String,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::{message::encode_assistant_message_content, providers::ModelKind, tool::ToolCall};

    use super::*;

    #[test]
    fn build_anthropic_request_maps_tool_result_to_provider_tool_id() {
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
                    "output": "ok"
                })
                .to_string(),
            ),
            Message::new(MessageRole::User, "next"),
        ];

        let request = ProviderRequest::new(ModelKind::ClaudeSonnet46, &messages);
        let mapped = build_anthropic_request(request).expect("request should map");

        assert!(matches!(
            &mapped.messages[2].content[0],
            AnthropicRequestContent::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } if tool_use_id == "fc_123" && !is_error && content == &json!({"is_error":false,"output":"ok"}).to_string()
        ));
    }

    #[test]
    fn apply_cache_control_marks_system_and_last_content() {
        let mut system = vec![AnthropicSystemContent::Text {
            text: "system".to_string(),
            cache_control: None,
        }];
        let mut messages = vec![
            AnthropicMessage {
                role: "user",
                content: vec![AnthropicRequestContent::Text {
                    text: "hello".to_string(),
                    cache_control: None,
                }],
            },
            AnthropicMessage {
                role: "assistant",
                content: vec![AnthropicRequestContent::Text {
                    text: "world".to_string(),
                    cache_control: None,
                }],
            },
        ];

        apply_cache_control(&mut system, &mut messages);

        assert!(matches!(
            &system[0],
            AnthropicSystemContent::Text {
                cache_control: Some(_),
                ..
            }
        ));
        assert!(matches!(
            &messages[1].content[0],
            AnthropicRequestContent::Text {
                cache_control: Some(_),
                ..
            }
        ));
        assert!(matches!(
            &messages[0].content[0],
            AnthropicRequestContent::Text {
                cache_control: None,
                ..
            }
        ));
    }

    #[test]
    fn finalize_pending_tool_call_parses_json_input() {
        let pending = PendingAnthropicToolCall {
            id: "toolu_1".to_string(),
            name: "read".to_string(),
            start_input: Value::Null,
            input_json: "{\"path\":\"README.md\"}".to_string(),
        };

        let call = finalize_pending_anthropic_tool_call(0, pending)
            .expect("pending tool call should finalize");

        assert_eq!(call.id.as_deref(), Some("toolu_1"));
        assert_eq!(call.call_id, "toolu_1");
        assert_eq!(call.name, "read");
        assert_eq!(call.input, json!({ "path": "README.md" }));
    }
}
