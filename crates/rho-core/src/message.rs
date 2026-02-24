use serde::{Deserialize, Serialize};

use crate::tool::ToolCall;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Message {
    pub role: MessageRole,
    pub content: String,
}

impl Message {
    pub fn new(role: MessageRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
        }
    }
}

const ASSISTANT_TOOL_CALLS_PAYLOAD_KIND: &str = "rho_assistant_tool_calls_v1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ParsedAssistantMessageContent {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
}

pub fn encode_assistant_message_content(
    text: impl Into<String>,
    tool_calls: &[ToolCall],
) -> String {
    let plain_text = text.into();
    if tool_calls.is_empty() {
        return plain_text;
    }

    let payload = AssistantToolCallsPayloadRef {
        kind: ASSISTANT_TOOL_CALLS_PAYLOAD_KIND,
        text: plain_text.as_str(),
        tool_calls,
    };

    serde_json::to_string(&payload).unwrap_or(plain_text)
}

pub fn decode_assistant_message_content(content: &str) -> ParsedAssistantMessageContent {
    if !looks_like_json_object(content) {
        return parsed_plain_text_content(content);
    }

    match serde_json::from_str::<AssistantToolCallsPayload>(content) {
        Ok(payload) if payload.kind == ASSISTANT_TOOL_CALLS_PAYLOAD_KIND => {
            ParsedAssistantMessageContent {
                text: payload.text,
                tool_calls: payload.tool_calls,
            }
        }
        _ => parsed_plain_text_content(content),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AssistantToolCallsPayload {
    kind: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Serialize)]
struct AssistantToolCallsPayloadRef<'a> {
    kind: &'static str,
    text: &'a str,
    tool_calls: &'a [ToolCall],
}

fn parsed_plain_text_content(content: &str) -> ParsedAssistantMessageContent {
    ParsedAssistantMessageContent {
        text: content.to_string(),
        tool_calls: Vec::new(),
    }
}

fn looks_like_json_object(content: &str) -> bool {
    content.trim_start().starts_with('{')
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn assistant_message_content_round_trips_tool_calls() {
        let encoded = encode_assistant_message_content(
            "thinking",
            &[ToolCall {
                id: Some("fc-1".to_string()),
                call_id: "call-1".to_string(),
                name: "read".to_string(),
                input: json!({ "path": "README.md" }),
            }],
        );

        let parsed = decode_assistant_message_content(&encoded);
        assert_eq!(parsed.text, "thinking");
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].id.as_deref(), Some("fc-1"));
        assert_eq!(parsed.tool_calls[0].call_id, "call-1");
        assert_eq!(parsed.tool_calls[0].name, "read");
        assert_eq!(parsed.tool_calls[0].input, json!({ "path": "README.md" }));
    }

    #[test]
    fn assistant_message_content_leaves_plain_text_unchanged() {
        let parsed = decode_assistant_message_content("just text");
        assert_eq!(parsed.text, "just text");
        assert!(parsed.tool_calls.is_empty());
    }

    #[test]
    fn assistant_message_content_decodes_legacy_payload_without_id() {
        let legacy = json!({
            "kind": "rho_assistant_tool_calls_v1",
            "text": "",
            "tool_calls": [{
                "call_id": "call-legacy",
                "name": "read",
                "input": { "path": "README.md" }
            }]
        })
        .to_string();

        let parsed = decode_assistant_message_content(&legacy);
        assert_eq!(parsed.tool_calls.len(), 1);
        assert!(parsed.tool_calls[0].id.is_none());
        assert_eq!(parsed.tool_calls[0].call_id, "call-legacy");
    }
}
