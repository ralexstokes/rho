use std::collections::{BTreeMap, HashMap};

use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{
    message::{Message, MessageRole, decode_assistant_message_content},
    providers::{
        CancellationToken, Provider, ProviderError, ProviderKind, ProviderRequest, ProviderStream,
        ensure_success_status, looks_like_auth_error, map_reqwest_error,
        normalize_provider_request, normalize_tool_result_message, shared_http_client,
        validate_api_key,
    },
    stream::ProviderEvent,
    tool::{ToolCall, ToolDefinition},
};

const OPENAI_API_KEY_ENV: &str = "OPENAI_API_KEY";
const OPENAI_BASE_URL_ENV: &str = "OPENAI_BASE_URL";
const OPENAI_DEFAULT_BASE_URL: &str = "https://api.openai.com";

#[derive(Debug, Clone, Default)]
pub struct OpenAiProvider;

impl OpenAiProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Provider for OpenAiProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenAi
    }

    fn stream(&self, request: ProviderRequest<'_>, cancel: CancellationToken) -> ProviderStream {
        let request_body = build_openai_request(request);
        let client = shared_http_client();
        let api_key = openai_api_key();
        let url = openai_chat_completions_url();

        Box::pin(async_stream::try_stream! {
            let request_body = request_body?;
            let client = client?;
            let api_key = api_key?;

            let response = client
                .post(&url)
                .bearer_auth(api_key)
                .header(reqwest::header::ACCEPT, "text/event-stream")
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .json(&request_body)
                .send()
                .await
                .map_err(map_reqwest_error)?;

            let response = ensure_success_status(OPENAI_API_KEY_ENV, response).await?;
            let mut event_stream = response.bytes_stream().eventsource();
            let mut pending_tool_calls = BTreeMap::<usize, PendingOpenAiToolCall>::new();
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
                if data == "[DONE]" {
                    break;
                }

                if let Some(error) = parse_openai_error_payload(data) {
                    Err(error)?;
                }

                let chunk = match serde_json::from_str::<OpenAiStreamChunk>(data) {
                    Ok(chunk) => chunk,
                    Err(_) => continue,
                };

                let Some(choice) = chunk.choices.first() else {
                    continue;
                };

                if let Some(content) = choice.delta.content.as_deref().filter(|content| !content.is_empty()) {
                    assistant_text.push_str(content);
                    yield ProviderEvent::AssistantDelta {
                        delta: content.to_string(),
                    };
                }

                for tool_delta in &choice.delta.tool_calls {
                    apply_tool_call_delta(&mut pending_tool_calls, tool_delta);
                }

                if matches!(choice.finish_reason.as_deref(), Some("tool_calls")) {
                    for call in flush_pending_tool_calls(&mut pending_tool_calls) {
                        yield ProviderEvent::ToolCall { call };
                    }
                }
            }

            for call in flush_pending_tool_calls(&mut pending_tool_calls) {
                yield ProviderEvent::ToolCall { call };
            }

            let message = Message::new(MessageRole::Assistant, assistant_text);
            yield ProviderEvent::Message { message };
            yield ProviderEvent::Finished;
        })
    }
}

fn openai_api_key() -> Result<String, ProviderError> {
    validate_api_key(OPENAI_API_KEY_ENV, std::env::var(OPENAI_API_KEY_ENV).ok())
}

fn openai_chat_completions_url() -> String {
    openai_chat_completions_url_from_base(std::env::var(OPENAI_BASE_URL_ENV).ok().as_deref())
}

fn openai_chat_completions_url_from_base(base: Option<&str>) -> String {
    let base = base
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| OPENAI_DEFAULT_BASE_URL.to_string());

    let base = base.trim_end_matches('/');
    if base.ends_with("/chat/completions") {
        base.to_string()
    } else if base.ends_with("/v1") {
        format!("{base}/chat/completions")
    } else {
        format!("{base}/v1/chat/completions")
    }
}

fn build_openai_request(request: ProviderRequest<'_>) -> Result<OpenAiChatRequest, ProviderError> {
    let normalized = normalize_provider_request(request)?;

    let mut messages = Vec::new();
    if let Some(system) = normalized.system {
        messages.push(OpenAiMessage::System { content: system });
    }

    let mut provider_tool_ids = HashMap::<String, String>::new();

    for message in normalized.messages {
        match message.role {
            MessageRole::System => {}
            MessageRole::User => {
                messages.push(OpenAiMessage::User {
                    content: message.content.clone(),
                });
            }
            MessageRole::Assistant => {
                let parsed = decode_assistant_message_content(&message.content);
                let mut tool_calls = Vec::with_capacity(parsed.tool_calls.len());

                for tool_call in parsed.tool_calls {
                    let tool_id = tool_call.id.unwrap_or_else(|| tool_call.call_id.clone());
                    provider_tool_ids.insert(tool_call.call_id, tool_id.clone());

                    tool_calls.push(OpenAiToolCall {
                        id: tool_id,
                        kind: "function",
                        function: OpenAiToolFunctionCall {
                            name: tool_call.name,
                            arguments: tool_call.input.to_string(),
                        },
                    });
                }

                messages.push(OpenAiMessage::Assistant {
                    content: parsed.text,
                    tool_calls,
                });
            }
            MessageRole::Tool => {
                let tool_result = normalize_tool_result_message(&message.content);
                let provider_tool_id = provider_tool_ids
                    .get(&tool_result.call_id)
                    .cloned()
                    .unwrap_or_else(|| tool_result.call_id.clone());

                messages.push(OpenAiMessage::Tool {
                    tool_call_id: provider_tool_id,
                    content: tool_result.output,
                });
            }
        }
    }

    let tools = normalized
        .tools
        .iter()
        .map(to_openai_tool_definition)
        .collect();

    Ok(OpenAiChatRequest {
        model: normalized.model,
        messages,
        tools,
        stream: true,
        stream_options: OpenAiStreamOptions {
            include_usage: true,
        },
    })
}

fn to_openai_tool_definition(tool: &ToolDefinition) -> OpenAiToolDefinition {
    OpenAiToolDefinition {
        kind: "function",
        function: OpenAiToolFunctionDefinition {
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters: tool.parameters.clone(),
        },
    }
}

fn parse_openai_error_payload(data: &str) -> Option<ProviderError> {
    let payload = serde_json::from_str::<OpenAiErrorEnvelope>(data).ok()?;

    let message = match (
        payload.error.code.trim().is_empty(),
        payload.error.message.trim().is_empty(),
    ) {
        (true, true) => "OpenAI stream returned an error payload without details".to_string(),
        (false, true) => payload.error.code,
        (true, false) => payload.error.message,
        (false, false) => format!("{}: {}", payload.error.code, payload.error.message),
    };

    if looks_like_auth_error(&message) {
        Some(ProviderError::InvalidApiKey(OPENAI_API_KEY_ENV))
    } else {
        Some(ProviderError::Transport(message))
    }
}

fn apply_tool_call_delta(
    pending_tool_calls: &mut BTreeMap<usize, PendingOpenAiToolCall>,
    delta: &OpenAiStreamToolCallDelta,
) {
    let pending = pending_tool_calls.entry(delta.index).or_default();

    if let Some(id) = delta.id.as_deref().filter(|id| !id.is_empty()) {
        pending.id = Some(id.to_string());
    }

    if let Some(name) = delta
        .function
        .name
        .as_deref()
        .filter(|name| !name.is_empty())
    {
        pending.name = Some(name.to_string());
    }

    if let Some(arguments_chunk) = delta
        .function
        .arguments
        .as_deref()
        .filter(|arguments| !arguments.is_empty())
    {
        pending.arguments.push_str(arguments_chunk);
    }
}

fn flush_pending_tool_calls(
    pending_tool_calls: &mut BTreeMap<usize, PendingOpenAiToolCall>,
) -> Vec<ToolCall> {
    std::mem::take(pending_tool_calls)
        .into_iter()
        .map(|(index, pending)| {
            let id = pending.id.unwrap_or_else(|| format!("call-{index}"));
            let name = pending.name.unwrap_or_else(|| "unknown".to_string());

            ToolCall {
                id: Some(id.clone()),
                call_id: id,
                name,
                input: parse_openai_tool_arguments(&pending.arguments),
            }
        })
        .collect()
}

fn parse_openai_tool_arguments(arguments: &str) -> Value {
    let trimmed = arguments.trim();
    if trimmed.is_empty() {
        return Value::Object(Map::new());
    }

    serde_json::from_str(trimmed).unwrap_or_else(|_| Value::String(arguments.to_string()))
}

#[derive(Debug, Serialize)]
struct OpenAiChatRequest {
    model: String,
    messages: Vec<OpenAiMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<OpenAiToolDefinition>,
    stream: bool,
    stream_options: OpenAiStreamOptions,
}

#[derive(Debug, Serialize)]
struct OpenAiStreamOptions {
    include_usage: bool,
}

#[derive(Debug, Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
enum OpenAiMessage {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        content: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<OpenAiToolCall>,
    },
    #[serde(rename = "tool")]
    Tool {
        tool_call_id: String,
        content: String,
    },
}

#[derive(Debug, Serialize)]
struct OpenAiToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    function: OpenAiToolFunctionCall,
}

#[derive(Debug, Serialize)]
struct OpenAiToolFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Debug, Serialize)]
struct OpenAiToolDefinition {
    #[serde(rename = "type")]
    kind: &'static str,
    function: OpenAiToolFunctionDefinition,
}

#[derive(Debug, Serialize)]
struct OpenAiToolFunctionDefinition {
    name: String,
    description: String,
    parameters: Value,
}

#[derive(Debug, Deserialize)]
struct OpenAiErrorEnvelope {
    error: OpenAiErrorPayload,
}

#[derive(Debug, Default, Deserialize)]
struct OpenAiErrorPayload {
    #[serde(default)]
    message: String,
    #[serde(default)]
    code: String,
}

#[derive(Debug, Deserialize)]
struct OpenAiStreamChunk {
    #[serde(default)]
    choices: Vec<OpenAiStreamChoice>,
}

#[derive(Debug, Default, Deserialize)]
struct OpenAiStreamChoice {
    #[serde(default)]
    delta: OpenAiStreamDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct OpenAiStreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<OpenAiStreamToolCallDelta>,
}

#[derive(Debug, Deserialize)]
struct OpenAiStreamToolCallDelta {
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: OpenAiStreamFunctionDelta,
}

#[derive(Debug, Default, Deserialize)]
struct OpenAiStreamFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Default)]
struct PendingOpenAiToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::{message::encode_assistant_message_content, providers::ModelKind, tool::ToolCall};

    use super::*;

    #[test]
    fn build_openai_request_maps_tool_result_to_provider_tool_id() {
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
        let request = ProviderRequest::new(ModelKind::Gpt52, &messages);

        let mapped = build_openai_request(request).expect("request should map");

        assert!(matches!(
            &mapped.messages[2],
            OpenAiMessage::Tool {
                tool_call_id,
                content
            } if tool_call_id == "fc_123" && content == &json!({"is_error":false,"output":"ok"}).to_string()
        ));
    }

    #[test]
    fn openai_chat_completions_url_uses_defaults_and_custom_bases() {
        assert_eq!(
            openai_chat_completions_url_from_base(None),
            "https://api.openai.com/v1/chat/completions"
        );

        assert_eq!(
            openai_chat_completions_url_from_base(Some("https://example.com/custom/v1")),
            "https://example.com/custom/v1/chat/completions"
        );

        assert_eq!(
            openai_chat_completions_url_from_base(Some(
                "https://example.com/custom/v1/chat/completions"
            )),
            "https://example.com/custom/v1/chat/completions"
        );
    }

    #[test]
    fn parse_openai_tool_arguments_handles_empty_and_partial_json() {
        assert_eq!(parse_openai_tool_arguments(""), Value::Object(Map::new()));
        assert_eq!(
            parse_openai_tool_arguments("{\"path\":\"README.md\"}"),
            json!({ "path": "README.md" })
        );
        assert_eq!(
            parse_openai_tool_arguments("{\"path\":\"README"),
            Value::String("{\"path\":\"README".to_string())
        );
    }
}
