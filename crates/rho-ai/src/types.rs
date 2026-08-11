use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use serde_json::Value;

macro_rules! string_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Creates an identifier from an owned string.
            #[must_use]
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            /// Borrows the encoded identifier.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl FromStr for $name {
            type Err = std::convert::Infallible;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Ok(Self::from(value))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}

string_id!(ProviderId, "Stable provider identifier.");
string_id!(ModelId, "Provider model identifier.");
string_id!(ToolCallId, "Provider-issued tool-call identifier.");

/// Static model metadata exposed by a configured provider.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelInfo {
    /// Provider model identifier.
    pub id: ModelId,
    /// Human-readable model name.
    pub display_name: String,
    /// Maximum model context when known.
    pub context_tokens: Option<u64>,
    /// Maximum output when known.
    pub max_output_tokens: Option<u64>,
}

/// Requested reasoning effort.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingLevel {
    /// Ask the provider not to reason in a dedicated channel.
    None,
    /// Minimize reasoning work.
    Low,
    /// Balance latency and reasoning depth.
    Medium,
    /// Use high reasoning effort.
    #[default]
    High,
    /// Use extra-high reasoning effort.
    Xhigh,
    /// Use the provider's maximum reasoning effort.
    Max,
}

impl ThinkingLevel {
    /// Returns the stable provider-facing spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

/// Provider-owned replay state. Only the matching adapter may interpret it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OpaqueBlob {
    /// Adapter that produced the state.
    pub provider: ProviderId,
    /// Provider-specific payload kind.
    pub kind: String,
    /// Opaque serialized payload.
    pub data: String,
}

/// A model-visible tool declaration.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolDefinition {
    /// Function name visible to the model.
    pub name: String,
    /// Concrete guidance for when and how to call the function.
    pub description: String,
    /// JSON Schema accepted as arguments.
    pub parameters: Value,
}

impl ToolDefinition {
    /// Creates a tool definition. Call [`crate::validate_tool_definition`] at a trust boundary.
    #[must_use]
    pub fn new(name: impl Into<String>, description: impl Into<String>, parameters: Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }
}

/// Structured rejection for a provider-emitted tool call.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolArgumentError {
    /// Stable machine-readable error class.
    pub kind: String,
    /// Human-readable validation detail safe to return to the model.
    pub message: String,
}

/// One provider-independent message content item.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Plain text.
    Text {
        /// Text payload.
        text: String,
    },
    /// Provider-visible reasoning plus optional provider-owned replay state.
    Thinking {
        /// API-visible thinking text or summary.
        text: String,
        /// Signature, encrypted continuation, or other provider-owned state.
        opaque: Option<OpaqueBlob>,
    },
    /// Inline image bytes.
    Image {
        /// Unencoded image bytes.
        data: Vec<u8>,
        /// MIME type, such as `image/png`.
        mime: String,
    },
    /// Parsed and schema-conforming tool invocation.
    ToolCall {
        /// Provider-issued call identifier.
        id: ToolCallId,
        /// Requested tool name.
        name: String,
        /// Parsed, schema-conforming arguments.
        args: Value,
    },
    /// Tool invocation rejected at the adapter boundary.
    RejectedToolCall {
        /// Provider-issued call identifier when one was available.
        id: ToolCallId,
        /// Requested tool name.
        name: String,
        /// Parsed arguments when parsing succeeded but schema validation failed.
        args: Option<Value>,
        /// Structured parse or schema failure.
        error: ToolArgumentError,
    },
}

impl ContentBlock {
    /// Creates a text block.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}

/// Why an assistant generation stopped.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// Natural or stop-sequence completion.
    Stop,
    /// One or more client tools were requested.
    ToolUse,
    /// Output or context length was exhausted.
    Length,
    /// Provider reported a terminal generation error.
    Error,
    /// Caller cancellation won the race.
    Aborted,
    /// Provider paused a long-running turn for explicit continuation.
    Paused,
    /// Provider refused the request.
    Refusal,
}

/// Token accounting returned by a provider.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Usage {
    /// Input tokens billed for the request.
    pub input_tokens: u64,
    /// Output tokens billed for the request.
    pub output_tokens: u64,
    /// Input tokens served from cache.
    pub cache_read_tokens: u64,
    /// Input tokens written to cache.
    pub cache_write_tokens: u64,
}

/// Authoritative terminal assistant message.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AssistantMessage {
    /// Ordered content blocks.
    pub blocks: Vec<ContentBlock>,
    /// Terminal reason.
    pub stop: StopReason,
    /// Provider token accounting.
    pub usage: Usage,
    /// Adapter that produced the message.
    pub provider: ProviderId,
    /// Actual serving model.
    pub model: ModelId,
}

/// One item in the authoritative transcript sent on every request.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    /// User-authored content.
    User {
        /// Ordered user content.
        content: Vec<ContentBlock>,
    },
    /// A prior authoritative provider response.
    Assistant(AssistantMessage),
    /// Result paired with a prior tool call.
    ToolResult(ToolResult),
}

impl Message {
    /// Creates a text-only user message.
    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        Self::User {
            content: vec![ContentBlock::text(text)],
        }
    }
}

/// Result returned after a client tool call.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolResult {
    /// Tool call being answered.
    pub call_id: ToolCallId,
    /// Text result visible to the model.
    pub content: String,
    /// Whether execution or argument validation failed.
    pub is_error: bool,
}

/// Configuration fixed for one live provider session.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionConfig {
    /// Provider model used for every generation in the session.
    pub model: ModelId,
}

/// Complete authoritative provider request.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Request {
    /// Stable developer/system instructions.
    pub system: String,
    /// Complete authoritative transcript.
    pub messages: Vec<Message>,
    /// Complete tool set available for this request.
    pub tools: Vec<ToolDefinition>,
    /// Hard provider output-token limit.
    pub max_output_tokens: u64,
    /// Requested reasoning effort.
    pub thinking: ThinkingLevel,
}

/// Kind of an advisory streaming delta.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeltaKind {
    /// Assistant text.
    Text,
    /// API-visible reasoning.
    Thinking,
    /// Raw, unparsed tool argument fragment for display only.
    ToolArguments,
}

/// Stable provider failure category.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// Missing or rejected credentials.
    Authentication,
    /// Request rate limit.
    RateLimited,
    /// Temporary provider overload.
    Overloaded,
    /// Caller supplied an unsupported or invalid request.
    InvalidRequest,
    /// Provider output violated the adapter contract.
    InvalidResponse,
    /// Context window was exceeded.
    ContextWindowExceeded,
    /// HTTP, socket, or stream transport failure.
    Transport,
    /// Caller cancellation.
    Cancelled,
    /// Provider-specific terminal failure.
    Other,
}

/// Typed provider failure. Adapters classify; the future loop owns retries.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProviderError {
    /// Whether invoking the request again may recover.
    pub retryable: bool,
    /// Stable category.
    pub kind: ErrorKind,
    /// Safe human-readable detail.
    pub message: String,
}

impl ProviderError {
    /// Creates a terminal adapter-boundary rejection.
    #[must_use]
    pub fn invalid_response(message: impl Into<String>) -> Self {
        Self {
            retryable: false,
            kind: ErrorKind::InvalidResponse,
            message: message.into(),
        }
    }

    /// Creates a caller-cancellation error.
    #[must_use]
    pub fn cancelled() -> Self {
        Self {
            retryable: false,
            kind: ErrorKind::Cancelled,
            message: "provider request cancelled".to_owned(),
        }
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ProviderError {}

/// Provider stream event. Only [`StreamEvent::Done`] is authoritative state.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// Provider accepted the request.
    Start,
    /// Advisory incremental display fragment.
    Delta {
        /// Provider block index.
        index: usize,
        /// Fragment kind.
        kind: DeltaKind,
        /// Newly received fragment.
        delta: String,
    },
    /// One fully parsed block is available.
    BlockDone {
        /// Provider block index.
        index: usize,
        /// Parsed block.
        block: ContentBlock,
    },
    /// Authoritative terminal message.
    Done(AssistantMessage),
    /// Terminal typed failure.
    Error(ProviderError),
}
