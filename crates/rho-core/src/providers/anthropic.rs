use std::collections::HashMap;

use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    message::{Message, MessageRole},
    providers::{
        Provider, ProviderError, ProviderKind, ProviderRequest, ProviderStream, validate_api_key,
    },
    stream::ProviderEvent,
    tool::ToolCall,
};

const ANTHROPIC_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
const ANTHROPIC_API_BASE: &str = "https://api.anthropic.com/v1";
const ANTHROPIC_API_VERSION: &str = "2023-06-01";
const ANTHROPIC_DEFAULT_MAX_TOKENS: u32 = 1024;

#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    client: reqwest::Client,
    base_url: String,
}

impl Default for AnthropicProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl AnthropicProvider {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: ANTHROPIC_API_BASE.to_string(),
        }
    }
}

impl Provider for AnthropicProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Anthropic
    }

    fn stream(&self, request: ProviderRequest) -> ProviderStream {
        let client = self.client.clone();
        let base_url = self.base_url.clone();

        Box::pin(async_stream::try_stream! {
            let api_key = anthropic_api_key()?;

            let response = client
                .post(format!("{base_url}/messages"))
                .header("x-api-key", api_key)
                .header("anthropic-version", ANTHROPIC_API_VERSION)
                .json(&AnthropicRequest::from(request))
                .send()
                .await
                .map_err(|error| ProviderError::Transport(format!("anthropic request failed: {error}")))?;

            let status = response.status();
            if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                Err(ProviderError::InvalidApiKey(ANTHROPIC_API_KEY_ENV))?;
            }

            if !status.is_success() {
                Err(ProviderError::Transport(format!(
                    "anthropic request failed with status {status}"
                )))?;
            }

            let mut aggregate_text = String::new();
            let mut tool_states: HashMap<usize, ToolCallState> = HashMap::new();
            let mut stream = response.bytes_stream().eventsource();

            while let Some(next_event) = stream.next().await {
                let event = next_event
                    .map_err(|error| ProviderError::Transport(format!("anthropic stream failed: {error}")))?;

                let data = event.data.trim();
                if data.is_empty() {
                    continue;
                }

                let chunk: AnthropicChunk = serde_json::from_str(data).map_err(|error| {
                    ProviderError::Transport(format!("invalid anthropic stream payload: {error}"))
                })?;

                match chunk {
                    AnthropicChunk::ContentBlockStart {
                        index,
                        content_block,
                    } => match content_block {
                        AnthropicContentBlock::Text { text } => {
                            if !text.is_empty() {
                                aggregate_text.push_str(&text);
                                yield ProviderEvent::AssistantDelta { delta: text };
                            }
                        }
                        AnthropicContentBlock::ToolUse { id, name, input } => {
                            tool_states.insert(
                                index,
                                ToolCallState {
                                    call_id: id,
                                    name,
                                    input,
                                    partial_json: String::new(),
                                },
                            );
                        }
                        AnthropicContentBlock::Unknown => {}
                    },
                    AnthropicChunk::ContentBlockDelta { index, delta } => match delta {
                        AnthropicDelta::TextDelta { text } => {
                            if !text.is_empty() {
                                aggregate_text.push_str(&text);
                                yield ProviderEvent::AssistantDelta { delta: text };
                            }
                        }
                        AnthropicDelta::InputJsonDelta { partial_json } => {
                            if let Some(state) = tool_states.get_mut(&index) {
                                state.partial_json.push_str(&partial_json);
                            }
                        }
                        AnthropicDelta::Unknown => {}
                    },
                    AnthropicChunk::ContentBlockStop { index } => {
                        if let Some(state) = tool_states.remove(&index) {
                            let call = state.into_tool_call()?;
                            yield ProviderEvent::ToolCall { call };
                        }
                    }
                    AnthropicChunk::Error { error } => {
                        let message = if error.message.trim().is_empty() {
                            "anthropic stream returned an error".to_string()
                        } else {
                            format!("anthropic stream error: {}", error.message)
                        };
                        Err(ProviderError::Transport(message))?;
                    }
                    AnthropicChunk::MessageStop => {
                        break;
                    }
                    AnthropicChunk::MessageStart
                    | AnthropicChunk::MessageDelta
                    | AnthropicChunk::Ping
                    | AnthropicChunk::Unknown => {}
                }
            }

            let message = Message::new(MessageRole::Assistant, aggregate_text);
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

#[derive(Debug, Serialize)]
struct AnthropicRequest {
    model: String,
    messages: Vec<AnthropicInputMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    max_tokens: u32,
    stream: bool,
}

impl From<ProviderRequest> for AnthropicRequest {
    fn from(request: ProviderRequest) -> Self {
        let mut system_sections = Vec::new();
        let mut messages = Vec::new();

        for message in request.messages {
            match message.role {
                MessageRole::System => system_sections.push(message.content),
                MessageRole::User | MessageRole::Tool => {
                    messages.push(AnthropicInputMessage::new("user", message.content))
                }
                MessageRole::Assistant => {
                    messages.push(AnthropicInputMessage::new("assistant", message.content))
                }
            }
        }

        let system = if system_sections.is_empty() {
            None
        } else {
            Some(system_sections.join("\n\n"))
        };

        Self {
            model: request.model,
            messages,
            system,
            max_tokens: ANTHROPIC_DEFAULT_MAX_TOKENS,
            stream: true,
        }
    }
}

#[derive(Debug, Serialize)]
struct AnthropicInputMessage {
    role: &'static str,
    content: Vec<AnthropicInputContent>,
}

impl AnthropicInputMessage {
    fn new(role: &'static str, text: String) -> Self {
        Self {
            role,
            content: vec![AnthropicInputContent::Text { text }],
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicInputContent {
    Text { text: String },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicChunk {
    MessageStart,
    MessageDelta,
    ContentBlockStart {
        index: usize,
        content_block: AnthropicContentBlock,
    },
    ContentBlockDelta {
        index: usize,
        delta: AnthropicDelta,
    },
    ContentBlockStop {
        index: usize,
    },
    MessageStop,
    Ping,
    Error {
        error: AnthropicError,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicContentBlock {
    Text {
        #[serde(default)]
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        #[serde(default = "empty_json_object")]
        input: Value,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicDelta {
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

#[derive(Debug, Deserialize)]
struct AnthropicError {
    #[serde(default)]
    message: String,
}

#[derive(Debug)]
struct ToolCallState {
    call_id: String,
    name: String,
    input: Value,
    partial_json: String,
}

impl ToolCallState {
    fn into_tool_call(self) -> Result<ToolCall, ProviderError> {
        let input = if self.partial_json.trim().is_empty() {
            self.input
        } else {
            serde_json::from_str(&self.partial_json).map_err(|error| {
                ProviderError::Transport(format!("invalid anthropic tool input payload: {error}"))
            })?
        };

        Ok(ToolCall {
            call_id: self.call_id,
            name: self.name,
            input,
        })
    }
}

fn empty_json_object() -> Value {
    json!({})
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::providers::ProviderRequest;

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

    #[test]
    fn provider_request_maps_to_anthropic_body() {
        let request = ProviderRequest::new(
            "claude-3-5-haiku-latest",
            vec![
                Message::new(MessageRole::System, "be concise"),
                Message::new(MessageRole::User, "hi"),
                Message::new(MessageRole::Assistant, "hello"),
                Message::new(MessageRole::Tool, "{\"ok\":true}"),
            ],
        );

        let body =
            serde_json::to_value(AnthropicRequest::from(request)).expect("serialize request");

        assert_eq!(body["model"], json!("claude-3-5-haiku-latest"));
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["max_tokens"], json!(ANTHROPIC_DEFAULT_MAX_TOKENS));
        assert_eq!(body["system"], json!("be concise"));
        assert_eq!(body["messages"][0]["role"], json!("user"));
        assert_eq!(body["messages"][0]["content"][0]["type"], json!("text"));
        assert_eq!(body["messages"][0]["content"][0]["text"], json!("hi"));
        assert_eq!(body["messages"][1]["role"], json!("assistant"));
        assert_eq!(body["messages"][1]["content"][0]["text"], json!("hello"));
        assert_eq!(body["messages"][2]["role"], json!("user"));
        assert_eq!(
            body["messages"][2]["content"][0]["text"],
            json!("{\"ok\":true}")
        );
    }

    #[test]
    fn chunk_deserializes_text_delta() {
        let json = r#"{
            "type":"content_block_delta",
            "index":0,
            "delta":{"type":"text_delta","text":"hello"}
        }"#;

        let chunk: AnthropicChunk = serde_json::from_str(json).expect("parse chunk");
        let AnthropicChunk::ContentBlockDelta { index, delta } = chunk else {
            panic!("expected content block delta");
        };
        assert_eq!(index, 0);
        match delta {
            AnthropicDelta::TextDelta { text } => assert_eq!(text, "hello"),
            _ => panic!("expected text delta"),
        }
    }

    #[test]
    fn chunk_deserializes_tool_use_start() {
        let json = r#"{
            "type":"content_block_start",
            "index":1,
            "content_block":{
                "type":"tool_use",
                "id":"toolu_123",
                "name":"read",
                "input":{"path":"/tmp/demo.txt"}
            }
        }"#;

        let chunk: AnthropicChunk = serde_json::from_str(json).expect("parse chunk");
        let AnthropicChunk::ContentBlockStart {
            index,
            content_block,
        } = chunk
        else {
            panic!("expected content block start");
        };
        assert_eq!(index, 1);
        match content_block {
            AnthropicContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "toolu_123");
                assert_eq!(name, "read");
                assert_eq!(input, json!({"path":"/tmp/demo.txt"}));
            }
            _ => panic!("expected tool use block"),
        }
    }

    #[test]
    fn tool_call_state_prefers_partial_json_when_present() {
        let state = ToolCallState {
            call_id: "call_1".to_string(),
            name: "read".to_string(),
            input: json!({"path":"/tmp/a.txt"}),
            partial_json: "{\"path\":\"/tmp/b.txt\"}".to_string(),
        };

        let tool_call = state.into_tool_call().expect("parse tool call");
        assert_eq!(tool_call.call_id, "call_1");
        assert_eq!(tool_call.name, "read");
        assert_eq!(tool_call.input, json!({"path":"/tmp/b.txt"}));
    }
}
