use std::{fmt, str::FromStr};

use rho_ai::{
    AssistantMessage, ContentBlock, Message as ProviderMessage, ModelId, ProviderId, ThinkingLevel,
    ToolCallId, ToolResult, Usage,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The only JSONL session format version this build accepts.
pub const FORMAT_VERSION: u32 = 1;

macro_rules! string_value {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Creates a value from its wire representation.
            #[must_use]
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            /// Borrows the wire representation.
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

string_value!(SessionId, "Globally unique session identifier.");
string_value!(EntryId, "Globally unique transcript-entry identifier.");
string_value!(OpId, "Globally unique operation identifier.");
string_value!(QueueId, "Stable identifier for a queued command.");
string_value!(Timestamp, "Opaque UTC timestamp minted by a mutable shell.");
string_value!(LaneName, "A named line of execution within a session.");

impl LaneName {
    /// The single lane supported by format v1.
    pub const MAIN: &'static str = "main";

    /// Returns the v1 main lane.
    #[must_use]
    pub fn main() -> Self {
        Self::from(Self::MAIN)
    }
}

/// Lineage recorded when a session is forked.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ForkParent {
    /// Source session.
    pub session: SessionId,
    /// Source entry copied through in the fork.
    pub entry: EntryId,
}

/// First line of a session file.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionHeader {
    /// Wire-format version.
    pub v: u32,
    /// Session identity.
    pub id: SessionId,
    /// Creation timestamp.
    pub created_at: Timestamp,
    /// Working directory captured at creation.
    pub cwd: String,
    /// Optional fork lineage.
    pub parent: Option<ForkParent>,
}

/// Provider and model selected by a settings entry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelRef {
    /// Adapter identifier.
    pub provider: ProviderId,
    /// Provider model identifier.
    pub model: ModelId,
}

/// A message stored in the transcript plane.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum SessionMessage {
    /// User-authored content.
    User {
        /// Ordered content blocks.
        content: Vec<ContentBlock>,
    },
    /// Verbatim authoritative provider output.
    Assistant(AssistantMessage),
    /// Result closing a provider tool call.
    ToolResult {
        /// Provider-issued call identifier.
        call_id: ToolCallId,
        /// Ordered result content.
        content: Vec<ContentBlock>,
        /// Whether execution failed.
        is_error: bool,
        /// Optional structured diagnostic details not sent to providers.
        details: Option<Value>,
    },
    /// Extension-injected model-visible context.
    Custom {
        /// Extension-defined discriminator.
        tag: String,
        /// Ordered model-visible content.
        content: Vec<ContentBlock>,
    },
}

impl SessionMessage {
    /// Creates a text-only user message.
    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        Self::User {
            content: vec![ContentBlock::text(text)],
        }
    }

    /// Projects durable session content onto the provider boundary.
    #[must_use]
    pub fn to_provider(&self) -> ProviderMessage {
        match self {
            Self::User { content } | Self::Custom { content, .. } => ProviderMessage::User {
                content: content.clone(),
            },
            Self::Assistant(message) => ProviderMessage::Assistant(message.clone()),
            Self::ToolResult {
                call_id,
                content,
                is_error,
                ..
            } => ProviderMessage::ToolResult(ToolResult {
                call_id: call_id.clone(),
                content: render_tool_content(content),
                is_error: *is_error,
            }),
        }
    }
}

fn render_tool_content(content: &[ContentBlock]) -> String {
    content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => text.clone(),
            other => serde_json::to_string(other)
                .unwrap_or_else(|error| format!("<unserializable tool content: {error}>")),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Transcript entry payload.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntryBody {
    /// Model-visible conversation content.
    Message {
        /// Durable message.
        message: SessionMessage,
    },
    /// Self-contained compaction checkpoint.
    Compaction {
        /// Summary of compacted history.
        summary: String,
        /// Optional back-pointer representation of retained history.
        first_kept: Option<EntryId>,
        /// Self-contained retained tail.
        retained_tail: Vec<SessionMessage>,
        /// Token estimate before compaction.
        tokens_before: u64,
        /// Usage of the summarization call.
        usage: Usage,
    },
    /// Model or reasoning setting change.
    SettingsChange {
        /// New model when changed.
        model: Option<ModelRef>,
        /// New reasoning level when changed.
        thinking: Option<ThinkingLevel>,
    },
    /// Extension state excluded from model context.
    Custom {
        /// Extension-defined discriminator.
        tag: String,
        /// Opaque extension data.
        data: Value,
    },
}

/// An append-only transcript entry with its storage sequence.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Entry {
    /// Per-session total-order sequence.
    pub seq: u64,
    /// Entry identity.
    pub id: EntryId,
    /// Parent entry in the transcript tree.
    pub parent: Option<EntryId>,
    /// Owning lane.
    pub lane: LaneName,
    /// Operation that produced this entry, if any.
    pub op: Option<OpId>,
    /// Shell-minted timestamp.
    pub at: Timestamp,
    /// Entry payload.
    pub body: EntryBody,
}

/// Entry supplied to storage before it receives a sequence number.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct NewEntry {
    /// Entry identity.
    pub id: EntryId,
    /// Parent entry in the transcript tree.
    pub parent: Option<EntryId>,
    /// Owning lane.
    pub lane: LaneName,
    /// Operation that produced this entry, if any.
    pub op: Option<OpId>,
    /// Shell-minted timestamp.
    pub at: Timestamp,
    /// Entry payload.
    pub body: EntryBody,
}

/// Why an operation was started.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OpIntent {
    /// Normal agent run.
    Run,
    /// Explicit compaction run.
    Compaction,
}

/// Provenance for side-effecting work.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// Requested by an external caller.
    External,
    /// Synthesized while recovering durable state.
    Replay,
}

/// Whether interrupted tool execution may be repeated.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplaySafety {
    /// Re-execution is permitted.
    Safe,
    /// Re-execution may duplicate an unsafe side effect.
    Never,
}

/// Final operation outcome.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum OpOutcome {
    /// Work reached a normal terminal state.
    Completed,
    /// Work was cancelled.
    Aborted,
    /// Work failed.
    Failed {
        /// Stable human-readable failure.
        error: String,
    },
}

/// Opaque host metadata recorded for diagnostics and fencing.
pub type HostInfo = Value;

/// Kind of queued user input.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueKind {
    /// Inject during the active run.
    Steer,
    /// Start after the active run.
    FollowUp,
}

/// Durable change to a lane's input queues.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum QueueChange {
    /// Add a queued message.
    Enqueued {
        /// Queue item identity.
        id: QueueId,
        /// Queue semantics.
        kind: QueueKind,
        /// Verbatim queued message.
        message: SessionMessage,
    },
    /// Cancel a queued message without deleting its audit record.
    Cancelled {
        /// Queue item identity.
        id: QueueId,
    },
}

/// Journal record payload.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecordBody {
    /// Opens an operation.
    OpStarted {
        /// Operation identity.
        op: OpId,
        /// Operation intent.
        intent: OpIntent,
        /// Request provenance.
        origin: Origin,
        /// Optional host fencing metadata.
        host: Option<HostInfo>,
    },
    /// Closes an operation.
    OpFinished {
        /// Operation identity.
        op: OpId,
        /// Terminal result.
        outcome: OpOutcome,
    },
    /// Records cooperative cancellation.
    AbortRequested {
        /// Operation identity.
        op: OpId,
    },
    /// Records one provider generation attempt.
    Step {
        /// Operation identity.
        op: OpId,
        /// Consecutive one-based attempt number.
        n: u32,
    },
    /// Records the exact tool invocation before execution.
    ToolStarted {
        /// Operation identity.
        op: OpId,
        /// Provider-issued call identifier.
        call_id: ToolCallId,
        /// Tool name.
        name: String,
        /// Post-hook arguments actually executed.
        effective_args: Value,
        /// Interruption replay policy.
        replay: ReplaySafety,
    },
    /// Records queue state.
    QueueChanged {
        /// Active operation, when the change happened during one.
        op: Option<OpId>,
        /// Queue mutation.
        change: QueueChange,
    },
    /// Records provider token accounting.
    Usage {
        /// Operation identity.
        op: OpId,
        /// Provider usage.
        usage: Usage,
    },
    /// Moves a lane leaf without rewriting transcript data.
    LaneMoved {
        /// New leaf.
        to: EntryId,
    },
}

impl RecordBody {
    /// Returns the operation referenced by this record, when any.
    #[must_use]
    pub fn op(&self) -> Option<&OpId> {
        match self {
            Self::OpStarted { op, .. }
            | Self::OpFinished { op, .. }
            | Self::AbortRequested { op }
            | Self::Step { op, .. }
            | Self::ToolStarted { op, .. }
            | Self::Usage { op, .. } => Some(op),
            Self::QueueChanged { op, .. } => op.as_ref(),
            Self::LaneMoved { .. } => None,
        }
    }
}

/// An append-only journal record with its storage sequence.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Record {
    /// Per-session total-order sequence.
    pub seq: u64,
    /// Owning lane.
    pub lane: LaneName,
    /// Shell-minted timestamp.
    pub at: Timestamp,
    /// Record payload.
    pub body: RecordBody,
}

/// Record supplied to storage before it receives a sequence number.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct NewRecord {
    /// Owning lane.
    pub lane: LaneName,
    /// Shell-minted timestamp.
    pub at: Timestamp,
    /// Record payload.
    pub body: RecordBody,
}

/// Last-writer-wins session fact.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Fact {
    /// Per-session total-order sequence.
    pub seq: u64,
    /// Shell-minted timestamp.
    pub at: Timestamp,
    /// Fact key.
    pub key: String,
    /// Fact value.
    pub value: Value,
}

/// Fact supplied to storage before it receives a sequence number.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct NewFact {
    /// Shell-minted timestamp.
    pub at: Timestamp,
    /// Fact key.
    pub key: String,
    /// Fact value.
    pub value: Value,
}

/// One item in the interleaved per-session stream.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "t", content = "item", rename_all = "snake_case")]
pub enum Item {
    /// Transcript item.
    Entry(Entry),
    /// Journal item.
    Record(Record),
    /// Session fact update.
    Fact(Fact),
}

impl Item {
    /// Returns this item's total-order sequence.
    #[must_use]
    pub fn seq(&self) -> u64 {
        match self {
            Self::Entry(entry) => entry.seq,
            Self::Record(record) => record.seq,
            Self::Fact(fact) => fact.seq,
        }
    }
}

/// Tool execution left open by a crash.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct OpenTool {
    /// Provider-issued call identifier.
    pub call_id: ToolCallId,
    /// Tool name.
    pub name: String,
    /// Exact post-hook arguments.
    pub effective_args: Value,
    /// Interruption replay policy.
    pub replay: ReplaySafety,
}

/// Recoverable operation state.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SuspendedOp {
    /// Operation identity.
    pub op: OpId,
    /// Whether the opening record was durable before interruption.
    pub operation_started: bool,
    /// Original intent.
    pub intent: OpIntent,
    /// Whether cancellation was durable before the crash.
    pub abort_requested: bool,
    /// Last provider step, if any.
    pub last_step: Option<u32>,
    /// Tool calls without durable results.
    pub open_tools: Vec<OpenTool>,
    /// Whether a provider step lacks an assistant result.
    pub stream_in_flight: bool,
    /// Latest durable assistant result for the operation, if any.
    pub last_assistant: Option<AssistantMessage>,
    /// Whether usage for `last_assistant` is already durable.
    pub last_assistant_usage_recorded: bool,
    /// Calls from `last_assistant` that already have durable results.
    pub resolved_tool_calls: Vec<ToolCallId>,
}

/// Named impossible journal sequence.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CorruptionReason {
    /// Item sequence numbers are not strictly consecutive.
    NonConsecutiveSequence {
        /// Expected sequence.
        expected: u64,
        /// Observed sequence.
        actual: u64,
    },
    /// More than one operation was simultaneously open.
    MultipleOpenOperations {
        /// Existing open operation.
        first: OpId,
        /// Newly opened operation.
        second: OpId,
    },
    /// An operation was started twice.
    DuplicateOperation {
        /// Duplicated operation.
        op: OpId,
    },
    /// An item refers to an operation that was never started.
    UnknownOperation {
        /// Missing operation.
        op: OpId,
    },
    /// An item was appended for an already finished operation.
    ItemAfterFinish {
        /// Closed operation.
        op: OpId,
    },
    /// Provider step numbering skipped or repeated a number.
    NonConsecutiveStep {
        /// Operation identity.
        op: OpId,
        /// Expected step.
        expected: u32,
        /// Observed step.
        actual: u32,
    },
    /// An assistant result did not follow an open provider step.
    AssistantWithoutStep {
        /// Operation identity.
        op: OpId,
    },
    /// Tool execution began before the provider step produced a message.
    ToolStartedBeforeAssistant {
        /// Operation identity.
        op: OpId,
        /// Premature call identifier.
        call_id: ToolCallId,
    },
    /// A tool start reused a still-open call identifier.
    DuplicateToolStart {
        /// Operation identity.
        op: OpId,
        /// Duplicated call identifier.
        call_id: ToolCallId,
    },
    /// A tool result had no corresponding open tool start.
    ToolResultWithoutStart {
        /// Operation identity.
        op: OpId,
        /// Unknown or already-closed call identifier.
        call_id: ToolCallId,
    },
    /// An operation closed while tool calls still lacked results.
    FinishedWithOpenTools {
        /// Operation identity.
        op: OpId,
        /// Still-open calls in start order.
        call_ids: Vec<ToolCallId>,
    },
    /// A completed operation still had a provider stream in flight.
    CompletedWithStreamInFlight {
        /// Operation identity.
        op: OpId,
    },
}

/// Cheap lane recovery result.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum LaneStatus {
    /// No operation is open.
    Idle,
    /// Exactly one operation is recoverable.
    Suspended(SuspendedOp),
    /// The journal contains an impossible sequence.
    Corrupt(CorruptionReason),
}
