use std::collections::{BTreeMap, HashMap};

use rho_ai::{
    AssistantMessage, ContentBlock, DeltaKind, ErrorKind, ModelId, OpaqueBlob, ProviderError,
    ProviderId, StopReason, StreamEvent, ToolArgumentError, ToolCallId, ToolDefinition, Usage,
    validate_tool_arguments,
};
use serde::Deserialize;
use serde_json::Value;

use crate::decoder::DecodedEvent;

const PROVIDER: &str = "anthropic";

pub(crate) struct ResponseAssembler {
    requested_model: ModelId,
    actual_model: Option<ModelId>,
    tools: HashMap<String, ToolDefinition>,
    open: BTreeMap<usize, PartialBlock>,
    complete: BTreeMap<usize, ContentBlock>,
    usage: Usage,
    stop: Option<StopReason>,
    started: bool,
    stopped: bool,
}

impl ResponseAssembler {
    pub(crate) fn new(requested_model: ModelId, tools: Vec<ToolDefinition>) -> Self {
        Self {
            requested_model,
            actual_model: None,
            tools: tools
                .into_iter()
                .map(|tool| (tool.name.clone(), tool))
                .collect(),
            open: BTreeMap::new(),
            complete: BTreeMap::new(),
            usage: Usage::default(),
            stop: None,
            started: false,
            stopped: false,
        }
    }

    pub(crate) fn accept(
        &mut self,
        event: DecodedEvent,
    ) -> Result<Vec<StreamEvent>, ProviderError> {
        if self.stopped {
            return Err(invalid("received an event after message_stop"));
        }
        match event.event.as_str() {
            "message_start" => self.message_start(event.data),
            "content_block_start" => self.block_start(event.data),
            "content_block_delta" => self.block_delta(event.data),
            "content_block_stop" => self.block_stop(event.data),
            "message_delta" => self.message_delta(event.data),
            "message_stop" => self.message_stop(),
            "error" => Err(api_error(event.data)),
            "ping" => Ok(Vec::new()),
            _ => Ok(Vec::new()),
        }
    }

    fn message_start(&mut self, data: Value) -> Result<Vec<StreamEvent>, ProviderError> {
        if self.started {
            return Err(invalid("received duplicate message_start"));
        }
        let event: MessageStart = parse(data, "message_start")?;
        self.started = true;
        self.actual_model = Some(ModelId::from(event.message.model));
        self.usage.input_tokens = event.message.usage.input_tokens;
        self.usage.output_tokens = event.message.usage.output_tokens;
        self.usage.cache_read_tokens = event.message.usage.cache_read_input_tokens;
        self.usage.cache_write_tokens = event.message.usage.cache_creation_input_tokens;
        Ok(vec![StreamEvent::Start])
    }

    fn block_start(&mut self, data: Value) -> Result<Vec<StreamEvent>, ProviderError> {
        self.require_started("content_block_start")?;
        let event: ContentBlockStart = parse(data, "content_block_start")?;
        if self.open.contains_key(&event.index) || self.complete.contains_key(&event.index) {
            return Err(invalid(format!(
                "received duplicate content block index {}",
                event.index
            )));
        }
        let block = match event.content_block {
            StartBlock::Text { text } => PartialBlock::Text(text),
            StartBlock::Thinking { thinking } => PartialBlock::Thinking {
                text: thinking,
                signature: None,
            },
            StartBlock::RedactedThinking { data } => PartialBlock::RedactedThinking(data),
            StartBlock::ToolUse { id, name, input } => PartialBlock::ToolUse {
                id,
                name,
                initial_input: input,
                partial_json: String::new(),
                saw_delta: false,
            },
            StartBlock::Unknown => PartialBlock::Unknown,
        };
        self.open.insert(event.index, block);
        Ok(Vec::new())
    }

    fn block_delta(&mut self, data: Value) -> Result<Vec<StreamEvent>, ProviderError> {
        self.require_started("content_block_delta")?;
        let event: ContentBlockDelta = parse(data, "content_block_delta")?;
        let block = self.open.get_mut(&event.index).ok_or_else(|| {
            invalid(format!(
                "received delta for unopened content block {}",
                event.index
            ))
        })?;
        let stream_event = match (&mut *block, event.delta) {
            (PartialBlock::Text(text), BlockDelta::Text { text: delta }) => {
                text.push_str(&delta);
                Some(StreamEvent::Delta {
                    index: event.index,
                    kind: DeltaKind::Text,
                    delta,
                })
            }
            (PartialBlock::Thinking { text, .. }, BlockDelta::Thinking { thinking: delta }) => {
                text.push_str(&delta);
                Some(StreamEvent::Delta {
                    index: event.index,
                    kind: DeltaKind::Thinking,
                    delta,
                })
            }
            (
                PartialBlock::Thinking { signature, .. },
                BlockDelta::Signature { signature: delta },
            ) => {
                signature.get_or_insert_with(String::new).push_str(&delta);
                None
            }
            (
                PartialBlock::ToolUse {
                    partial_json,
                    saw_delta,
                    ..
                },
                BlockDelta::InputJson {
                    partial_json: delta,
                },
            ) => {
                *saw_delta = true;
                partial_json.push_str(&delta);
                Some(StreamEvent::Delta {
                    index: event.index,
                    kind: DeltaKind::ToolArguments,
                    delta,
                })
            }
            (PartialBlock::Unknown, _) | (_, BlockDelta::Unknown) => None,
            _ => {
                return Err(invalid(format!(
                    "delta type did not match content block {}",
                    event.index
                )));
            }
        };
        Ok(stream_event.into_iter().collect())
    }

    fn block_stop(&mut self, data: Value) -> Result<Vec<StreamEvent>, ProviderError> {
        self.require_started("content_block_stop")?;
        let event: ContentBlockStop = parse(data, "content_block_stop")?;
        let block = self.open.remove(&event.index).ok_or_else(|| {
            invalid(format!(
                "received stop for unopened content block {}",
                event.index
            ))
        })?;
        let Some(block) = self.finish_block(block)? else {
            return Ok(Vec::new());
        };
        self.complete.insert(event.index, block.clone());
        Ok(vec![StreamEvent::BlockDone {
            index: event.index,
            block,
        }])
    }

    fn finish_block(&self, block: PartialBlock) -> Result<Option<ContentBlock>, ProviderError> {
        match block {
            PartialBlock::Text(text) => Ok(Some(ContentBlock::Text { text })),
            PartialBlock::Thinking { text, signature } => Ok(Some(ContentBlock::Thinking {
                text,
                opaque: signature.map(|data| OpaqueBlob {
                    provider: ProviderId::from(PROVIDER),
                    kind: "signature".to_owned(),
                    data,
                }),
            })),
            PartialBlock::RedactedThinking(data) => Ok(Some(ContentBlock::Thinking {
                text: String::new(),
                opaque: Some(OpaqueBlob {
                    provider: ProviderId::from(PROVIDER),
                    kind: "redacted_thinking".to_owned(),
                    data,
                }),
            })),
            PartialBlock::ToolUse {
                id,
                name,
                initial_input,
                partial_json,
                saw_delta,
            } => {
                let arguments = if saw_delta {
                    serde_json::from_str(&partial_json).map_err(|error| {
                        invalid(format!("tool {name} emitted malformed arguments: {error}"))
                    })?
                } else {
                    initial_input
                };
                let Some(tool) = self.tools.get(&name) else {
                    return Ok(Some(ContentBlock::RejectedToolCall {
                        id: ToolCallId::from(id),
                        name,
                        args: Some(arguments),
                        error: ToolArgumentError {
                            kind: "unknown_tool".to_owned(),
                            message: "provider requested a tool that was not declared".to_owned(),
                        },
                    }));
                };
                match validate_tool_arguments(tool, &arguments) {
                    Ok(()) => Ok(Some(ContentBlock::ToolCall {
                        id: ToolCallId::from(id),
                        name,
                        args: arguments,
                    })),
                    Err(error) => Ok(Some(ContentBlock::RejectedToolCall {
                        id: ToolCallId::from(id),
                        name,
                        args: Some(arguments),
                        error,
                    })),
                }
            }
            PartialBlock::Unknown => Ok(None),
        }
    }

    fn message_delta(&mut self, data: Value) -> Result<Vec<StreamEvent>, ProviderError> {
        self.require_started("message_delta")?;
        let event: MessageDelta = parse(data, "message_delta")?;
        if let Some(stop_reason) = event.delta.stop_reason {
            self.stop = Some(stop_reason_value(&stop_reason));
        }
        self.usage.input_tokens = self.usage.input_tokens.max(event.usage.input_tokens);
        self.usage.output_tokens = event.usage.output_tokens;
        self.usage.cache_read_tokens = self
            .usage
            .cache_read_tokens
            .max(event.usage.cache_read_input_tokens);
        self.usage.cache_write_tokens = self
            .usage
            .cache_write_tokens
            .max(event.usage.cache_creation_input_tokens);
        Ok(Vec::new())
    }

    fn message_stop(&mut self) -> Result<Vec<StreamEvent>, ProviderError> {
        self.require_started("message_stop")?;
        if !self.open.is_empty() {
            return Err(invalid("message_stop arrived with open content blocks"));
        }
        let stop = self
            .stop
            .ok_or_else(|| invalid("message_stop arrived before a stop reason"))?;
        self.stopped = true;
        Ok(vec![StreamEvent::Done(AssistantMessage {
            blocks: self.complete.values().cloned().collect(),
            stop,
            usage: self.usage.clone(),
            provider: ProviderId::from(PROVIDER),
            model: self
                .actual_model
                .clone()
                .unwrap_or_else(|| self.requested_model.clone()),
        })])
    }

    fn require_started(&self, event: &str) -> Result<(), ProviderError> {
        if self.started {
            Ok(())
        } else {
            Err(invalid(format!("received {event} before message_start")))
        }
    }
}

enum PartialBlock {
    Text(String),
    Thinking {
        text: String,
        signature: Option<String>,
    },
    RedactedThinking(String),
    ToolUse {
        id: String,
        name: String,
        initial_input: Value,
        partial_json: String,
        saw_delta: bool,
    },
    Unknown,
}

#[derive(Deserialize)]
struct MessageStart {
    message: StartMessage,
}

#[derive(Deserialize)]
struct StartMessage {
    model: String,
    #[serde(default)]
    usage: WireUsage,
}

#[derive(Default, Deserialize)]
struct WireUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
}

#[derive(Deserialize)]
struct ContentBlockStart {
    index: usize,
    content_block: StartBlock,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StartBlock {
    Text {
        #[serde(default)]
        text: String,
    },
    Thinking {
        #[serde(default)]
        thinking: String,
    },
    RedactedThinking {
        data: String,
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

#[derive(Deserialize)]
struct ContentBlockDelta {
    index: usize,
    delta: BlockDelta,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum BlockDelta {
    #[serde(rename = "text_delta")]
    Text { text: String },
    #[serde(rename = "thinking_delta")]
    Thinking { thinking: String },
    #[serde(rename = "signature_delta")]
    Signature { signature: String },
    #[serde(rename = "input_json_delta")]
    InputJson { partial_json: String },
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
struct ContentBlockStop {
    index: usize,
}

#[derive(Deserialize)]
struct MessageDelta {
    delta: TopLevelDelta,
    #[serde(default)]
    usage: WireUsage,
}

#[derive(Deserialize)]
struct TopLevelDelta {
    stop_reason: Option<String>,
}

fn parse<T: for<'de> Deserialize<'de>>(value: Value, event: &str) -> Result<T, ProviderError> {
    serde_json::from_value(value)
        .map_err(|error| invalid(format!("invalid {event} payload: {error}")))
}

fn invalid(message: impl Into<String>) -> ProviderError {
    ProviderError::invalid_response(message)
}

fn api_error(value: Value) -> ProviderError {
    let error_type = value
        .pointer("/error/type")
        .and_then(Value::as_str)
        .unwrap_or("unknown_error");
    let message = value
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("Anthropic stream returned an error");
    let (kind, retryable) = match error_type {
        "authentication_error" | "permission_error" => (ErrorKind::Authentication, false),
        "rate_limit_error" => (ErrorKind::RateLimited, true),
        "overloaded_error" => (ErrorKind::Overloaded, true),
        "invalid_request_error" => (ErrorKind::InvalidRequest, false),
        _ => (ErrorKind::Other, false),
    };
    ProviderError {
        retryable,
        kind,
        message: message.to_owned(),
    }
}

fn stop_reason_value(reason: &str) -> StopReason {
    match reason {
        "tool_use" => StopReason::ToolUse,
        "max_tokens" | "model_context_window_exceeded" => StopReason::Length,
        "pause_turn" => StopReason::Paused,
        "refusal" => StopReason::Refusal,
        "end_turn" | "stop_sequence" => StopReason::Stop,
        _ => StopReason::Error,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::decoder::Decoder;

    fn bash_tool() -> ToolDefinition {
        ToolDefinition::new(
            "bash",
            "Run a command.",
            json!({
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"],
                "additionalProperties": false
            }),
        )
    }

    fn fixture() -> String {
        [
            r#"event: message_start
data: {"type":"message_start","message":{"model":"claude-sonnet-5","usage":{"input_tokens":12,"output_tokens":1}}}

"#,
            r#"event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}

"#,
            r#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"check"}}

"#,
            r#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-1"}}

"#,
            r#"event: content_block_stop
data: {"type":"content_block_stop","index":0}

"#,
            r#"event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu-1","name":"bash","input":{}}}

"#,
            r#"event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"pwd\"}"}}

"#,
            r#"event: content_block_stop
data: {"type":"content_block_stop","index":1}

"#,
            r#"event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":19}}

"#,
            r#"event: message_stop
data: {"type":"message_stop"}

"#,
        ]
        .concat()
    }

    #[test]
    fn recorded_stream_round_trips_thinking_tools_stop_and_usage() {
        let mut decoder = Decoder::new();
        let events = decoder.feed(fixture().as_bytes()).unwrap();
        let mut assembler =
            ResponseAssembler::new(ModelId::from("claude-sonnet-5"), vec![bash_tool()]);
        let output = events
            .into_iter()
            .flat_map(|event| assembler.accept(event).unwrap())
            .collect::<Vec<_>>();

        let StreamEvent::Done(message) = output.last().unwrap() else {
            panic!("fixture must end in Done");
        };
        assert_eq!(message.stop, StopReason::ToolUse);
        assert_eq!(message.usage.input_tokens, 12);
        assert_eq!(message.usage.output_tokens, 19);
        assert!(matches!(
            &message.blocks[0],
            ContentBlock::Thinking { text, opaque: Some(opaque) }
                if text == "check" && opaque.data == "sig-1"
        ));
        assert!(matches!(
            &message.blocks[1],
            ContentBlock::ToolCall { name, args, .. }
                if name == "bash" && args["command"] == "pwd"
        ));
    }

    #[test]
    fn nonconforming_tool_args_are_structured_rejections_without_coercion() {
        let frames = fixture().replace(r#"{\"command\":\"pwd\"}"#, r#"{\"command\":5}"#);
        let mut decoder = Decoder::new();
        let events = decoder.feed(frames.as_bytes()).unwrap();
        let mut assembler =
            ResponseAssembler::new(ModelId::from("claude-sonnet-5"), vec![bash_tool()]);
        let output = events
            .into_iter()
            .flat_map(|event| assembler.accept(event).unwrap())
            .collect::<Vec<_>>();
        let StreamEvent::Done(message) = output.last().unwrap() else {
            panic!("fixture must end in Done");
        };
        assert!(matches!(
            &message.blocks[1],
            ContentBlock::RejectedToolCall { error, .. }
                if error.kind == "schema_validation"
        ));
    }

    #[test]
    fn malformed_tool_json_never_crosses_boundary() {
        let frames = fixture().replace(r#"{\"command\":\"pwd\"}"#, r#"{\"command\":"#);
        let mut decoder = Decoder::new();
        let events = decoder.feed(frames.as_bytes()).unwrap();
        let mut assembler =
            ResponseAssembler::new(ModelId::from("claude-sonnet-5"), vec![bash_tool()]);
        let error = events
            .into_iter()
            .find_map(|event| assembler.accept(event).err())
            .expect("malformed arguments must be rejected");
        assert_eq!(error.kind, ErrorKind::InvalidResponse);
    }
}
