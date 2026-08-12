use std::collections::VecDeque;

use rho_ai::{
    AssistantMessage, ContentBlock, ErrorKind, ProviderError, Request, StopReason, ThinkingLevel,
    ToolCallId, ToolDefinition, Usage,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::{
    CompactionWork, ContextError, Entry, EntryBody, EntryId, HookInvocation, HookOutput, HookPoint,
    HostInfo, InteractionAnswer, InteractionRequest, LaneName, LaneStatus, NewEntry, NewRecord,
    OpId, OpIntent, OpOutcome, Origin, QueueChange, QueueId, QueueKind, QueuedInput, RecordBody,
    ReplaySafety, SessionMessage, Timestamp, assemble_context, plan_compaction,
};

/// Asynchronous control-plane input accepted while a run is active.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum SessionControl {
    /// Queue steering or follow-up input.
    Enqueue {
        /// Stable host-minted queue identity.
        id: QueueId,
        /// Queue semantics.
        kind: QueueKind,
        /// Verbatim user message.
        message: SessionMessage,
    },
    /// Cancel a pending queue item.
    Cancel {
        /// Queue item identity.
        id: QueueId,
    },
    /// Cooperatively abort the active operation.
    Abort,
    /// Answer a pending headless interaction.
    AnswerInteraction {
        /// Stable request identity.
        id: String,
        /// Client answer.
        answer: InteractionAnswer,
    },
}

/// Immutable tool metadata used by the pure machine.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolSpec {
    /// Provider-visible declaration.
    pub definition: ToolDefinition,
    /// Crash replay policy.
    pub replay: ReplaySafety,
}

/// Configuration fixed for a machine incarnation.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MachineConfig {
    /// Stable developer/system instructions.
    pub system: String,
    /// Maximum provider output tokens.
    pub max_output_tokens: u64,
    /// Requested reasoning level.
    pub thinking: ThinkingLevel,
    /// Provider/model required for generation actions.
    pub model: crate::ModelRef,
    /// Available tools and their durability metadata.
    pub tools: Vec<ToolSpec>,
    /// Hook points enabled for this machine incarnation.
    pub hooks: Vec<HookPoint>,
    /// Automatic context compaction policy, or `None` to disable it.
    pub compaction: Option<CompactionConfig>,
}

/// Simple summarize-and-truncate policy.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CompactionConfig {
    /// Compact after a provider request reports at least this many input tokens.
    pub threshold_tokens: u64,
    /// Number of newest messages copied into the self-contained checkpoint.
    pub retain_messages: usize,
    /// System instructions used for the isolated summarization request.
    pub system_prompt: String,
}

/// Authoritative output of a compaction summarization request.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CompactionSummary {
    /// Plain-text history summary.
    pub text: String,
    /// Provider token accounting.
    pub usage: Usage,
}

/// Shell-minted identity and time for a transcript effect.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EntryStamp {
    /// Globally unique entry identity.
    pub id: EntryId,
    /// Wall-clock timestamp.
    pub at: Timestamp,
}

/// A validated tool request ready for the mutable shell.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PreparedToolCall {
    /// Provider-issued call identifier.
    pub call_id: ToolCallId,
    /// Requested tool name.
    pub name: String,
    /// Parsed effective arguments.
    pub effective_args: Value,
    /// Crash replay policy.
    pub replay: ReplaySafety,
    /// Deterministic failure that the shell returns without invoking a tool.
    pub precomputed_error: Option<String>,
}

/// Pure instruction for a mutable shell.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    /// Generate one authoritative assistant message.
    StreamAssistant {
        /// Complete transcript-authoritative request.
        request: Request,
        /// Provider session the shell must have open.
        model: crate::ModelRef,
        /// Side-effect provenance.
        origin: Origin,
    },
    /// Execute one prepared tool call.
    ExecuteTool {
        /// Exact invocation, including replay policy.
        call: PreparedToolCall,
        /// Side-effect provenance.
        origin: Origin,
    },
    /// Generate a compaction summary.
    Summarize {
        /// Isolated summarization request.
        request: Request,
        /// Provider session the shell must have open.
        model: crate::ModelRef,
        /// Side-effect provenance.
        origin: Origin,
    },
    /// Ask the client a question.
    AwaitInteraction {
        /// Owned interaction payload.
        request: InteractionRequest,
        /// Side-effect provenance.
        origin: Origin,
    },
    /// Invoke an extension hook.
    InvokeHook {
        /// Owned hook payload.
        invocation: HookInvocation,
        /// Side-effect provenance.
        origin: Origin,
    },
}

/// Advisory event emitted by the deterministic machine.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentEvent {
    /// An operation began.
    OperationStarted {
        /// Operation identity.
        op: OpId,
        /// Provenance.
        origin: Origin,
    },
    /// A durable message was derived.
    MessageAppended {
        /// Operation identity.
        op: OpId,
        /// Message payload.
        message: SessionMessage,
    },
    /// Tool execution is about to begin.
    ToolExecutionStarted {
        /// Operation identity.
        op: OpId,
        /// Provider-issued call identifier.
        call_id: ToolCallId,
        /// Tool name.
        name: String,
    },
    /// Context compaction began.
    CompactionStarted {
        /// Operation identity.
        op: OpId,
        /// Token estimate before compaction.
        tokens_before: u64,
    },
    /// A self-contained compaction checkpoint was appended.
    CompactionFinished {
        /// Operation identity.
        op: OpId,
        /// Generated summary.
        summary: String,
    },
    /// Durable steering or follow-up state changed.
    QueueChanged {
        /// Queue mutation.
        change: QueueChange,
    },
    /// Cooperative cancellation was requested.
    AbortRequested {
        /// Active operation identity.
        op: OpId,
    },
    /// A durable hook action began.
    HookStarted {
        /// Operation identity.
        op: OpId,
        /// Hook action number.
        n: u32,
        /// Exact invocation.
        invocation: HookInvocation,
    },
    /// A durable hook action completed.
    HookFinished {
        /// Operation identity.
        op: OpId,
        /// Hook action number.
        n: u32,
        /// Terminal hook result.
        result: Result<HookOutput, String>,
    },
    /// A headless client interaction is awaiting an answer.
    InteractionRequested {
        /// Operation identity.
        op: OpId,
        /// Exact client request.
        request: InteractionRequest,
    },
    /// A headless client interaction was durably answered.
    InteractionAnswered {
        /// Operation identity.
        op: OpId,
        /// Stable request identity.
        request_id: String,
        /// Durable answer.
        answer: InteractionAnswer,
    },
    /// Advisory provider stream event. Snapshots remain authoritative.
    ProviderStream {
        /// Transient provider event.
        event: rho_ai::StreamEvent,
    },
    /// An operation reached a terminal state.
    OperationFinished {
        /// Operation identity.
        op: OpId,
        /// Terminal result.
        outcome: OpOutcome,
    },
}

/// Deterministic durable or advisory output.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Effect {
    /// Append to the transcript plane.
    AppendEntry(NewEntry),
    /// Append to the journal plane.
    AppendRecord(NewRecord),
    /// Publish an advisory event.
    Emit(AgentEvent),
}

/// Input command accepted by the machine.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Input {
    /// Begin a normal operation with a user message.
    Prompt {
        /// User message.
        message: SessionMessage,
        /// Shell-minted operation identity.
        op: OpId,
        /// Shell-minted entry identity and time.
        stamp: EntryStamp,
        /// Request provenance.
        origin: Origin,
        /// Optional host fencing metadata.
        host: Option<HostInfo>,
        /// Queue item promoted into this prompt, when any.
        queue: Option<crate::QueueId>,
        /// Shell-minted stamps for steering already queued at the pre-poll.
        steer_stamps: Vec<EntryStamp>,
    },
    /// Begin an explicit or automatically triggered compaction operation.
    Compact {
        /// Shell-minted operation identity.
        op: OpId,
        /// Shell-minted timestamp for journal effects.
        at: Timestamp,
        /// Request provenance.
        origin: Origin,
        /// Optional host fencing metadata.
        host: Option<HostInfo>,
    },
    /// Inspect recovery state before resuming in the shell.
    Resume {
        /// Reducer result read from storage.
        status: LaneStatus,
        /// Shell-minted timestamp for newly derived recovery effects.
        at: Timestamp,
        /// Shell-minted stamps for steering already queued at the pre-poll.
        steer_stamps: Vec<EntryStamp>,
    },
}

/// Result fed back after a shell action.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActionOutcome {
    /// Provider generation completed or failed.
    Assistant {
        /// Terminal provider message or normalized error.
        result: Result<AssistantMessage, ProviderError>,
        /// Identity and time used if a message is appended.
        stamp: EntryStamp,
        /// Stamps for steering received while generation was in flight.
        steer_stamps: Vec<EntryStamp>,
    },
    /// Tool execution completed.
    Tool {
        /// Provider-issued call identifier.
        call_id: ToolCallId,
        /// Model-visible result content.
        content: Vec<ContentBlock>,
        /// Whether execution failed.
        is_error: bool,
        /// Optional structured diagnostic details.
        details: Option<Value>,
        /// Identity and time for the result entry.
        stamp: EntryStamp,
        /// Stamps for steering received while tool execution was in flight.
        steer_stamps: Vec<EntryStamp>,
    },
    /// A summarization action completed.
    Summary {
        /// Summary or normalized error.
        result: Result<CompactionSummary, ProviderError>,
        /// Identity and time for a future checkpoint entry.
        stamp: EntryStamp,
    },
    /// A client interaction completed.
    Interaction {
        /// Stable request identity.
        request_id: String,
        /// Durable answer.
        answer: InteractionAnswer,
        /// Shell-minted time for the durable answer.
        at: Timestamp,
    },
    /// A hook completed.
    Hook {
        /// Hook decision or normalized host failure.
        result: Result<crate::HookOutput, String>,
        /// Shell-minted time for the durable outcome.
        at: Timestamp,
    },
}

/// Output of one pure transition.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Step {
    /// Apply effects in order, then execute at most one action.
    Do {
        /// Ordered effects.
        effects: Vec<Effect>,
        /// Optional shell action.
        action: Option<Action>,
    },
    /// An action is already in flight.
    AwaitingOutcome,
    /// No operation or queued work remains.
    Idle,
}

/// Invalid state-machine transition.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum MachineError {
    /// A command requires an idle machine.
    #[error("an operation is already in progress")]
    Busy,
    /// An outcome arrived with no matching action.
    #[error("no action is awaiting an outcome")]
    UnexpectedOutcome,
    /// An outcome kind did not match the pending action.
    #[error("outcome did not match the pending action")]
    MismatchedOutcome,
    /// Tool result did not match the requested call.
    #[error("tool result for {actual} arrived while awaiting {expected}")]
    MismatchedToolCall {
        /// Expected call.
        expected: ToolCallId,
        /// Actual call.
        actual: ToolCallId,
    },
    /// Resume cannot continue a corrupt lane.
    #[error("cannot resume a corrupt lane")]
    CorruptResume,
    /// Prompt commands must introduce user-authored content.
    #[error("prompt input must be a user message")]
    InvalidPrompt,
    /// Queued control-plane messages must be user-authored content.
    #[error("queued input must be a user message")]
    InvalidQueuedInput,
    /// A pending queue identity was reused.
    #[error("queue item {0} is already pending")]
    DuplicateQueueItem(QueueId),
    /// A cancellation named an item not currently pending.
    #[error("queue item {0} is not pending")]
    UnknownQueueItem(QueueId),
    /// An abort was requested while no operation was active.
    #[error("no operation is active")]
    IdleAbort,
    /// The shell supplied the wrong number of steering entry stamps.
    #[error("expected {expected} steering stamps, received {actual}")]
    SteerStampCount {
        /// Number of pending steering items.
        expected: usize,
        /// Number of supplied stamps.
        actual: usize,
    },
    /// A hook returned a value that does not satisfy its typed point contract.
    #[error("hook {hook:?} returned an invalid result")]
    InvalidHookResult {
        /// Hook point whose result failed validation.
        hook: HookPoint,
    },
    /// An interaction answer did not match the pending request.
    #[error("interaction answer for {actual:?} arrived while awaiting {expected:?}")]
    MismatchedInteraction {
        /// Pending request identity.
        expected: String,
        /// Supplied request identity.
        actual: String,
    },
    /// Hook action numbering overflowed.
    #[error("hook action sequence exhausted")]
    HookSequenceExhausted,
    /// Compaction was requested without an enabled policy.
    #[error("compaction is disabled for this machine")]
    CompactionDisabled,
    /// There is no old context to summarize under the configured retention policy.
    #[error("the current context does not require compaction")]
    NothingToCompact,
}

#[derive(Clone, Debug, PartialEq)]
enum Phase {
    Idle,
    AwaitingAssistant {
        op: OpId,
        step: u32,
        origin: Origin,
    },
    AwaitingTool(AwaitingTool),
    AwaitingSummary {
        op: OpId,
        step: u32,
        work: CompactionWork,
        origin: Origin,
    },
    AwaitingHook(AwaitingHook),
    AwaitingInteraction(AwaitingInteraction),
}

#[derive(Clone, Debug, PartialEq)]
struct AwaitingHook {
    op: OpId,
    n: u32,
    invocation: HookInvocation,
    origin: Origin,
    continuation: HookContinuation,
}

#[derive(Clone, Debug, PartialEq)]
struct AwaitingInteraction {
    hook: AwaitingHook,
    request: InteractionRequest,
}

#[derive(Clone, Debug, PartialEq)]
enum HookContinuation {
    RunStarted {
        step: u32,
    },
    TransformContext {
        step: u32,
    },
    BeforeRequest {
        step: u32,
        request: Request,
    },
    AfterRequest {
        step: u32,
        message: AssistantMessage,
        at: Timestamp,
        steer_stamps: Vec<EntryStamp>,
    },
    BeforeTool {
        pending: AwaitingTool,
    },
    AfterTool {
        pending: AwaitingTool,
        at: Timestamp,
        steer_stamps: Vec<EntryStamp>,
    },
    BeforeCompaction {
        step: u32,
        work: CompactionWork,
        request: Request,
    },
    RunFinished {
        outcome: OpOutcome,
        at: Timestamp,
    },
}

struct HookStart {
    op: OpId,
    hook: HookPoint,
    payload: Value,
    origin: Origin,
    continuation: HookContinuation,
    at: Timestamp,
    effects: Vec<Effect>,
}

struct SummaryStart {
    op: OpId,
    step: u32,
    work: CompactionWork,
    request: Request,
    origin: Origin,
    at: Timestamp,
    effects: Vec<Effect>,
}

struct AssistantBoundary {
    op: OpId,
    step: u32,
    origin: Origin,
    message: AssistantMessage,
    at: Timestamp,
    steer_stamps: Vec<EntryStamp>,
    effects: Vec<Effect>,
}

struct RecoveryAssistantBoundary {
    boundary: AssistantBoundary,
    open_tools: Vec<crate::OpenTool>,
    resolved_tool_calls: Vec<ToolCallId>,
}

#[derive(Clone, Debug, PartialEq)]
struct AwaitingTool {
    op: OpId,
    step: u32,
    current: PendingTool,
    remaining: VecDeque<PendingTool>,
    after: AfterTools,
    origin: Origin,
}

#[derive(Clone, Debug, PartialEq)]
struct PendingTool {
    call: PreparedToolCall,
    journal_start: bool,
}

#[derive(Clone, Debug, PartialEq)]
enum AfterTools {
    Stream,
    Finish(OpOutcome),
}

/// Pure, explicitly driven session state machine.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionMachine {
    config: MachineConfig,
    entries: Vec<Entry>,
    provider_messages: Vec<rho_ai::Message>,
    leaf: Option<EntryId>,
    phase: Phase,
    last_input_tokens: u64,
    queued: VecDeque<QueuedInput>,
    abort_requested: bool,
    hook_n: u32,
}

impl SessionMachine {
    /// Creates a machine from one current root-to-leaf branch.
    pub fn new(mut config: MachineConfig, entries: Vec<Entry>) -> Result<Self, ContextError> {
        let context = assemble_context(&entries)?;
        if let Some(model) = context.settings.model {
            config.model = model;
        }
        if let Some(thinking) = context.settings.thinking {
            config.thinking = thinking;
        }
        let leaf = entries.last().map(|entry| entry.id.clone());
        let context_start = entries
            .iter()
            .rposition(|entry| matches!(entry.body, EntryBody::Compaction { .. }))
            .map_or(0, |index| index + 1);
        let last_input_tokens = entries[context_start..]
            .iter()
            .rev()
            .find_map(|entry| match &entry.body {
                EntryBody::Message {
                    message: SessionMessage::Assistant(message),
                } => Some(message.usage.input_tokens),
                _ => None,
            })
            .unwrap_or(0);
        Ok(Self {
            config,
            entries,
            provider_messages: context.messages,
            leaf,
            phase: Phase::Idle,
            last_input_tokens,
            queued: VecDeque::new(),
            abort_requested: false,
            hook_n: 0,
        })
    }

    /// Returns whether no action or operation is active.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.phase == Phase::Idle
    }

    /// Returns the durable effective provider/model selection.
    #[must_use]
    pub fn model(&self) -> &crate::ModelRef {
        &self.config.model
    }

    /// Returns the root-to-leaf branch this machine was reconstructed from.
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Returns the latest provider input-token estimate when policy says to compact.
    #[must_use]
    pub fn compaction_due(&self) -> Option<u64> {
        let policy = self.config.compaction.as_ref()?;
        (self.is_idle()
            && self.last_input_tokens >= policy.threshold_tokens
            && !plan_compaction(&self.entries, policy.retain_messages)
                .compacted
                .is_empty())
        .then_some(self.last_input_tokens)
    }

    /// Rehydrates pending durable input before starting or resuming a run.
    pub fn hydrate_queue(&mut self, queued: Vec<QueuedInput>) -> Result<(), MachineError> {
        if self.phase != Phase::Idle {
            return Err(MachineError::Busy);
        }
        if queued
            .iter()
            .any(|item| !matches!(item.message, SessionMessage::User { .. }))
        {
            return Err(MachineError::InvalidQueuedInput);
        }
        self.queued = queued.into();
        Ok(())
    }

    /// Returns the number of pending steering messages.
    #[must_use]
    pub fn pending_steers(&self) -> usize {
        self.queued
            .iter()
            .filter(|item| item.kind == QueueKind::Steer)
            .count()
    }

    /// Returns whether any durable queued input remains.
    #[must_use]
    pub fn has_queued_input(&self) -> bool {
        !self.queued.is_empty()
    }

    /// Applies one control-plane command while preserving the pending action.
    pub fn accept_control(
        &mut self,
        control: SessionControl,
        at: Timestamp,
    ) -> Result<Vec<Effect>, MachineError> {
        let op = self.active_op().cloned();
        match control {
            SessionControl::Enqueue { id, kind, message } => {
                if !matches!(message, SessionMessage::User { .. }) {
                    return Err(MachineError::InvalidQueuedInput);
                }
                if self.queued.iter().any(|item| item.id == id) {
                    return Err(MachineError::DuplicateQueueItem(id));
                }
                let item = QueuedInput {
                    id: id.clone(),
                    kind,
                    message: message.clone(),
                };
                self.queued.push_back(item);
                let change = QueueChange::Enqueued { id, kind, message };
                Ok(vec![
                    record_effect(
                        &at,
                        RecordBody::QueueChanged {
                            op,
                            change: change.clone(),
                        },
                    ),
                    Effect::Emit(AgentEvent::QueueChanged { change }),
                ])
            }
            SessionControl::Cancel { id } => {
                let Some(position) = self.queued.iter().position(|item| item.id == id) else {
                    return Err(MachineError::UnknownQueueItem(id));
                };
                self.queued.remove(position);
                let change = QueueChange::Cancelled { id };
                Ok(vec![
                    record_effect(
                        &at,
                        RecordBody::QueueChanged {
                            op,
                            change: change.clone(),
                        },
                    ),
                    Effect::Emit(AgentEvent::QueueChanged { change }),
                ])
            }
            SessionControl::Abort => {
                let op = op.ok_or(MachineError::IdleAbort)?;
                if self.abort_requested {
                    return Ok(Vec::new());
                }
                self.abort_requested = true;
                Ok(vec![
                    record_effect(&at, RecordBody::AbortRequested { op: op.clone() }),
                    Effect::Emit(AgentEvent::AbortRequested { op }),
                ])
            }
            SessionControl::AnswerInteraction { id, .. } => {
                let expected = match &self.phase {
                    Phase::AwaitingInteraction(interaction) => interaction.request.id.clone(),
                    _ => String::new(),
                };
                Err(MachineError::MismatchedInteraction {
                    expected,
                    actual: id,
                })
            }
        }
    }

    /// Removes the oldest queued item for a new operation after the current one ends.
    pub fn pop_queued_input(&mut self) -> Option<QueuedInput> {
        self.queued.pop_front()
    }

    /// Refreshes a pending provider action after the shell's pre-poll accepted steering.
    pub fn prepare_action(
        &mut self,
        action: Action,
        steer_stamps: Vec<EntryStamp>,
    ) -> Result<(Vec<Effect>, Action), MachineError> {
        let Action::StreamAssistant {
            mut request,
            model,
            origin,
        } = action
        else {
            if steer_stamps.is_empty() {
                return Ok((Vec::new(), action));
            }
            return Err(MachineError::SteerStampCount {
                expected: 0,
                actual: steer_stamps.len(),
            });
        };
        let op = self.active_op().cloned().ok_or(MachineError::IdleAbort)?;
        let mut effects = Vec::new();
        let provider_len = self.provider_messages.len();
        self.drain_steers(&op, steer_stamps, &mut effects)?;
        request
            .messages
            .extend_from_slice(&self.provider_messages[provider_len..]);
        Ok((
            effects,
            Action::StreamAssistant {
                request,
                model,
                origin,
            },
        ))
    }

    fn begin_run(
        self,
        op: OpId,
        step: u32,
        origin: Origin,
        at: Timestamp,
        effects: Vec<Effect>,
    ) -> Result<(Self, Step), MachineError> {
        if self.hook_enabled(HookPoint::RunStarted) {
            let payload = serde_json::json!({ "op": op, "origin": origin });
            self.start_hook(HookStart {
                op,
                hook: HookPoint::RunStarted,
                payload,
                origin,
                continuation: HookContinuation::RunStarted { step },
                at,
                effects,
            })
        } else {
            self.begin_request(op, step, origin, at, effects)
        }
    }

    fn begin_request(
        self,
        op: OpId,
        step: u32,
        origin: Origin,
        at: Timestamp,
        effects: Vec<Effect>,
    ) -> Result<(Self, Step), MachineError> {
        let request = self.request();
        if self.hook_enabled(HookPoint::TransformContext) {
            let payload =
                serde_json::to_value(&request).map_err(|_| MachineError::InvalidHookResult {
                    hook: HookPoint::TransformContext,
                })?;
            self.start_hook(HookStart {
                op,
                hook: HookPoint::TransformContext,
                payload,
                origin,
                continuation: HookContinuation::TransformContext { step },
                at,
                effects,
            })
        } else {
            self.begin_before_request(op, step, request, origin, at, effects)
        }
    }

    fn begin_before_request(
        self,
        op: OpId,
        step: u32,
        request: Request,
        origin: Origin,
        at: Timestamp,
        effects: Vec<Effect>,
    ) -> Result<(Self, Step), MachineError> {
        if self.hook_enabled(HookPoint::BeforeRequest) {
            let payload =
                serde_json::to_value(&request).map_err(|_| MachineError::InvalidHookResult {
                    hook: HookPoint::BeforeRequest,
                })?;
            self.start_hook(HookStart {
                op,
                hook: HookPoint::BeforeRequest,
                payload,
                origin,
                continuation: HookContinuation::BeforeRequest { step, request },
                at,
                effects,
            })
        } else {
            Ok(self.start_provider(op, step, request, origin, at, effects))
        }
    }

    fn start_provider(
        mut self,
        op: OpId,
        step: u32,
        request: Request,
        origin: Origin,
        at: Timestamp,
        mut effects: Vec<Effect>,
    ) -> (Self, Step) {
        effects.push(record_effect(
            &at,
            RecordBody::Step {
                op: op.clone(),
                n: step,
            },
        ));
        self.phase = Phase::AwaitingAssistant { op, step, origin };
        let model = self.config.model.clone();
        (
            self,
            Step::Do {
                effects,
                action: Some(Action::StreamAssistant {
                    request,
                    model,
                    origin,
                }),
            },
        )
    }

    fn begin_compaction(
        self,
        op: OpId,
        step: u32,
        work: CompactionWork,
        origin: Origin,
        at: Timestamp,
        effects: Vec<Effect>,
    ) -> Result<(Self, Step), MachineError> {
        let request = self.summary_request(&work);
        if self.hook_enabled(HookPoint::BeforeCompaction) {
            let payload =
                serde_json::to_value(&request).map_err(|_| MachineError::InvalidHookResult {
                    hook: HookPoint::BeforeCompaction,
                })?;
            self.start_hook(HookStart {
                op,
                hook: HookPoint::BeforeCompaction,
                payload,
                origin,
                continuation: HookContinuation::BeforeCompaction {
                    step,
                    work,
                    request,
                },
                at,
                effects,
            })
        } else {
            Ok(self.start_summary(SummaryStart {
                op,
                step,
                work,
                request,
                origin,
                at,
                effects,
            }))
        }
    }

    fn start_summary(mut self, start: SummaryStart) -> (Self, Step) {
        let SummaryStart {
            op,
            step,
            work,
            request,
            origin,
            at,
            mut effects,
        } = start;
        effects.push(record_effect(
            &at,
            RecordBody::Step {
                op: op.clone(),
                n: step,
            },
        ));
        self.phase = Phase::AwaitingSummary {
            op,
            step,
            work,
            origin,
        };
        let model = self.config.model.clone();
        (
            self,
            Step::Do {
                effects,
                action: Some(Action::Summarize {
                    request,
                    model,
                    origin,
                }),
            },
        )
    }

    fn begin_tool(
        self,
        pending: AwaitingTool,
        at: Timestamp,
        effects: Vec<Effect>,
    ) -> Result<(Self, Step), MachineError> {
        if pending.current.journal_start && self.hook_enabled(HookPoint::BeforeTool) {
            let payload = serde_json::to_value(&pending.current.call).map_err(|_| {
                MachineError::InvalidHookResult {
                    hook: HookPoint::BeforeTool,
                }
            })?;
            let op = pending.op.clone();
            let origin = pending.origin;
            self.start_hook(HookStart {
                op,
                hook: HookPoint::BeforeTool,
                payload,
                origin,
                continuation: HookContinuation::BeforeTool { pending },
                at,
                effects,
            })
        } else {
            self.start_hooked_tool(pending.clone(), pending.current.call, at, effects)
        }
    }

    fn start_hooked_tool(
        mut self,
        mut pending: AwaitingTool,
        call: PreparedToolCall,
        at: Timestamp,
        mut effects: Vec<Effect>,
    ) -> Result<(Self, Step), MachineError> {
        if !self.hooked_call_is_valid(&pending.current.call, &call) {
            return Err(MachineError::InvalidHookResult {
                hook: HookPoint::BeforeTool,
            });
        }
        pending.current.call = call.clone();
        if pending.current.journal_start {
            effects.extend(start_tool_effects(&pending.op, &call, &at));
        }
        let action = Action::ExecuteTool {
            call,
            origin: pending.origin,
        };
        self.phase = Phase::AwaitingTool(pending);
        Ok((
            self,
            Step::Do {
                effects,
                action: Some(action),
            },
        ))
    }

    fn hooked_call_is_valid(&self, original: &PreparedToolCall, call: &PreparedToolCall) -> bool {
        if call.call_id != original.call_id
            || call.name != original.name
            || call.replay != original.replay
            || call.precomputed_error != original.precomputed_error
        {
            return false;
        }
        if call.precomputed_error.is_some() {
            return true;
        }
        self.config
            .tools
            .iter()
            .find(|spec| spec.definition.name == call.name)
            .is_some_and(|spec| {
                rho_ai::validate_tool_arguments(&spec.definition, &call.effective_args).is_ok()
            })
    }

    fn begin_finish(
        self,
        op: OpId,
        outcome: OpOutcome,
        origin: Origin,
        at: Timestamp,
        effects: Vec<Effect>,
    ) -> Result<(Self, Step), MachineError> {
        if self.hook_enabled(HookPoint::RunFinished) {
            let payload =
                serde_json::to_value(&outcome).map_err(|_| MachineError::InvalidHookResult {
                    hook: HookPoint::RunFinished,
                })?;
            self.start_hook(HookStart {
                op,
                hook: HookPoint::RunFinished,
                payload,
                origin,
                continuation: HookContinuation::RunFinished {
                    outcome,
                    at: at.clone(),
                },
                at,
                effects,
            })
        } else {
            Ok(self.finish_without_hook(op, outcome, at, effects))
        }
    }

    fn finish_without_hook(
        mut self,
        op: OpId,
        outcome: OpOutcome,
        at: Timestamp,
        mut effects: Vec<Effect>,
    ) -> (Self, Step) {
        effects.extend(finish_effects(&op, outcome, &at));
        self.phase = Phase::Idle;
        (
            self,
            Step::Do {
                effects,
                action: None,
            },
        )
    }

    fn invalid_hook_result(
        self,
        op: OpId,
        hook: HookPoint,
        origin: Origin,
        at: Timestamp,
        effects: Vec<Effect>,
    ) -> Result<(Self, Step), MachineError> {
        let outcome = OpOutcome::Failed {
            error: format!("hook {hook:?} returned an invalid result"),
        };
        if hook != HookPoint::RunFinished {
            self.begin_finish(op, outcome, origin, at, effects)
        } else {
            Ok(self.finish_without_hook(op, outcome, at, effects))
        }
    }

    fn hook_enabled(&self, hook: HookPoint) -> bool {
        self.config.hooks.contains(&hook)
    }

    fn start_hook(mut self, start: HookStart) -> Result<(Self, Step), MachineError> {
        let HookStart {
            op,
            hook,
            payload,
            origin,
            continuation,
            at,
            mut effects,
        } = start;
        self.hook_n = self
            .hook_n
            .checked_add(1)
            .ok_or(MachineError::HookSequenceExhausted)?;
        let n = self.hook_n;
        let invocation = HookInvocation { hook, payload };
        effects.push(record_effect(
            &at,
            RecordBody::HookStarted {
                op: op.clone(),
                n,
                invocation: invocation.clone(),
            },
        ));
        effects.push(Effect::Emit(AgentEvent::HookStarted {
            op: op.clone(),
            n,
            invocation: invocation.clone(),
        }));
        self.phase = Phase::AwaitingHook(AwaitingHook {
            op,
            n,
            invocation: invocation.clone(),
            origin,
            continuation,
        });
        Ok((
            self,
            Step::Do {
                effects,
                action: Some(Action::InvokeHook { invocation, origin }),
            },
        ))
    }

    fn active_op(&self) -> Option<&OpId> {
        match &self.phase {
            Phase::Idle => None,
            Phase::AwaitingAssistant { op, .. }
            | Phase::AwaitingSummary { op, .. }
            | Phase::AwaitingTool(AwaitingTool { op, .. }) => Some(op),
            Phase::AwaitingHook(hook) => Some(&hook.op),
            Phase::AwaitingInteraction(interaction) => Some(&interaction.hook.op),
        }
    }

    /// Handles an external or recovery command.
    pub fn handle(mut self, input: Input) -> Result<(Self, Step), MachineError> {
        match input {
            Input::Prompt {
                message,
                op,
                stamp,
                origin,
                host,
                queue,
                steer_stamps,
            } => {
                if self.phase != Phase::Idle {
                    return Err(MachineError::Busy);
                }
                if !matches!(message, SessionMessage::User { .. }) {
                    return Err(MachineError::InvalidPrompt);
                }
                let entry = NewEntry {
                    id: stamp.id,
                    parent: self.leaf.clone(),
                    lane: LaneName::main(),
                    op: Some(op.clone()),
                    source_queue: queue,
                    at: stamp.at.clone(),
                    body: EntryBody::Message {
                        message: message.clone(),
                    },
                };
                self.remember_entry(&entry);
                self.abort_requested = false;
                self.hook_n = 0;
                let mut effects = vec![
                    Effect::AppendEntry(entry),
                    record_effect(
                        &stamp.at,
                        RecordBody::OpStarted {
                            op: op.clone(),
                            intent: OpIntent::Run,
                            origin,
                            host,
                        },
                    ),
                    Effect::Emit(AgentEvent::OperationStarted {
                        op: op.clone(),
                        origin,
                    }),
                ];
                self.drain_steers(&op, steer_stamps, &mut effects)?;
                self.begin_run(op, 1, origin, stamp.at, effects)
            }
            Input::Compact {
                op,
                at,
                origin,
                host,
            } => {
                if self.phase != Phase::Idle {
                    return Err(MachineError::Busy);
                }
                let policy = self
                    .config
                    .compaction
                    .as_ref()
                    .ok_or(MachineError::CompactionDisabled)?;
                let plan = plan_compaction(&self.entries, policy.retain_messages);
                if plan.compacted.is_empty() {
                    return Err(MachineError::NothingToCompact);
                }
                let work = CompactionWork {
                    compacted: plan.compacted,
                    retained_tail: plan.retained_tail,
                    first_kept: plan.first_kept,
                    tokens_before: self.last_input_tokens,
                };
                self.abort_requested = false;
                self.hook_n = 0;
                let effects = vec![
                    record_effect(
                        &at,
                        RecordBody::OpStarted {
                            op: op.clone(),
                            intent: OpIntent::Compaction,
                            origin,
                            host,
                        },
                    ),
                    record_effect(
                        &at,
                        RecordBody::CompactionStarted {
                            op: op.clone(),
                            work: work.clone(),
                        },
                    ),
                    Effect::Emit(AgentEvent::OperationStarted {
                        op: op.clone(),
                        origin,
                    }),
                    Effect::Emit(AgentEvent::CompactionStarted {
                        op: op.clone(),
                        tokens_before: work.tokens_before,
                    }),
                ];
                self.begin_compaction(op, 1, work, origin, at, effects)
            }
            Input::Resume {
                status,
                at,
                steer_stamps,
            } => {
                match status {
                    LaneStatus::Idle => Ok((self, Step::Idle)),
                    LaneStatus::Suspended(mut suspended) => {
                        let op = suspended.op.clone();
                        let mut effects = vec![Effect::Emit(AgentEvent::OperationStarted {
                            op: op.clone(),
                            origin: Origin::Replay,
                        })];
                        if !suspended.operation_started {
                            effects.push(record_effect(
                                &at,
                                RecordBody::OpStarted {
                                    op: op.clone(),
                                    intent: suspended.intent,
                                    origin: Origin::Replay,
                                    host: None,
                                },
                            ));
                        }

                        self.hook_n = suspended.last_hook;
                        if suspended.abort_requested
                            && let Some(hook) = suspended.hook.as_ref()
                            && hook.result.is_none()
                        {
                            if let Some(interaction) = hook
                                .interactions
                                .last()
                                .filter(|interaction| interaction.answer.is_none())
                            {
                                effects.extend([
                                    record_effect(
                                        &at,
                                        RecordBody::InteractionAnswered {
                                            op: op.clone(),
                                            hook: hook.n,
                                            request_id: interaction.request.id.clone(),
                                            answer: InteractionAnswer::TimedOut,
                                        },
                                    ),
                                    Effect::Emit(AgentEvent::InteractionAnswered {
                                        op: op.clone(),
                                        request_id: interaction.request.id.clone(),
                                        answer: InteractionAnswer::TimedOut,
                                    }),
                                ]);
                            }
                            let result = Err("interrupted after abort was requested".to_owned());
                            effects.extend([
                                record_effect(
                                    &at,
                                    RecordBody::HookFinished {
                                        op: op.clone(),
                                        n: hook.n,
                                        result: result.clone(),
                                    },
                                ),
                                Effect::Emit(AgentEvent::HookFinished {
                                    op: op.clone(),
                                    n: hook.n,
                                    result,
                                }),
                            ]);
                        }
                        if let Some(hook) = suspended.hook.clone()
                            && !suspended.abort_requested
                        {
                            return self.resume_hook(&suspended, *hook, at, effects, steer_stamps);
                        }

                        if suspended.intent == OpIntent::Compaction {
                            return self.resume_compaction(suspended, at, effects, steer_stamps);
                        }

                        if suspended.abort_requested {
                            let mut calls = suspended
                                .open_tools
                                .into_iter()
                                .map(|tool| PendingTool {
                                    call: PreparedToolCall {
                                        call_id: tool.call_id,
                                        name: tool.name,
                                        effective_args: tool.effective_args,
                                        replay: tool.replay,
                                        precomputed_error: Some(
                                            "interrupted after abort was requested".to_owned(),
                                        ),
                                    },
                                    journal_start: false,
                                })
                                .collect::<VecDeque<_>>();
                            let Some(current) = calls.pop_front() else {
                                if suspended.hook.as_ref().is_some_and(|hook| {
                                    hook.invocation.hook == HookPoint::RunFinished
                                }) {
                                    return Ok(self.finish_without_hook(
                                        op,
                                        OpOutcome::Aborted,
                                        at,
                                        effects,
                                    ));
                                }
                                return self.begin_finish(
                                    op,
                                    OpOutcome::Aborted,
                                    Origin::Replay,
                                    at,
                                    effects,
                                );
                            };
                            let action = Action::ExecuteTool {
                                call: current.call.clone(),
                                origin: Origin::Replay,
                            };
                            self.phase = Phase::AwaitingTool(AwaitingTool {
                                op: op.clone(),
                                step: suspended.last_step.unwrap_or(0),
                                current,
                                remaining: calls,
                                after: AfterTools::Finish(OpOutcome::Aborted),
                                origin: Origin::Replay,
                            });
                            return Ok((
                                self,
                                Step::Do {
                                    effects,
                                    action: Some(action),
                                },
                            ));
                        }

                        if !suspended.stream_in_flight
                            && let Some(message) = suspended.last_assistant.take()
                        {
                            if !suspended.last_assistant_usage_recorded {
                                effects.push(record_effect(
                                    &at,
                                    RecordBody::Usage {
                                        op: op.clone(),
                                        usage: message.usage.clone(),
                                    },
                                ));
                            }
                            return self.resume_after_assistant(
                                suspended,
                                message,
                                at,
                                effects,
                                steer_stamps,
                            );
                        }

                        let next = suspended.last_step.unwrap_or(0) + 1;
                        self.drain_steers(&op, steer_stamps, &mut effects)?;
                        if suspended.last_step.is_none() {
                            return self.begin_run(op, next, Origin::Replay, at, effects);
                        }
                        self.phase = Phase::AwaitingAssistant {
                            op: op.clone(),
                            step: next,
                            origin: Origin::Replay,
                        };
                        effects.push(record_effect(
                            &at,
                            RecordBody::Step {
                                op: op.clone(),
                                n: next,
                            },
                        ));
                        let action = Action::StreamAssistant {
                            request: self.request(),
                            model: self.config.model.clone(),
                            origin: Origin::Replay,
                        };
                        Ok((
                            self,
                            Step::Do {
                                effects,
                                action: Some(action),
                            },
                        ))
                    }
                    LaneStatus::Corrupt(_) => Err(MachineError::CorruptResume),
                }
            }
        }
    }

    fn resume_hook(
        mut self,
        suspended: &crate::SuspendedOp,
        hook: crate::SuspendedHook,
        at: Timestamp,
        mut effects: Vec<Effect>,
        steer_stamps: Vec<EntryStamp>,
    ) -> Result<(Self, Step), MachineError> {
        let op = suspended.op.clone();
        let origin = Origin::Replay;
        let step = suspended.last_step.unwrap_or(0) + 1;
        let continuation = match hook.invocation.hook {
            HookPoint::RunStarted => HookContinuation::RunStarted { step },
            HookPoint::TransformContext => HookContinuation::TransformContext { step },
            HookPoint::BeforeRequest => HookContinuation::BeforeRequest {
                step,
                request: serde_json::from_value(hook.invocation.payload.clone())
                    .map_err(|_| MachineError::CorruptResume)?,
            },
            HookPoint::AfterRequest => HookContinuation::AfterRequest {
                step: suspended.last_step.ok_or(MachineError::CorruptResume)?,
                message: suspended
                    .last_assistant
                    .clone()
                    .ok_or(MachineError::CorruptResume)?,
                at: at.clone(),
                steer_stamps,
            },
            HookPoint::BeforeTool => {
                let current: PreparedToolCall =
                    serde_json::from_value(hook.invocation.payload.clone())
                        .map_err(|_| MachineError::CorruptResume)?;
                let message = suspended
                    .last_assistant
                    .as_ref()
                    .ok_or(MachineError::CorruptResume)?;
                let remaining = self
                    .prepare_calls(message, message.stop == StopReason::Length)
                    .into_iter()
                    .filter(|call| {
                        call.call_id != current.call_id
                            && !suspended.resolved_tool_calls.contains(&call.call_id)
                    })
                    .map(|call| PendingTool {
                        call,
                        journal_start: true,
                    })
                    .collect();
                HookContinuation::BeforeTool {
                    pending: AwaitingTool {
                        op: op.clone(),
                        step: suspended.last_step.unwrap_or(0),
                        current: PendingTool {
                            call: current,
                            journal_start: true,
                        },
                        remaining,
                        after: AfterTools::Stream,
                        origin,
                    },
                }
            }
            HookPoint::AfterTool => {
                let SessionMessage::ToolResult { call_id, .. } =
                    serde_json::from_value(hook.invocation.payload.clone())
                        .map_err(|_| MachineError::CorruptResume)?
                else {
                    return Err(MachineError::CorruptResume);
                };
                let message = suspended
                    .last_assistant
                    .as_ref()
                    .ok_or(MachineError::CorruptResume)?;
                let prepared = self.prepare_calls(message, message.stop == StopReason::Length);
                let current = prepared
                    .iter()
                    .find(|call| call.call_id == call_id)
                    .cloned()
                    .ok_or(MachineError::CorruptResume)?;
                let remaining = prepared
                    .into_iter()
                    .filter(|call| !suspended.resolved_tool_calls.contains(&call.call_id))
                    .map(|call| PendingTool {
                        call,
                        journal_start: true,
                    })
                    .collect();
                HookContinuation::AfterTool {
                    pending: AwaitingTool {
                        op: op.clone(),
                        step: suspended.last_step.unwrap_or(0),
                        current: PendingTool {
                            call: current,
                            journal_start: false,
                        },
                        remaining,
                        after: AfterTools::Stream,
                        origin,
                    },
                    at: at.clone(),
                    steer_stamps,
                }
            }
            HookPoint::BeforeCompaction => {
                let work = suspended
                    .compaction
                    .as_ref()
                    .map(|compaction| compaction.work.clone())
                    .ok_or(MachineError::CorruptResume)?;
                HookContinuation::BeforeCompaction {
                    step,
                    work,
                    request: serde_json::from_value(hook.invocation.payload.clone())
                        .map_err(|_| MachineError::CorruptResume)?,
                }
            }
            HookPoint::RunFinished => HookContinuation::RunFinished {
                outcome: serde_json::from_value(hook.invocation.payload.clone())
                    .map_err(|_| MachineError::CorruptResume)?,
                at: at.clone(),
            },
        };
        let mut pending = AwaitingHook {
            op: op.clone(),
            n: hook.n,
            invocation: hook.invocation,
            origin,
            continuation,
        };
        if let Some(result) = hook.result {
            return self.resolve_hook(pending, result, at, false, effects);
        }
        for interaction in hook.interactions {
            let request = interaction.request;
            let unanswered = interaction.answer.is_none();
            let answer = interaction.answer.unwrap_or(InteractionAnswer::TimedOut);
            pending.invocation.payload = serde_json::json!({
                "input": pending.invocation.payload,
                "interaction": {
                    "request": request.clone(),
                    "answer": answer.clone(),
                },
            });
            if unanswered {
                effects.extend([
                    record_effect(
                        &at,
                        RecordBody::InteractionAnswered {
                            op: op.clone(),
                            hook: hook.n,
                            request_id: request.id.clone(),
                            answer: InteractionAnswer::TimedOut,
                        },
                    ),
                    Effect::Emit(AgentEvent::InteractionAnswered {
                        op: op.clone(),
                        request_id: request.id,
                        answer: InteractionAnswer::TimedOut,
                    }),
                ]);
            }
        }
        let action = Action::InvokeHook {
            invocation: pending.invocation.clone(),
            origin,
        };
        self.phase = Phase::AwaitingHook(pending);
        Ok((
            self,
            Step::Do {
                effects,
                action: Some(action),
            },
        ))
    }

    fn resume_compaction(
        mut self,
        suspended: crate::SuspendedOp,
        at: Timestamp,
        mut effects: Vec<Effect>,
        _steer_stamps: Vec<EntryStamp>,
    ) -> Result<(Self, Step), MachineError> {
        let op = suspended.op.clone();
        if suspended.abort_requested {
            return Ok(self.finish_without_hook(op, OpOutcome::Aborted, at, effects));
        }
        let compaction = if let Some(compaction) = suspended.compaction {
            compaction
        } else {
            let policy = self
                .config
                .compaction
                .as_ref()
                .ok_or(MachineError::CompactionDisabled)?;
            let plan = plan_compaction(&self.entries, policy.retain_messages);
            if plan.compacted.is_empty() {
                return Err(MachineError::CorruptResume);
            }
            let work = CompactionWork {
                compacted: plan.compacted,
                retained_tail: plan.retained_tail,
                first_kept: plan.first_kept,
                tokens_before: self.last_input_tokens,
            };
            effects.push(record_effect(
                &at,
                RecordBody::CompactionStarted {
                    op: op.clone(),
                    work: work.clone(),
                },
            ));
            Box::new(crate::SuspendedCompaction {
                work,
                completed: None,
                usage_recorded: false,
            })
        };
        if let Some(completed) = compaction.completed {
            if !compaction.usage_recorded {
                effects.push(record_effect(
                    &at,
                    RecordBody::Usage {
                        op: op.clone(),
                        usage: completed.usage,
                    },
                ));
            }
            effects.push(Effect::Emit(AgentEvent::CompactionFinished {
                op: op.clone(),
                summary: completed.summary,
            }));
            self.last_input_tokens = 0;
            return Ok(self.finish_without_hook(op, OpOutcome::Completed, at, effects));
        }

        if suspended.last_step.is_none() {
            return self.begin_compaction(op, 1, compaction.work, Origin::Replay, at, effects);
        }

        let next = suspended.last_step.unwrap_or(0) + 1;
        effects.push(record_effect(
            &at,
            RecordBody::Step {
                op: op.clone(),
                n: next,
            },
        ));
        self.phase = Phase::AwaitingSummary {
            op: op.clone(),
            step: next,
            work: compaction.work.clone(),
            origin: Origin::Replay,
        };
        let action = Action::Summarize {
            request: self.summary_request(&compaction.work),
            model: self.config.model.clone(),
            origin: Origin::Replay,
        };
        Ok((
            self,
            Step::Do {
                effects,
                action: Some(action),
            },
        ))
    }

    fn resume_after_assistant(
        self,
        suspended: crate::SuspendedOp,
        message: AssistantMessage,
        at: Timestamp,
        effects: Vec<Effect>,
        steer_stamps: Vec<EntryStamp>,
    ) -> Result<(Self, Step), MachineError> {
        let op = suspended.op;
        let step = suspended.last_step.unwrap_or(0);
        let open_tools = suspended.open_tools;
        let resolved_tool_calls = suspended.resolved_tool_calls;
        if open_tools.is_empty()
            && resolved_tool_calls.is_empty()
            && self.hook_enabled(HookPoint::AfterRequest)
        {
            let payload =
                serde_json::to_value(&message).map_err(|_| MachineError::InvalidHookResult {
                    hook: HookPoint::AfterRequest,
                })?;
            return self.start_hook(HookStart {
                op,
                hook: HookPoint::AfterRequest,
                payload,
                origin: Origin::Replay,
                continuation: HookContinuation::AfterRequest {
                    step,
                    message,
                    at: at.clone(),
                    steer_stamps,
                },
                at,
                effects,
            });
        }
        if open_tools.is_empty()
            && self.hook_enabled(HookPoint::AfterTool)
            && let Some(call_id) = resolved_tool_calls.last()
        {
            let result = self
                .entries
                .iter()
                .rev()
                .find_map(|entry| match &entry.body {
                    EntryBody::Message {
                        message:
                            result @ SessionMessage::ToolResult {
                                call_id: stored, ..
                            },
                    } if entry.op.as_ref() == Some(&op) && stored == call_id => {
                        Some(result.clone())
                    }
                    _ => None,
                })
                .ok_or(MachineError::CorruptResume)?;
            let prepared = self.prepare_calls(&message, message.stop == StopReason::Length);
            let current = prepared
                .iter()
                .find(|call| call.call_id == *call_id)
                .cloned()
                .ok_or(MachineError::CorruptResume)?;
            let remaining = prepared
                .into_iter()
                .filter(|call| !resolved_tool_calls.contains(&call.call_id))
                .map(|call| PendingTool {
                    call,
                    journal_start: true,
                })
                .collect();
            let payload =
                serde_json::to_value(&result).map_err(|_| MachineError::InvalidHookResult {
                    hook: HookPoint::AfterTool,
                })?;
            return self.start_hook(HookStart {
                op: op.clone(),
                hook: HookPoint::AfterTool,
                payload,
                origin: Origin::Replay,
                continuation: HookContinuation::AfterTool {
                    pending: AwaitingTool {
                        op,
                        step,
                        current: PendingTool {
                            call: current,
                            journal_start: false,
                        },
                        remaining,
                        after: AfterTools::Stream,
                        origin: Origin::Replay,
                    },
                    at: at.clone(),
                    steer_stamps,
                },
                at,
                effects,
            });
        }
        self.continue_after_assistant(RecoveryAssistantBoundary {
            boundary: AssistantBoundary {
                op,
                step,
                origin: Origin::Replay,
                message,
                at,
                steer_stamps,
                effects,
            },
            open_tools,
            resolved_tool_calls,
        })
    }

    fn continue_after_assistant(
        mut self,
        recovery: RecoveryAssistantBoundary,
    ) -> Result<(Self, Step), MachineError> {
        let RecoveryAssistantBoundary {
            boundary:
                AssistantBoundary {
                    op,
                    step,
                    origin,
                    message,
                    at,
                    steer_stamps,
                    mut effects,
                },
            open_tools,
            resolved_tool_calls,
        } = recovery;
        match message.stop {
            StopReason::Stop => {
                if self.drain_steers(&op, steer_stamps, &mut effects)? {
                    let next = step + 1;
                    return self.begin_request(op, next, origin, at, effects);
                }
                self.begin_finish(op, OpOutcome::Completed, origin, at, effects)
            }
            StopReason::ToolUse | StopReason::Length => {
                let prepared = self.prepare_calls(&message, message.stop == StopReason::Length);
                if prepared.is_empty() {
                    let error = if message.stop == StopReason::Length {
                        "provider output was truncated before completing the turn"
                    } else {
                        "provider stopped for tool use without returning a tool call"
                    };
                    return self.begin_finish(
                        op,
                        OpOutcome::Failed {
                            error: error.to_owned(),
                        },
                        origin,
                        at,
                        effects,
                    );
                }

                let mut calls = prepared
                    .into_iter()
                    .filter(|call| !resolved_tool_calls.contains(&call.call_id))
                    .map(|call| {
                        if let Some(open) =
                            open_tools.iter().find(|open| open.call_id == call.call_id)
                        {
                            PendingTool {
                                call: PreparedToolCall {
                                    call_id: open.call_id.clone(),
                                    name: open.name.clone(),
                                    effective_args: open.effective_args.clone(),
                                    replay: open.replay,
                                    precomputed_error: (open.replay == ReplaySafety::Never).then(
                                        || "interrupted; tool is not safe to re-run".to_owned(),
                                    ),
                                },
                                journal_start: false,
                            }
                        } else {
                            PendingTool {
                                call,
                                journal_start: true,
                            }
                        }
                    })
                    .collect::<VecDeque<_>>();
                let Some(current) = calls.pop_front() else {
                    let next = step + 1;
                    return self.begin_request(op, next, origin, at, effects);
                };
                self.begin_tool(
                    AwaitingTool {
                        op,
                        step,
                        current,
                        remaining: calls,
                        after: AfterTools::Stream,
                        origin,
                    },
                    at,
                    effects,
                )
            }
            StopReason::Paused => {
                self.drain_steers(&op, steer_stamps, &mut effects)?;
                let next = step + 1;
                self.begin_request(op, next, origin, at, effects)
            }
            StopReason::Aborted => self.begin_finish(op, OpOutcome::Aborted, origin, at, effects),
            StopReason::Refusal | StopReason::Error => self.begin_finish(
                op,
                OpOutcome::Failed {
                    error: format!("provider ended the generation with {:?}", message.stop),
                },
                origin,
                at,
                effects,
            ),
            _ => self.begin_finish(
                op,
                OpOutcome::Failed {
                    error: "provider returned an unsupported stop reason".to_owned(),
                },
                origin,
                at,
                effects,
            ),
        }
    }

    /// Resolves the single action currently in flight.
    pub fn resolve(self, outcome: ActionOutcome) -> Result<(Self, Step), MachineError> {
        match (self.phase.clone(), outcome) {
            (
                Phase::AwaitingAssistant { op, step, origin },
                ActionOutcome::Assistant {
                    result,
                    stamp,
                    steer_stamps,
                },
            ) => self.resolve_assistant(op, step, origin, result, stamp, steer_stamps),
            (
                Phase::AwaitingTool(pending),
                ActionOutcome::Tool {
                    call_id,
                    content,
                    is_error,
                    details,
                    stamp,
                    steer_stamps,
                },
            ) => {
                if call_id != pending.current.call.call_id {
                    return Err(MachineError::MismatchedToolCall {
                        expected: pending.current.call.call_id,
                        actual: call_id,
                    });
                }
                self.resolve_tool(pending, content, is_error, details, stamp, steer_stamps)
            }
            (
                Phase::AwaitingSummary {
                    op,
                    step: _,
                    work,
                    origin,
                },
                ActionOutcome::Summary { result, stamp },
            ) => self.resolve_summary(op, work, origin, result, stamp),
            (Phase::AwaitingHook(hook), ActionOutcome::Hook { result, at }) => {
                self.resolve_hook(hook, result, at, true, Vec::new())
            }
            (
                Phase::AwaitingInteraction(interaction),
                ActionOutcome::Interaction {
                    request_id,
                    answer,
                    at,
                },
            ) => self.resolve_interaction(interaction, request_id, answer, at),
            (Phase::Idle, _) => Err(MachineError::UnexpectedOutcome),
            (
                Phase::AwaitingAssistant { .. }
                | Phase::AwaitingTool(_)
                | Phase::AwaitingSummary { .. }
                | Phase::AwaitingHook(_)
                | Phase::AwaitingInteraction(_),
                _,
            ) => Err(MachineError::MismatchedOutcome),
        }
    }

    fn resolve_hook(
        mut self,
        pending: AwaitingHook,
        result: Result<HookOutput, String>,
        at: Timestamp,
        journal_result: bool,
        mut prior_effects: Vec<Effect>,
    ) -> Result<(Self, Step), MachineError> {
        let AwaitingHook {
            op,
            n,
            invocation,
            origin,
            continuation,
        } = pending;
        let result = match result {
            Ok(HookOutput::Interact { request }) if request.id.is_empty() => {
                Err("interaction request id must not be empty".to_owned())
            }
            result => result,
        };
        if let Ok(HookOutput::Interact { request }) = result {
            prior_effects.extend([
                record_effect(
                    &at,
                    RecordBody::InteractionRequested {
                        op: op.clone(),
                        hook: n,
                        request: request.clone(),
                    },
                ),
                Effect::Emit(AgentEvent::InteractionRequested {
                    op: op.clone(),
                    request: request.clone(),
                }),
            ]);
            self.phase = Phase::AwaitingInteraction(AwaitingInteraction {
                hook: AwaitingHook {
                    op,
                    n,
                    invocation,
                    origin,
                    continuation,
                },
                request: request.clone(),
            });
            return Ok((
                self,
                Step::Do {
                    effects: prior_effects,
                    action: Some(Action::AwaitInteraction { request, origin }),
                },
            ));
        }

        if journal_result {
            prior_effects.extend([
                record_effect(
                    &at,
                    RecordBody::HookFinished {
                        op: op.clone(),
                        n,
                        result: result.clone(),
                    },
                ),
                Effect::Emit(AgentEvent::HookFinished {
                    op: op.clone(),
                    n,
                    result: result.clone(),
                }),
            ]);
        }
        let mut effects = prior_effects;
        if self.abort_requested {
            if invocation.hook != HookPoint::RunFinished {
                return self.begin_finish(op, OpOutcome::Aborted, origin, at, effects);
            }
            effects.extend(finish_effects(&op, OpOutcome::Aborted, &at));
            self.phase = Phase::Idle;
            self.abort_requested = false;
            return Ok((
                self,
                Step::Do {
                    effects,
                    action: None,
                },
            ));
        }
        let value = match result {
            Ok(HookOutput::Continue { value }) => value,
            Ok(HookOutput::Interact { .. }) => unreachable!("handled above"),
            Err(error) => {
                let outcome = OpOutcome::Failed {
                    error: format!("hook {:?} failed: {error}", invocation.hook),
                };
                if invocation.hook != HookPoint::RunFinished {
                    return self.begin_finish(op, outcome, origin, at, effects);
                }
                effects.extend(finish_effects(&op, outcome, &at));
                self.phase = Phase::Idle;
                return Ok((
                    self,
                    Step::Do {
                        effects,
                        action: None,
                    },
                ));
            }
        };

        match continuation {
            HookContinuation::RunStarted { step } => {
                self.begin_request(op, step, origin, at, effects)
            }
            HookContinuation::TransformContext { step } => {
                let Ok(request) = serde_json::from_value(value) else {
                    return self.invalid_hook_result(
                        op,
                        HookPoint::TransformContext,
                        origin,
                        at,
                        effects,
                    );
                };
                self.begin_before_request(op, step, request, origin, at, effects)
            }
            HookContinuation::BeforeRequest { step, request: _ } => {
                let Ok(request) = serde_json::from_value(value) else {
                    return self.invalid_hook_result(
                        op,
                        HookPoint::BeforeRequest,
                        origin,
                        at,
                        effects,
                    );
                };
                Ok(self.start_provider(op, step, request, origin, at, effects))
            }
            HookContinuation::AfterRequest {
                step,
                message,
                at,
                steer_stamps,
            } => self.continue_live_after_assistant(AssistantBoundary {
                op,
                step,
                origin,
                message,
                at,
                steer_stamps,
                effects,
            }),
            HookContinuation::BeforeTool { pending } => {
                let Ok(call) = serde_json::from_value(value) else {
                    return self.invalid_hook_result(
                        op,
                        HookPoint::BeforeTool,
                        origin,
                        at,
                        effects,
                    );
                };
                if !self.hooked_call_is_valid(&pending.current.call, &call) {
                    return self.invalid_hook_result(
                        op,
                        HookPoint::BeforeTool,
                        origin,
                        at,
                        effects,
                    );
                }
                self.start_hooked_tool(pending, call, at, effects)
            }
            HookContinuation::AfterTool {
                pending,
                at,
                steer_stamps,
            } => self.continue_after_tool(pending, at, steer_stamps, effects),
            HookContinuation::BeforeCompaction {
                step,
                work,
                request: _,
            } => {
                let Ok(request) = serde_json::from_value(value) else {
                    return self.invalid_hook_result(
                        op,
                        HookPoint::BeforeCompaction,
                        origin,
                        at,
                        effects,
                    );
                };
                Ok(self.start_summary(SummaryStart {
                    op,
                    step,
                    work,
                    request,
                    origin,
                    at,
                    effects,
                }))
            }
            HookContinuation::RunFinished { outcome, at } => {
                effects.extend(finish_effects(&op, outcome, &at));
                self.phase = Phase::Idle;
                Ok((
                    self,
                    Step::Do {
                        effects,
                        action: None,
                    },
                ))
            }
        }
    }

    fn resolve_interaction(
        mut self,
        pending: AwaitingInteraction,
        request_id: String,
        answer: InteractionAnswer,
        at: Timestamp,
    ) -> Result<(Self, Step), MachineError> {
        if pending.request.id != request_id {
            return Err(MachineError::MismatchedInteraction {
                expected: pending.request.id,
                actual: request_id,
            });
        }
        let mut hook = pending.hook;
        let payload = serde_json::json!({
            "input": hook.invocation.payload,
            "interaction": {
                "request": pending.request,
                "answer": answer,
            },
        });
        hook.invocation.payload = payload;
        let action = Action::InvokeHook {
            invocation: hook.invocation.clone(),
            origin: hook.origin,
        };
        let effects = vec![
            record_effect(
                &at,
                RecordBody::InteractionAnswered {
                    op: hook.op.clone(),
                    hook: hook.n,
                    request_id: request_id.clone(),
                    answer: answer.clone(),
                },
            ),
            Effect::Emit(AgentEvent::InteractionAnswered {
                op: hook.op.clone(),
                request_id,
                answer,
            }),
        ];
        self.phase = Phase::AwaitingHook(hook);
        Ok((
            self,
            Step::Do {
                effects,
                action: Some(action),
            },
        ))
    }

    fn resolve_assistant(
        mut self,
        op: OpId,
        step: u32,
        origin: Origin,
        result: Result<AssistantMessage, ProviderError>,
        stamp: EntryStamp,
        steer_stamps: Vec<EntryStamp>,
    ) -> Result<(Self, Step), MachineError> {
        let message = match result {
            Ok(message) => message,
            Err(error) => {
                let outcome = if error.kind == ErrorKind::Cancelled {
                    OpOutcome::Aborted
                } else {
                    OpOutcome::Failed {
                        error: error.to_string(),
                    }
                };
                return self.begin_finish(op, outcome, origin, stamp.at, Vec::new());
            }
        };
        self.last_input_tokens = message.usage.input_tokens;
        let stored = SessionMessage::Assistant(message.clone());
        let entry = NewEntry {
            id: stamp.id,
            parent: self.leaf.clone(),
            lane: LaneName::main(),
            op: Some(op.clone()),
            source_queue: None,
            at: stamp.at.clone(),
            body: EntryBody::Message {
                message: stored.clone(),
            },
        };
        self.remember_entry(&entry);
        let effects = vec![
            Effect::AppendEntry(entry),
            record_effect(
                &stamp.at,
                RecordBody::Usage {
                    op: op.clone(),
                    usage: message.usage.clone(),
                },
            ),
            Effect::Emit(AgentEvent::MessageAppended {
                op: op.clone(),
                message: stored,
            }),
        ];

        if self.abort_requested {
            return self.begin_finish(op, OpOutcome::Aborted, origin, stamp.at, effects);
        }

        if self.hook_enabled(HookPoint::AfterRequest) {
            let payload =
                serde_json::to_value(&message).map_err(|_| MachineError::InvalidHookResult {
                    hook: HookPoint::AfterRequest,
                })?;
            return self.start_hook(HookStart {
                op,
                hook: HookPoint::AfterRequest,
                payload,
                origin,
                continuation: HookContinuation::AfterRequest {
                    step,
                    message,
                    at: stamp.at.clone(),
                    steer_stamps,
                },
                at: stamp.at,
                effects,
            });
        }
        self.continue_live_after_assistant(AssistantBoundary {
            op,
            step,
            origin,
            message,
            at: stamp.at,
            steer_stamps,
            effects,
        })
    }

    fn continue_live_after_assistant(
        mut self,
        boundary: AssistantBoundary,
    ) -> Result<(Self, Step), MachineError> {
        let AssistantBoundary {
            op,
            step,
            origin,
            message,
            at,
            steer_stamps,
            mut effects,
        } = boundary;
        match message.stop {
            StopReason::Stop => {
                if self.drain_steers(&op, steer_stamps, &mut effects)? {
                    let next = step + 1;
                    return self.begin_request(op, next, origin, at, effects);
                }
                self.begin_finish(op, OpOutcome::Completed, origin, at, effects)
            }
            StopReason::ToolUse | StopReason::Length => {
                let mut calls = self
                    .prepare_calls(&message, message.stop == StopReason::Length)
                    .into_iter()
                    .map(|call| PendingTool {
                        call,
                        journal_start: true,
                    })
                    .collect::<VecDeque<_>>();
                let Some(current) = calls.pop_front() else {
                    let error = if message.stop == StopReason::Length {
                        "provider output was truncated before completing the turn"
                    } else {
                        "provider stopped for tool use without returning a tool call"
                    };
                    return self.begin_finish(
                        op,
                        OpOutcome::Failed {
                            error: error.to_owned(),
                        },
                        origin,
                        at,
                        effects,
                    );
                };
                self.begin_tool(
                    AwaitingTool {
                        op,
                        step,
                        current,
                        remaining: calls,
                        after: AfterTools::Stream,
                        origin,
                    },
                    at,
                    effects,
                )
            }
            StopReason::Paused => {
                self.drain_steers(&op, steer_stamps, &mut effects)?;
                let next = step + 1;
                self.begin_request(op, next, origin, at, effects)
            }
            StopReason::Aborted => self.begin_finish(op, OpOutcome::Aborted, origin, at, effects),
            StopReason::Refusal | StopReason::Error => {
                let error = format!("provider ended the generation with {:?}", message.stop);
                self.begin_finish(op, OpOutcome::Failed { error }, origin, at, effects)
            }
            _ => self.begin_finish(
                op,
                OpOutcome::Failed {
                    error: "provider returned an unsupported stop reason".to_owned(),
                },
                origin,
                at,
                effects,
            ),
        }
    }

    fn resolve_tool(
        mut self,
        pending: AwaitingTool,
        content: Vec<ContentBlock>,
        is_error: bool,
        details: Option<Value>,
        stamp: EntryStamp,
        steer_stamps: Vec<EntryStamp>,
    ) -> Result<(Self, Step), MachineError> {
        let AwaitingTool {
            op,
            step,
            current,
            remaining,
            after,
            origin,
        } = pending;
        let message = SessionMessage::ToolResult {
            call_id: current.call.call_id.clone(),
            content,
            is_error,
            details,
        };
        let entry = NewEntry {
            id: stamp.id,
            parent: self.leaf.clone(),
            lane: LaneName::main(),
            op: Some(op.clone()),
            source_queue: None,
            at: stamp.at.clone(),
            body: EntryBody::Message {
                message: message.clone(),
            },
        };
        self.remember_entry(&entry);
        let effects = vec![
            Effect::AppendEntry(entry),
            Effect::Emit(AgentEvent::MessageAppended {
                op: op.clone(),
                message: message.clone(),
            }),
        ];

        if self.abort_requested {
            return self.begin_finish(op, OpOutcome::Aborted, origin, stamp.at, effects);
        }

        let pending = AwaitingTool {
            op: op.clone(),
            step,
            current,
            remaining,
            after,
            origin,
        };
        if self.hook_enabled(HookPoint::AfterTool) {
            let payload =
                serde_json::to_value(&message).map_err(|_| MachineError::InvalidHookResult {
                    hook: HookPoint::AfterTool,
                })?;
            return self.start_hook(HookStart {
                op,
                hook: HookPoint::AfterTool,
                payload,
                origin,
                continuation: HookContinuation::AfterTool {
                    pending,
                    at: stamp.at.clone(),
                    steer_stamps,
                },
                at: stamp.at,
                effects,
            });
        }
        self.continue_after_tool(pending, stamp.at, steer_stamps, effects)
    }

    fn continue_after_tool(
        mut self,
        pending: AwaitingTool,
        at: Timestamp,
        steer_stamps: Vec<EntryStamp>,
        mut effects: Vec<Effect>,
    ) -> Result<(Self, Step), MachineError> {
        let AwaitingTool {
            op,
            step,
            current: _,
            mut remaining,
            after,
            origin,
        } = pending;
        if let Some(next) = remaining.pop_front() {
            return self.begin_tool(
                AwaitingTool {
                    op,
                    step,
                    current: next,
                    remaining,
                    after,
                    origin,
                },
                at,
                effects,
            );
        }

        match after {
            AfterTools::Stream => {
                self.drain_steers(&op, steer_stamps, &mut effects)?;
                let next_step = step + 1;
                self.begin_request(op, next_step, origin, at, effects)
            }
            AfterTools::Finish(outcome) => self.begin_finish(op, outcome, origin, at, effects),
        }
    }

    fn resolve_summary(
        mut self,
        op: OpId,
        work: CompactionWork,
        _origin: Origin,
        result: Result<CompactionSummary, ProviderError>,
        stamp: EntryStamp,
    ) -> Result<(Self, Step), MachineError> {
        if self.abort_requested {
            return Ok(self.finish_without_hook(op, OpOutcome::Aborted, stamp.at, Vec::new()));
        }
        let summary = match result {
            Ok(summary) => summary,
            Err(error) => {
                let outcome = if error.kind == ErrorKind::Cancelled {
                    OpOutcome::Aborted
                } else {
                    OpOutcome::Failed {
                        error: error.to_string(),
                    }
                };
                return Ok(self.finish_without_hook(op, outcome, stamp.at, Vec::new()));
            }
        };
        let entry = NewEntry {
            id: stamp.id,
            parent: self.leaf.clone(),
            lane: LaneName::main(),
            op: Some(op.clone()),
            source_queue: None,
            at: stamp.at.clone(),
            body: EntryBody::Compaction {
                summary: summary.text.clone(),
                first_kept: work.first_kept,
                retained_tail: work.retained_tail,
                tokens_before: work.tokens_before,
                usage: summary.usage.clone(),
            },
        };
        self.remember_entry(&entry);
        self.last_input_tokens = 0;
        let effects = vec![
            Effect::AppendEntry(entry),
            record_effect(
                &stamp.at,
                RecordBody::Usage {
                    op: op.clone(),
                    usage: summary.usage,
                },
            ),
            Effect::Emit(AgentEvent::CompactionFinished {
                op: op.clone(),
                summary: summary.text,
            }),
        ];
        Ok(self.finish_without_hook(op, OpOutcome::Completed, stamp.at, effects))
    }

    fn prepare_calls(
        &self,
        message: &AssistantMessage,
        truncated: bool,
    ) -> VecDeque<PreparedToolCall> {
        message
            .blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolCall { id, name, args } => {
                    let spec = self
                        .config
                        .tools
                        .iter()
                        .find(|spec| spec.definition.name == *name);
                    let precomputed_error = if truncated {
                        Some(
                            "tool call was not executed because model output was truncated"
                                .to_owned(),
                        )
                    } else if spec.is_none() {
                        Some(format!("unknown tool {name:?}"))
                    } else {
                        None
                    };
                    Some(PreparedToolCall {
                        call_id: id.clone(),
                        name: name.clone(),
                        effective_args: args.clone(),
                        replay: if precomputed_error.is_some() {
                            ReplaySafety::Never
                        } else {
                            spec.map_or(ReplaySafety::Never, |spec| spec.replay)
                        },
                        precomputed_error,
                    })
                }
                ContentBlock::RejectedToolCall {
                    id,
                    name,
                    args,
                    error,
                } => Some(PreparedToolCall {
                    call_id: id.clone(),
                    name: name.clone(),
                    effective_args: args.clone().unwrap_or(Value::Null),
                    replay: ReplaySafety::Never,
                    precomputed_error: Some(format!("tool arguments rejected: {}", error.message)),
                }),
                _ => None,
            })
            .collect()
    }

    fn request(&self) -> Request {
        Request {
            system: self.config.system.clone(),
            messages: self.provider_messages.clone(),
            tools: self
                .config
                .tools
                .iter()
                .map(|tool| tool.definition.clone())
                .collect(),
            max_output_tokens: self.config.max_output_tokens,
            thinking: self.config.thinking,
        }
    }

    fn summary_request(&self, work: &CompactionWork) -> Request {
        let policy = self
            .config
            .compaction
            .as_ref()
            .expect("compaction work requires an enabled policy");
        Request {
            system: policy.system_prompt.clone(),
            messages: work
                .compacted
                .iter()
                .map(SessionMessage::to_provider)
                .collect(),
            tools: Vec::new(),
            max_output_tokens: self.config.max_output_tokens,
            thinking: self.config.thinking,
        }
    }

    fn drain_steers(
        &mut self,
        op: &OpId,
        stamps: Vec<EntryStamp>,
        effects: &mut Vec<Effect>,
    ) -> Result<bool, MachineError> {
        let expected = self.pending_steers();
        if stamps.len() != expected {
            return Err(MachineError::SteerStampCount {
                expected,
                actual: stamps.len(),
            });
        }
        let mut stamps = stamps.into_iter();
        let mut remaining = VecDeque::new();
        let mut drained = false;
        while let Some(item) = self.queued.pop_front() {
            if item.kind == QueueKind::FollowUp {
                remaining.push_back(item);
                continue;
            }
            let stamp = stamps
                .next()
                .expect("steering stamp count was checked before draining");
            let message = item.message;
            let entry = NewEntry {
                id: stamp.id,
                parent: self.leaf.clone(),
                lane: LaneName::main(),
                op: Some(op.clone()),
                source_queue: Some(item.id),
                at: stamp.at,
                body: EntryBody::Message {
                    message: message.clone(),
                },
            };
            self.remember_entry(&entry);
            effects.push(Effect::AppendEntry(entry));
            effects.push(Effect::Emit(AgentEvent::MessageAppended {
                op: op.clone(),
                message,
            }));
            drained = true;
        }
        self.queued = remaining;
        Ok(drained)
    }

    fn remember_entry(&mut self, entry: &NewEntry) {
        self.leaf = Some(entry.id.clone());
        self.entries.push(Entry {
            seq: 0,
            id: entry.id.clone(),
            parent: entry.parent.clone(),
            lane: entry.lane.clone(),
            op: entry.op.clone(),
            source_queue: entry.source_queue.clone(),
            at: entry.at.clone(),
            body: entry.body.clone(),
        });
        self.provider_messages = assemble_context(&self.entries)
            .expect("machine entries remain a valid root-to-leaf path")
            .messages;
    }
}

fn record_effect(at: &Timestamp, body: RecordBody) -> Effect {
    Effect::AppendRecord(NewRecord {
        lane: LaneName::main(),
        at: at.clone(),
        body,
    })
}

fn start_tool_effects(op: &OpId, call: &PreparedToolCall, at: &Timestamp) -> Vec<Effect> {
    let mut effects = vec![record_effect(
        at,
        RecordBody::ToolStarted {
            op: op.clone(),
            call_id: call.call_id.clone(),
            name: call.name.clone(),
            effective_args: call.effective_args.clone(),
            replay: call.replay,
        },
    )];
    effects.push(Effect::Emit(AgentEvent::ToolExecutionStarted {
        op: op.clone(),
        call_id: call.call_id.clone(),
        name: call.name.clone(),
    }));
    effects
}

fn finish_effects(op: &OpId, outcome: OpOutcome, at: &Timestamp) -> Vec<Effect> {
    vec![
        record_effect(
            at,
            RecordBody::OpFinished {
                op: op.clone(),
                outcome: outcome.clone(),
            },
        ),
        Effect::Emit(AgentEvent::OperationFinished {
            op: op.clone(),
            outcome,
        }),
    ]
}

#[cfg(test)]
mod tests {
    use rho_ai::{AssistantMessage, ModelId, ProviderId, StopReason, ToolCallId, Usage};
    use serde_json::json;

    use crate::Item;

    use super::*;

    fn config() -> MachineConfig {
        MachineConfig {
            system: "test".to_owned(),
            max_output_tokens: 100,
            thinking: ThinkingLevel::None,
            model: crate::ModelRef {
                provider: ProviderId::from("p"),
                model: ModelId::from("m"),
            },
            tools: vec![ToolSpec {
                definition: ToolDefinition::new("read", "read", json!({"type": "object"})),
                replay: ReplaySafety::Safe,
            }],
            hooks: Vec::new(),
            compaction: None,
        }
    }

    fn stamp(id: &str) -> EntryStamp {
        EntryStamp {
            id: EntryId::from(id),
            at: Timestamp::from("t"),
        }
    }

    fn item_record(seq: u64, body: RecordBody) -> Item {
        Item::Record(crate::Record {
            seq,
            lane: LaneName::main(),
            at: Timestamp::from(format!("t{seq}")),
            body,
        })
    }

    fn assistant(stop: StopReason, blocks: Vec<ContentBlock>) -> AssistantMessage {
        AssistantMessage {
            blocks,
            stop,
            usage: Usage::default(),
            provider: ProviderId::from("p"),
            model: ModelId::from("m"),
        }
    }

    fn persisted_prefix(message: AssistantMessage, include_usage: bool) -> (Vec<Entry>, Vec<Item>) {
        let op = OpId::from("op");
        let user = Entry {
            seq: 1,
            id: EntryId::from("user"),
            parent: None,
            lane: LaneName::main(),
            op: Some(op.clone()),
            source_queue: None,
            at: Timestamp::from("t1"),
            body: EntryBody::Message {
                message: SessionMessage::user("hello"),
            },
        };
        let assistant = Entry {
            seq: 4,
            id: EntryId::from("assistant"),
            parent: Some(user.id.clone()),
            lane: LaneName::main(),
            op: Some(op.clone()),
            source_queue: None,
            at: Timestamp::from("t3"),
            body: EntryBody::Message {
                message: SessionMessage::Assistant(message.clone()),
            },
        };
        let mut items = vec![
            Item::Entry(user.clone()),
            Item::Record(crate::Record {
                seq: 2,
                lane: LaneName::main(),
                at: Timestamp::from("t1"),
                body: RecordBody::OpStarted {
                    op: op.clone(),
                    intent: OpIntent::Run,
                    origin: Origin::External,
                    host: None,
                },
            }),
            Item::Record(crate::Record {
                seq: 3,
                lane: LaneName::main(),
                at: Timestamp::from("t2"),
                body: RecordBody::Step {
                    op: op.clone(),
                    n: 1,
                },
            }),
            Item::Entry(assistant.clone()),
        ];
        if include_usage {
            items.push(Item::Record(crate::Record {
                seq: 5,
                lane: LaneName::main(),
                at: Timestamp::from("t3"),
                body: RecordBody::Usage {
                    op,
                    usage: message.usage,
                },
            }));
        }
        (vec![user, assistant], items)
    }

    #[test]
    fn prompt_and_terminal_message_derive_action_boundary_journal() {
        let machine = SessionMachine::new(config(), Vec::new()).unwrap();
        let (machine, Step::Do { effects, action }) = machine
            .handle(Input::Prompt {
                message: SessionMessage::user("hello"),
                op: OpId::from("op"),
                stamp: stamp("user"),
                origin: Origin::External,
                host: None,
                queue: None,
                steer_stamps: Vec::new(),
            })
            .unwrap()
        else {
            panic!("expected work");
        };
        assert_eq!(effects.len(), 4);
        let Some(Action::StreamAssistant { request, .. }) = action else {
            panic!("expected provider action");
        };
        assert_eq!(request.messages, [rho_ai::Message::user("hello")]);

        let (machine, Step::Do { effects, action }) = machine
            .resolve(ActionOutcome::Assistant {
                result: Ok(assistant(
                    StopReason::Stop,
                    vec![ContentBlock::text("done")],
                )),
                stamp: stamp("assistant"),
                steer_stamps: Vec::new(),
            })
            .unwrap()
        else {
            panic!("expected terminal effects");
        };
        assert!(machine.is_idle());
        assert!(action.is_none());
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::AppendRecord(NewRecord {
                body: RecordBody::OpFinished {
                    outcome: OpOutcome::Completed,
                    ..
                },
                ..
            })
        )));
    }

    #[test]
    fn steering_received_in_flight_is_durable_and_continues_the_same_operation() {
        let machine = SessionMachine::new(config(), Vec::new()).unwrap();
        let (mut machine, _) = machine
            .handle(Input::Prompt {
                message: SessionMessage::user("initial"),
                op: OpId::from("op"),
                stamp: stamp("initial"),
                origin: Origin::External,
                host: None,
                queue: None,
                steer_stamps: Vec::new(),
            })
            .unwrap();
        let effects = machine
            .accept_control(
                SessionControl::Enqueue {
                    id: QueueId::from("steer"),
                    kind: QueueKind::Steer,
                    message: SessionMessage::user("course correction"),
                },
                Timestamp::from("t-control"),
            )
            .unwrap();
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::AppendRecord(NewRecord {
                body: RecordBody::QueueChanged {
                    op: Some(op),
                    change: QueueChange::Enqueued { id, .. },
                },
                ..
            }) if op == &OpId::from("op") && id == &QueueId::from("steer")
        )));

        let (machine, Step::Do { effects, action }) = machine
            .resolve(ActionOutcome::Assistant {
                result: Ok(assistant(
                    StopReason::Stop,
                    vec![ContentBlock::text("first answer")],
                )),
                stamp: stamp("assistant"),
                steer_stamps: vec![stamp("steer-entry")],
            })
            .unwrap()
        else {
            panic!("expected steered continuation");
        };
        assert!(!machine.is_idle());
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::AppendEntry(NewEntry {
                source_queue: Some(id),
                ..
            }) if id == &QueueId::from("steer")
        )));
        assert!(!effects.iter().any(|effect| matches!(
            effect,
            Effect::AppendRecord(NewRecord {
                body: RecordBody::OpFinished { .. },
                ..
            })
        )));
        let Some(Action::StreamAssistant { request, .. }) = action else {
            panic!("expected another provider step");
        };
        assert_eq!(
            request.messages,
            [
                rho_ai::Message::user("initial"),
                rho_ai::Message::Assistant(assistant(
                    StopReason::Stop,
                    vec![ContentBlock::text("first answer")],
                )),
                rho_ai::Message::user("course correction"),
            ]
        );
    }

    #[test]
    fn hook_interaction_is_journaled_before_the_hook_resumes() {
        let mut hooked = config();
        hooked.hooks = vec![HookPoint::RunStarted];
        let machine = SessionMachine::new(hooked, Vec::new()).unwrap();
        let (machine, Step::Do { effects, action }) = machine
            .handle(Input::Prompt {
                message: SessionMessage::user("hello"),
                op: OpId::from("op"),
                stamp: stamp("user"),
                origin: Origin::External,
                host: None,
                queue: None,
                steer_stamps: Vec::new(),
            })
            .unwrap()
        else {
            panic!("expected run-started hook");
        };
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::AppendRecord(NewRecord {
                body: RecordBody::HookStarted {
                    n: 1,
                    invocation: HookInvocation {
                        hook: HookPoint::RunStarted,
                        ..
                    },
                    ..
                },
                ..
            })
        )));
        assert!(matches!(action, Some(Action::InvokeHook { .. })));

        let request = InteractionRequest {
            id: "permission".to_owned(),
            prompt: "continue?".to_owned(),
            timeout_ms: 1_000,
        };
        let (machine, Step::Do { effects, action }) = machine
            .resolve(ActionOutcome::Hook {
                result: Ok(HookOutput::Interact {
                    request: request.clone(),
                }),
                at: Timestamp::from("t-hook"),
            })
            .unwrap()
        else {
            panic!("expected interaction action");
        };
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::AppendRecord(NewRecord {
                body: RecordBody::InteractionRequested { request: actual, .. },
                ..
            }) if actual == &request
        )));
        assert!(matches!(action, Some(Action::AwaitInteraction { .. })));

        let (machine, Step::Do { effects, action }) = machine
            .resolve(ActionOutcome::Interaction {
                request_id: request.id.clone(),
                answer: InteractionAnswer::Answered {
                    value: "yes".to_owned(),
                },
                at: Timestamp::from("t-answer"),
            })
            .unwrap()
        else {
            panic!("expected resumed hook");
        };
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::AppendRecord(NewRecord {
                body: RecordBody::InteractionAnswered { request_id, .. },
                ..
            }) if request_id == "permission"
        )));
        let Some(Action::InvokeHook { invocation, .. }) = action else {
            panic!("expected resumed hook action");
        };
        assert_eq!(
            invocation.payload["interaction"]["answer"]["kind"],
            "answered"
        );

        let (_, Step::Do { effects, action }) = machine
            .resolve(ActionOutcome::Hook {
                result: Ok(HookOutput::Continue { value: Value::Null }),
                at: Timestamp::from("t-hook-finished"),
            })
            .unwrap()
        else {
            panic!("expected provider action");
        };
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::AppendRecord(NewRecord {
                body: RecordBody::HookFinished { n: 1, .. },
                ..
            })
        )));
        assert!(matches!(action, Some(Action::StreamAssistant { .. })));
    }

    #[test]
    fn before_tool_hook_is_validated_and_journaled_before_execution() {
        let mut hooked = config();
        hooked.tools[0].definition.parameters = json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
            "additionalProperties": false
        });
        hooked.hooks = vec![HookPoint::BeforeTool];
        let machine = SessionMachine::new(hooked, Vec::new()).unwrap();
        let (machine, _) = machine
            .handle(Input::Prompt {
                message: SessionMessage::user("read"),
                op: OpId::from("op"),
                stamp: stamp("user"),
                origin: Origin::External,
                host: None,
                queue: None,
                steer_stamps: Vec::new(),
            })
            .unwrap();
        let call_id = ToolCallId::from("call");
        let (machine, Step::Do { effects, action }) = machine
            .resolve(ActionOutcome::Assistant {
                result: Ok(assistant(
                    StopReason::ToolUse,
                    vec![ContentBlock::ToolCall {
                        id: call_id.clone(),
                        name: "read".to_owned(),
                        args: json!({"path": "old"}),
                    }],
                )),
                stamp: stamp("assistant"),
                steer_stamps: Vec::new(),
            })
            .unwrap()
        else {
            panic!("expected before-tool hook");
        };
        assert!(!effects.iter().any(|effect| matches!(
            effect,
            Effect::AppendRecord(NewRecord {
                body: RecordBody::ToolStarted { .. },
                ..
            })
        )));
        let Some(Action::InvokeHook { invocation, .. }) = action else {
            panic!("expected hook action");
        };
        let mut call: PreparedToolCall = serde_json::from_value(invocation.payload).unwrap();
        call.effective_args = json!({"path": "new"});
        let (_, Step::Do { effects, action }) = machine
            .resolve(ActionOutcome::Hook {
                result: Ok(HookOutput::Continue {
                    value: serde_json::to_value(call).unwrap(),
                }),
                at: Timestamp::from("t-hook"),
            })
            .unwrap()
        else {
            panic!("expected tool action");
        };
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::AppendRecord(NewRecord {
                body: RecordBody::ToolStarted {
                    call_id: actual,
                    effective_args,
                    ..
                },
                ..
            }) if actual == &call_id && effective_args == &json!({"path": "new"})
        )));
        let Some(Action::ExecuteTool { call, .. }) = action else {
            panic!("expected tool action");
        };
        assert_eq!(call.effective_args, json!({"path": "new"}));
    }

    #[test]
    fn request_hooks_preserve_mutations_and_run_finish_precedes_terminal_record() {
        let mut hooked = config();
        hooked.hooks = vec![
            HookPoint::TransformContext,
            HookPoint::BeforeRequest,
            HookPoint::RunFinished,
        ];
        let machine = SessionMachine::new(hooked, Vec::new()).unwrap();
        let (machine, Step::Do { action, .. }) = machine
            .handle(Input::Prompt {
                message: SessionMessage::user("hello"),
                op: OpId::from("op"),
                stamp: stamp("user"),
                origin: Origin::External,
                host: None,
                queue: None,
                steer_stamps: Vec::new(),
            })
            .unwrap()
        else {
            panic!("expected context hook");
        };
        let Some(Action::InvokeHook { invocation, .. }) = action else {
            panic!("expected context hook");
        };
        let mut request: Request = serde_json::from_value(invocation.payload).unwrap();
        request.system = "transformed".to_owned();
        let (machine, Step::Do { action, .. }) = machine
            .resolve(ActionOutcome::Hook {
                result: Ok(HookOutput::Continue {
                    value: serde_json::to_value(request).unwrap(),
                }),
                at: Timestamp::from("t-context"),
            })
            .unwrap()
        else {
            panic!("expected before-request hook");
        };
        let Some(Action::InvokeHook { invocation, .. }) = action else {
            panic!("expected before-request hook");
        };
        let mut request: Request = serde_json::from_value(invocation.payload).unwrap();
        assert_eq!(request.system, "transformed");
        request.max_output_tokens = 17;
        let (mut machine, Step::Do { action, .. }) = machine
            .resolve(ActionOutcome::Hook {
                result: Ok(HookOutput::Continue {
                    value: serde_json::to_value(request).unwrap(),
                }),
                at: Timestamp::from("t-request"),
            })
            .unwrap()
        else {
            panic!("expected provider action");
        };
        let action = action.unwrap();
        let (_, action) = machine.prepare_action(action, Vec::new()).unwrap();
        let Action::StreamAssistant { request, .. } = action else {
            panic!("expected provider action");
        };
        assert_eq!(request.system, "transformed");
        assert_eq!(request.max_output_tokens, 17);

        let (machine, Step::Do { effects, action }) = machine
            .resolve(ActionOutcome::Assistant {
                result: Ok(assistant(
                    StopReason::Stop,
                    vec![ContentBlock::text("done")],
                )),
                stamp: stamp("assistant"),
                steer_stamps: Vec::new(),
            })
            .unwrap()
        else {
            panic!("expected run-finished hook");
        };
        assert!(!effects.iter().any(|effect| matches!(
            effect,
            Effect::AppendRecord(NewRecord {
                body: RecordBody::OpFinished { .. },
                ..
            })
        )));
        assert!(matches!(action, Some(Action::InvokeHook { .. })));
        let (_, Step::Do { effects, action }) = machine
            .resolve(ActionOutcome::Hook {
                result: Ok(HookOutput::Continue { value: Value::Null }),
                at: Timestamp::from("t-finished"),
            })
            .unwrap()
        else {
            panic!("expected terminal record");
        };
        assert!(action.is_none());
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::AppendRecord(NewRecord {
                body: RecordBody::OpFinished {
                    outcome: OpOutcome::Completed,
                    ..
                },
                ..
            })
        )));
    }

    #[test]
    fn invalid_hook_value_is_durable_and_fails_the_operation() {
        let mut hooked = config();
        hooked.hooks = vec![HookPoint::BeforeRequest];
        let machine = SessionMachine::new(hooked, Vec::new()).unwrap();
        let (machine, _) = machine
            .handle(Input::Prompt {
                message: SessionMessage::user("hello"),
                op: OpId::from("op"),
                stamp: stamp("user"),
                origin: Origin::External,
                host: None,
                queue: None,
                steer_stamps: Vec::new(),
            })
            .unwrap();
        let (_, Step::Do { effects, action }) = machine
            .resolve(ActionOutcome::Hook {
                result: Ok(HookOutput::Continue {
                    value: json!({"not": "a request"}),
                }),
                at: Timestamp::from("t-hook"),
            })
            .unwrap()
        else {
            panic!("expected terminal failure");
        };
        assert!(action.is_none());
        let hook_finished = effects
            .iter()
            .position(|effect| {
                matches!(
                    effect,
                    Effect::AppendRecord(NewRecord {
                        body: RecordBody::HookFinished { .. },
                        ..
                    })
                )
            })
            .unwrap();
        let op_finished = effects
            .iter()
            .position(|effect| {
                matches!(
                    effect,
                    Effect::AppendRecord(NewRecord {
                        body: RecordBody::OpFinished {
                            outcome: OpOutcome::Failed { .. },
                            ..
                        },
                        ..
                    })
                )
            })
            .unwrap();
        assert!(hook_finished < op_finished);

        let mut hooked = config();
        hooked.hooks = vec![HookPoint::RunStarted];
        let machine = SessionMachine::new(hooked, Vec::new()).unwrap();
        let (machine, _) = machine
            .handle(Input::Prompt {
                message: SessionMessage::user("hello"),
                op: OpId::from("empty-interaction"),
                stamp: stamp("empty-interaction-user"),
                origin: Origin::External,
                host: None,
                queue: None,
                steer_stamps: Vec::new(),
            })
            .unwrap();
        let (_, Step::Do { effects, action }) = machine
            .resolve(ActionOutcome::Hook {
                result: Ok(HookOutput::Interact {
                    request: InteractionRequest {
                        id: String::new(),
                        prompt: "invalid".to_owned(),
                        timeout_ms: 1_000,
                    },
                }),
                at: Timestamp::from("t-empty"),
            })
            .unwrap()
        else {
            panic!("expected durable terminal failure");
        };
        assert!(action.is_none());
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::AppendRecord(NewRecord {
                body: RecordBody::HookFinished { result: Err(_), .. },
                ..
            })
        )));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::AppendRecord(NewRecord {
                body: RecordBody::OpFinished {
                    outcome: OpOutcome::Failed { .. },
                    ..
                },
                ..
            })
        )));
    }

    #[test]
    fn recovery_times_out_an_unanswered_interaction_without_reasking() {
        let mut hooked = config();
        hooked.hooks = vec![HookPoint::RunStarted];
        let op = OpId::from("op");
        let user = Entry {
            seq: 1,
            id: EntryId::from("user"),
            parent: None,
            lane: LaneName::main(),
            op: Some(op.clone()),
            source_queue: None,
            at: Timestamp::from("t1"),
            body: EntryBody::Message {
                message: SessionMessage::user("hello"),
            },
        };
        let invocation = HookInvocation {
            hook: HookPoint::RunStarted,
            payload: json!({"op": "op", "origin": "external"}),
        };
        let request = InteractionRequest {
            id: "pending".to_owned(),
            prompt: "continue?".to_owned(),
            timeout_ms: 10_000,
        };
        let items = vec![
            Item::Entry(user.clone()),
            Item::Record(crate::Record {
                seq: 2,
                lane: LaneName::main(),
                at: Timestamp::from("t1"),
                body: RecordBody::OpStarted {
                    op: op.clone(),
                    intent: OpIntent::Run,
                    origin: Origin::External,
                    host: None,
                },
            }),
            Item::Record(crate::Record {
                seq: 3,
                lane: LaneName::main(),
                at: Timestamp::from("t2"),
                body: RecordBody::HookStarted {
                    op: op.clone(),
                    n: 1,
                    invocation,
                },
            }),
            Item::Record(crate::Record {
                seq: 4,
                lane: LaneName::main(),
                at: Timestamp::from("t3"),
                body: RecordBody::InteractionRequested {
                    op,
                    hook: 1,
                    request,
                },
            }),
        ];
        let status = crate::reduce_lane_status(&items, &LaneName::main());
        let machine = SessionMachine::new(hooked, vec![user]).unwrap();
        let (_, Step::Do { effects, action }) = machine
            .handle(Input::Resume {
                status,
                at: Timestamp::from("resume"),
                steer_stamps: Vec::new(),
            })
            .unwrap()
        else {
            panic!("expected resumed hook action");
        };
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::AppendRecord(NewRecord {
                body: RecordBody::InteractionAnswered {
                    answer: InteractionAnswer::TimedOut,
                    ..
                },
                ..
            })
        )));
        let Some(Action::InvokeHook { invocation, .. }) = action else {
            panic!("expected hook replay");
        };
        assert_eq!(
            invocation.payload["interaction"]["answer"]["kind"],
            "timed_out"
        );
    }

    #[test]
    fn recovery_reconstructs_every_answered_interaction_in_order() {
        let mut hooked = config();
        hooked.hooks = vec![HookPoint::RunStarted];
        let op = OpId::from("op");
        let user = Entry {
            seq: 1,
            id: EntryId::from("user"),
            parent: None,
            lane: LaneName::main(),
            op: Some(op.clone()),
            source_queue: None,
            at: Timestamp::from("t1"),
            body: EntryBody::Message {
                message: SessionMessage::user("hello"),
            },
        };
        let first = InteractionRequest {
            id: "first".to_owned(),
            prompt: "first?".to_owned(),
            timeout_ms: 10_000,
        };
        let second = InteractionRequest {
            id: "second".to_owned(),
            prompt: "second?".to_owned(),
            timeout_ms: 10_000,
        };
        let items = vec![
            Item::Entry(user.clone()),
            item_record(
                2,
                RecordBody::OpStarted {
                    op: op.clone(),
                    intent: OpIntent::Run,
                    origin: Origin::External,
                    host: None,
                },
            ),
            item_record(
                3,
                RecordBody::HookStarted {
                    op: op.clone(),
                    n: 1,
                    invocation: HookInvocation {
                        hook: HookPoint::RunStarted,
                        payload: json!({"seed": 1}),
                    },
                },
            ),
            item_record(
                4,
                RecordBody::InteractionRequested {
                    op: op.clone(),
                    hook: 1,
                    request: first.clone(),
                },
            ),
            item_record(
                5,
                RecordBody::InteractionAnswered {
                    op: op.clone(),
                    hook: 1,
                    request_id: first.id.clone(),
                    answer: InteractionAnswer::Answered {
                        value: "one".to_owned(),
                    },
                },
            ),
            item_record(
                6,
                RecordBody::InteractionRequested {
                    op: op.clone(),
                    hook: 1,
                    request: second.clone(),
                },
            ),
            item_record(
                7,
                RecordBody::InteractionAnswered {
                    op,
                    hook: 1,
                    request_id: second.id,
                    answer: InteractionAnswer::Declined,
                },
            ),
        ];
        let status = crate::reduce_lane_status(&items, &LaneName::main());
        let machine = SessionMachine::new(hooked, vec![user]).unwrap();
        let (_, Step::Do { effects, action }) = machine
            .handle(Input::Resume {
                status,
                at: Timestamp::from("resume"),
                steer_stamps: Vec::new(),
            })
            .unwrap()
        else {
            panic!("expected resumed hook");
        };
        assert!(!effects.iter().any(|effect| matches!(
            effect,
            Effect::AppendRecord(NewRecord {
                body: RecordBody::InteractionAnswered { .. },
                ..
            })
        )));
        let Some(Action::InvokeHook { invocation, .. }) = action else {
            panic!("expected hook replay");
        };
        assert_eq!(
            invocation.payload["input"]["interaction"]["answer"]["value"],
            "one"
        );
        assert_eq!(
            invocation.payload["interaction"]["answer"]["kind"],
            "declined"
        );
        assert_eq!(invocation.payload["input"]["input"]["seed"], 1);
    }

    #[test]
    fn recovery_starts_post_hooks_missing_after_durable_entries() {
        let terminal = assistant(StopReason::Stop, vec![ContentBlock::text("done")]);
        let (entries, items) = persisted_prefix(terminal, true);
        let status = crate::reduce_lane_status(&items, &LaneName::main());
        let mut hooked = config();
        hooked.hooks = vec![HookPoint::AfterRequest];
        let machine = SessionMachine::new(hooked, entries).unwrap();
        let (_, Step::Do { action, .. }) = machine
            .handle(Input::Resume {
                status,
                at: Timestamp::from("resume-request"),
                steer_stamps: Vec::new(),
            })
            .unwrap()
        else {
            panic!("expected missing after-request hook");
        };
        assert!(matches!(
            action,
            Some(Action::InvokeHook {
                invocation: HookInvocation {
                    hook: HookPoint::AfterRequest,
                    ..
                },
                origin: Origin::Replay,
            })
        ));

        let call_id = ToolCallId::from("call");
        let tool_message = assistant(
            StopReason::ToolUse,
            vec![ContentBlock::ToolCall {
                id: call_id.clone(),
                name: "read".to_owned(),
                args: json!({"path": "x"}),
            }],
        );
        let (mut entries, mut items) = persisted_prefix(tool_message, true);
        items.push(item_record(
            6,
            RecordBody::ToolStarted {
                op: OpId::from("op"),
                call_id: call_id.clone(),
                name: "read".to_owned(),
                effective_args: json!({"path": "x"}),
                replay: ReplaySafety::Safe,
            },
        ));
        let result = Entry {
            seq: 7,
            id: EntryId::from("result"),
            parent: Some(EntryId::from("assistant")),
            lane: LaneName::main(),
            op: Some(OpId::from("op")),
            source_queue: None,
            at: Timestamp::from("t7"),
            body: EntryBody::Message {
                message: SessionMessage::ToolResult {
                    call_id,
                    content: vec![ContentBlock::text("contents")],
                    is_error: false,
                    details: None,
                },
            },
        };
        entries.push(result.clone());
        items.push(Item::Entry(result));
        let status = crate::reduce_lane_status(&items, &LaneName::main());
        let mut hooked = config();
        hooked.hooks = vec![HookPoint::AfterTool];
        let machine = SessionMachine::new(hooked, entries).unwrap();
        let (_, Step::Do { action, .. }) = machine
            .handle(Input::Resume {
                status,
                at: Timestamp::from("resume-tool"),
                steer_stamps: Vec::new(),
            })
            .unwrap()
        else {
            panic!("expected missing after-tool hook");
        };
        assert!(matches!(
            action,
            Some(Action::InvokeHook {
                invocation: HookInvocation {
                    hook: HookPoint::AfterTool,
                    ..
                },
                origin: Origin::Replay,
            })
        ));
    }

    #[test]
    fn recovery_starts_missing_compaction_and_finish_hooks() {
        let entries = compaction_entries();
        let policy = compaction_config().compaction.unwrap();
        let plan = plan_compaction(&entries, policy.retain_messages);
        let op = OpId::from("compact");
        let work = CompactionWork {
            compacted: plan.compacted,
            retained_tail: plan.retained_tail,
            first_kept: plan.first_kept,
            tokens_before: 200,
        };
        let mut items = entries.iter().cloned().map(Item::Entry).collect::<Vec<_>>();
        items.extend([
            item_record(
                4,
                RecordBody::OpStarted {
                    op: op.clone(),
                    intent: OpIntent::Compaction,
                    origin: Origin::External,
                    host: None,
                },
            ),
            item_record(5, RecordBody::CompactionStarted { op, work }),
        ]);
        let status = crate::reduce_lane_status(&items, &LaneName::main());
        let mut hooked = compaction_config();
        hooked.hooks = vec![HookPoint::BeforeCompaction];
        let machine = SessionMachine::new(hooked, entries).unwrap();
        let (_, Step::Do { action, .. }) = machine
            .handle(Input::Resume {
                status,
                at: Timestamp::from("resume-compaction"),
                steer_stamps: Vec::new(),
            })
            .unwrap()
        else {
            panic!("expected missing before-compaction hook");
        };
        assert!(matches!(
            action,
            Some(Action::InvokeHook {
                invocation: HookInvocation {
                    hook: HookPoint::BeforeCompaction,
                    ..
                },
                origin: Origin::Replay,
            })
        ));

        let terminal = assistant(StopReason::Stop, vec![ContentBlock::text("done")]);
        let (entries, items) = persisted_prefix(terminal, true);
        let status = crate::reduce_lane_status(&items, &LaneName::main());
        let mut hooked = config();
        hooked.hooks = vec![HookPoint::RunFinished];
        let machine = SessionMachine::new(hooked, entries).unwrap();
        let (_, Step::Do { action, .. }) = machine
            .handle(Input::Resume {
                status,
                at: Timestamp::from("resume-finish"),
                steer_stamps: Vec::new(),
            })
            .unwrap()
        else {
            panic!("expected missing run-finished hook");
        };
        assert!(matches!(
            action,
            Some(Action::InvokeHook {
                invocation: HookInvocation {
                    hook: HookPoint::RunFinished,
                    ..
                },
                origin: Origin::Replay,
            })
        ));
    }

    #[test]
    fn compaction_does_not_fire_user_run_lifecycle_hooks() {
        let mut hooked = compaction_config();
        hooked.hooks = vec![HookPoint::RunStarted, HookPoint::RunFinished];
        let machine = SessionMachine::new(hooked, compaction_entries()).unwrap();
        let (machine, Step::Do { effects, action }) = machine
            .handle(Input::Compact {
                op: OpId::from("compact"),
                at: Timestamp::from("start"),
                origin: Origin::External,
                host: None,
            })
            .unwrap()
        else {
            panic!("expected compaction summary");
        };
        assert!(matches!(action, Some(Action::Summarize { .. })));
        assert!(!effects.iter().any(|effect| matches!(
            effect,
            Effect::AppendRecord(NewRecord {
                body: RecordBody::HookStarted { .. },
                ..
            })
        )));
        let (_, Step::Do { effects, action }) = machine
            .resolve(ActionOutcome::Summary {
                result: Ok(CompactionSummary {
                    text: "summary".to_owned(),
                    usage: Usage::default(),
                }),
                stamp: stamp("summary"),
            })
            .unwrap()
        else {
            panic!("expected terminal compaction effects");
        };
        assert!(action.is_none());
        assert!(!effects.iter().any(|effect| matches!(
            effect,
            Effect::AppendRecord(NewRecord {
                body: RecordBody::HookStarted { .. },
                ..
            })
        )));
    }

    #[test]
    fn recovery_closes_an_open_hook_before_finishing_an_abort() {
        let mut hooked = config();
        hooked.hooks = vec![HookPoint::RunStarted];
        let op = OpId::from("op");
        let user = Entry {
            seq: 1,
            id: EntryId::from("user"),
            parent: None,
            lane: LaneName::main(),
            op: Some(op.clone()),
            source_queue: None,
            at: Timestamp::from("t1"),
            body: EntryBody::Message {
                message: SessionMessage::user("hello"),
            },
        };
        let request = InteractionRequest {
            id: "pending".to_owned(),
            prompt: "continue?".to_owned(),
            timeout_ms: 10_000,
        };
        let items = vec![
            Item::Entry(user.clone()),
            Item::Record(crate::Record {
                seq: 2,
                lane: LaneName::main(),
                at: Timestamp::from("t1"),
                body: RecordBody::OpStarted {
                    op: op.clone(),
                    intent: OpIntent::Run,
                    origin: Origin::External,
                    host: None,
                },
            }),
            Item::Record(crate::Record {
                seq: 3,
                lane: LaneName::main(),
                at: Timestamp::from("t2"),
                body: RecordBody::HookStarted {
                    op: op.clone(),
                    n: 1,
                    invocation: HookInvocation {
                        hook: HookPoint::RunStarted,
                        payload: json!({"op": "op", "origin": "external"}),
                    },
                },
            }),
            Item::Record(crate::Record {
                seq: 4,
                lane: LaneName::main(),
                at: Timestamp::from("t3"),
                body: RecordBody::InteractionRequested {
                    op: op.clone(),
                    hook: 1,
                    request,
                },
            }),
            Item::Record(crate::Record {
                seq: 5,
                lane: LaneName::main(),
                at: Timestamp::from("t4"),
                body: RecordBody::AbortRequested { op },
            }),
        ];
        let status = crate::reduce_lane_status(&items, &LaneName::main());
        let machine = SessionMachine::new(hooked, vec![user]).unwrap();
        let (_, Step::Do { effects, action }) = machine
            .handle(Input::Resume {
                status,
                at: Timestamp::from("resume"),
                steer_stamps: Vec::new(),
            })
            .unwrap()
        else {
            panic!("expected recovered abort effects");
        };
        assert!(action.is_none());
        let records = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::AppendRecord(record) => Some(&record.body),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            records.as_slice(),
            [
                RecordBody::InteractionAnswered {
                    answer: InteractionAnswer::TimedOut,
                    ..
                },
                RecordBody::HookFinished { result: Err(_), .. },
                RecordBody::OpFinished {
                    outcome: OpOutcome::Aborted,
                    ..
                }
            ]
        ));
    }

    #[test]
    fn follow_up_waits_for_the_next_operation_and_abort_is_journaled() {
        let machine = SessionMachine::new(config(), Vec::new()).unwrap();
        let (mut machine, _) = machine
            .handle(Input::Prompt {
                message: SessionMessage::user("initial"),
                op: OpId::from("op"),
                stamp: stamp("initial"),
                origin: Origin::External,
                host: None,
                queue: None,
                steer_stamps: Vec::new(),
            })
            .unwrap();
        machine
            .accept_control(
                SessionControl::Enqueue {
                    id: QueueId::from("follow"),
                    kind: QueueKind::FollowUp,
                    message: SessionMessage::user("next task"),
                },
                Timestamp::from("t-control"),
            )
            .unwrap();
        let abort_effects = machine
            .accept_control(SessionControl::Abort, Timestamp::from("t-abort"))
            .unwrap();
        assert!(abort_effects.iter().any(|effect| matches!(
            effect,
            Effect::AppendRecord(NewRecord {
                body: RecordBody::AbortRequested { op },
                ..
            }) if op == &OpId::from("op")
        )));
        let (mut machine, Step::Do { effects, action }) = machine
            .resolve(ActionOutcome::Assistant {
                result: Ok(assistant(
                    StopReason::Stop,
                    vec![ContentBlock::text("raced with abort")],
                )),
                stamp: stamp("assistant"),
                steer_stamps: Vec::new(),
            })
            .unwrap()
        else {
            panic!("expected abort completion");
        };
        assert!(machine.is_idle());
        assert!(action.is_none());
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::AppendRecord(NewRecord {
                body: RecordBody::OpFinished {
                    outcome: OpOutcome::Aborted,
                    ..
                },
                ..
            })
        )));
        assert_eq!(
            machine.pop_queued_input(),
            Some(QueuedInput {
                id: QueueId::from("follow"),
                kind: QueueKind::FollowUp,
                message: SessionMessage::user("next task"),
            })
        );
    }

    #[test]
    fn tool_calls_are_journaled_before_the_shell_action() {
        let machine = SessionMachine::new(config(), Vec::new()).unwrap();
        let (machine, _) = machine
            .handle(Input::Prompt {
                message: SessionMessage::user("read"),
                op: OpId::from("op"),
                stamp: stamp("user"),
                origin: Origin::External,
                host: None,
                queue: None,
                steer_stamps: Vec::new(),
            })
            .unwrap();
        let call_id = ToolCallId::from("call");
        let (mut machine, Step::Do { effects, action }) = machine
            .resolve(ActionOutcome::Assistant {
                result: Ok(assistant(
                    StopReason::ToolUse,
                    vec![ContentBlock::ToolCall {
                        id: call_id.clone(),
                        name: "read".to_owned(),
                        args: json!({"path": "x"}),
                    }],
                )),
                stamp: stamp("assistant"),
                steer_stamps: Vec::new(),
            })
            .unwrap()
        else {
            panic!("expected tool action");
        };
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::AppendRecord(NewRecord {
                body: RecordBody::ToolStarted { .. },
                ..
            })
        )));
        let action = action.expect("expected tool action");
        assert!(matches!(action, Action::ExecuteTool { .. }));

        machine
            .accept_control(
                SessionControl::Enqueue {
                    id: QueueId::from("steer"),
                    kind: QueueKind::Steer,
                    message: SessionMessage::user("adjust after the tool"),
                },
                Timestamp::from("t-control"),
            )
            .unwrap();
        let (effects, action) = machine.prepare_action(action, Vec::new()).unwrap();
        assert!(effects.is_empty());
        assert!(matches!(action, Action::ExecuteTool { .. }));
        assert_eq!(machine.pending_steers(), 1);

        let (_, Step::Do { action, .. }) = machine
            .resolve(ActionOutcome::Tool {
                call_id,
                content: vec![ContentBlock::text("contents")],
                is_error: false,
                details: None,
                stamp: stamp("result"),
                steer_stamps: vec![stamp("steer-result")],
            })
            .unwrap()
        else {
            panic!("expected next provider action");
        };
        let Some(Action::StreamAssistant { request, .. }) = action else {
            panic!("expected next provider action");
        };
        assert!(matches!(
            request.messages.last(),
            Some(rho_ai::Message::User { content })
                if content == &vec![ContentBlock::text("adjust after the tool")]
        ));
    }

    #[test]
    fn length_stop_never_executes_a_tool() {
        let machine = SessionMachine::new(config(), Vec::new()).unwrap();
        let (machine, _) = machine
            .handle(Input::Prompt {
                message: SessionMessage::user("read"),
                op: OpId::from("op"),
                stamp: stamp("user"),
                origin: Origin::External,
                host: None,
                queue: None,
                steer_stamps: Vec::new(),
            })
            .unwrap();
        let (_, Step::Do { effects, action }) = machine
            .resolve(ActionOutcome::Assistant {
                result: Ok(assistant(
                    StopReason::Length,
                    vec![ContentBlock::ToolCall {
                        id: ToolCallId::from("call"),
                        name: "read".to_owned(),
                        args: json!({}),
                    }],
                )),
                stamp: stamp("assistant"),
                steer_stamps: Vec::new(),
            })
            .unwrap()
        else {
            panic!("expected deterministic tool failure");
        };
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::AppendRecord(NewRecord {
                body: RecordBody::ToolStarted {
                    replay: ReplaySafety::Never,
                    ..
                },
                ..
            })
        )));
        let Some(Action::ExecuteTool { call, .. }) = action else {
            panic!("expected shell-visible precomputed result");
        };
        assert!(call.precomputed_error.is_some());
    }

    #[test]
    fn resume_after_durable_terminal_message_only_derives_missing_effects() {
        for include_usage in [false, true] {
            let terminal = assistant(StopReason::Stop, vec![ContentBlock::text("done")]);
            let (entries, items) = persisted_prefix(terminal, include_usage);
            let status = crate::reduce_lane_status(&items, &LaneName::main());
            let machine = SessionMachine::new(config(), entries).unwrap();
            let (_, Step::Do { effects, action }) = machine
                .handle(Input::Resume {
                    status,
                    at: Timestamp::from("resume"),
                    steer_stamps: Vec::new(),
                })
                .unwrap()
            else {
                panic!("expected recovery effects");
            };
            assert!(action.is_none());
            assert_eq!(
                effects
                    .iter()
                    .filter(|effect| matches!(effect, Effect::AppendRecord(_)))
                    .count(),
                usize::from(!include_usage) + 1
            );
            assert!(effects.iter().any(|effect| matches!(
                effect,
                Effect::AppendRecord(NewRecord {
                    body: RecordBody::OpFinished {
                        outcome: OpOutcome::Completed,
                        ..
                    },
                    ..
                })
            )));
        }
    }

    #[test]
    fn resume_after_assistant_starts_missing_tool_instead_of_restreaming() {
        let call_id = ToolCallId::from("call");
        let tool_message = assistant(
            StopReason::ToolUse,
            vec![ContentBlock::ToolCall {
                id: call_id.clone(),
                name: "read".to_owned(),
                args: json!({"path": "x"}),
            }],
        );
        let (entries, items) = persisted_prefix(tool_message, true);
        let status = crate::reduce_lane_status(&items, &LaneName::main());
        let machine = SessionMachine::new(config(), entries).unwrap();
        let (_, Step::Do { effects, action }) = machine
            .handle(Input::Resume {
                status,
                at: Timestamp::from("resume"),
                steer_stamps: Vec::new(),
            })
            .unwrap()
        else {
            panic!("expected recovery tool action");
        };
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::AppendRecord(NewRecord {
                body: RecordBody::ToolStarted {
                    call_id: started,
                    ..
                },
                ..
            }) if started == &call_id
        )));
        assert!(matches!(
            action,
            Some(Action::ExecuteTool { call, origin: Origin::Replay }) if call.call_id == call_id
        ));
    }

    #[test]
    fn resume_continues_at_the_first_unresolved_tool_in_a_batch() {
        let first = ToolCallId::from("first");
        let second = ToolCallId::from("second");
        let tool_message = assistant(
            StopReason::ToolUse,
            vec![
                ContentBlock::ToolCall {
                    id: first.clone(),
                    name: "read".to_owned(),
                    args: json!({"path": "a"}),
                },
                ContentBlock::ToolCall {
                    id: second.clone(),
                    name: "read".to_owned(),
                    args: json!({"path": "b"}),
                },
            ],
        );
        let (mut entries, mut items) = persisted_prefix(tool_message, true);
        items.push(Item::Record(crate::Record {
            seq: 6,
            lane: LaneName::main(),
            at: Timestamp::from("t4"),
            body: RecordBody::ToolStarted {
                op: OpId::from("op"),
                call_id: first.clone(),
                name: "read".to_owned(),
                effective_args: json!({"path": "a"}),
                replay: ReplaySafety::Safe,
            },
        }));
        let result = Entry {
            seq: 7,
            id: EntryId::from("first-result"),
            parent: Some(EntryId::from("assistant")),
            lane: LaneName::main(),
            op: Some(OpId::from("op")),
            source_queue: None,
            at: Timestamp::from("t5"),
            body: EntryBody::Message {
                message: SessionMessage::ToolResult {
                    call_id: first,
                    content: vec![ContentBlock::text("a")],
                    is_error: false,
                    details: None,
                },
            },
        };
        entries.push(result.clone());
        items.push(Item::Entry(result));

        let status = crate::reduce_lane_status(&items, &LaneName::main());
        let machine = SessionMachine::new(config(), entries).unwrap();
        let (_, Step::Do { effects, action }) = machine
            .handle(Input::Resume {
                status,
                at: Timestamp::from("resume"),
                steer_stamps: Vec::new(),
            })
            .unwrap()
        else {
            panic!("expected second tool action");
        };
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::AppendRecord(NewRecord {
                body: RecordBody::ToolStarted { call_id, .. },
                ..
            }) if call_id == &second
        )));
        assert!(matches!(
            action,
            Some(Action::ExecuteTool { call, .. }) if call.call_id == second
        ));
    }

    #[test]
    fn resume_after_paused_message_derives_the_next_step() {
        let (entries, items) = persisted_prefix(assistant(StopReason::Paused, Vec::new()), true);
        let status = crate::reduce_lane_status(&items, &LaneName::main());
        let machine = SessionMachine::new(config(), entries).unwrap();
        let (_, Step::Do { effects, action }) = machine
            .handle(Input::Resume {
                status,
                at: Timestamp::from("resume"),
                steer_stamps: Vec::new(),
            })
            .unwrap()
        else {
            panic!("expected resumed stream");
        };
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::AppendRecord(NewRecord {
                body: RecordBody::Step { n: 2, .. },
                ..
            })
        )));
        assert!(matches!(
            action,
            Some(Action::StreamAssistant {
                origin: Origin::Replay,
                ..
            })
        ));
    }

    #[test]
    fn resume_closes_the_prompt_entry_before_op_started_crash_window() {
        let user = Entry {
            seq: 1,
            id: EntryId::from("user"),
            parent: None,
            lane: LaneName::main(),
            op: Some(OpId::from("op")),
            source_queue: None,
            at: Timestamp::from("t1"),
            body: EntryBody::Message {
                message: SessionMessage::user("durable prompt"),
            },
        };
        let status = crate::reduce_lane_status(&[Item::Entry(user.clone())], &LaneName::main());
        let machine = SessionMachine::new(config(), vec![user]).unwrap();
        let (_, Step::Do { effects, action }) = machine
            .handle(Input::Resume {
                status,
                at: Timestamp::from("resume"),
                steer_stamps: Vec::new(),
            })
            .unwrap()
        else {
            panic!("expected resumed provider action");
        };
        assert!(matches!(action, Some(Action::StreamAssistant { .. })));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::AppendRecord(NewRecord {
                body: RecordBody::OpStarted {
                    origin: Origin::Replay,
                    ..
                },
                ..
            })
        )));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::AppendRecord(NewRecord {
                body: RecordBody::Step { n: 1, .. },
                ..
            })
        )));
    }

    #[test]
    fn durable_settings_override_constructor_defaults_and_prompt_role_is_checked() {
        let settings = Entry {
            seq: 1,
            id: EntryId::from("settings"),
            parent: None,
            lane: LaneName::main(),
            op: None,
            source_queue: None,
            at: Timestamp::from("t"),
            body: EntryBody::SettingsChange {
                model: Some(crate::ModelRef {
                    provider: ProviderId::from("durable-provider"),
                    model: ModelId::from("durable-model"),
                }),
                thinking: Some(ThinkingLevel::Max),
            },
        };
        let machine = SessionMachine::new(config(), vec![settings]).unwrap();
        assert_eq!(machine.model().model, ModelId::from("durable-model"));
        let (_, Step::Do { action, .. }) = machine
            .clone()
            .handle(Input::Prompt {
                message: SessionMessage::user("continue"),
                op: OpId::from("valid-op"),
                stamp: stamp("valid-entry"),
                origin: Origin::External,
                host: None,
                queue: None,
                steer_stamps: Vec::new(),
            })
            .unwrap()
        else {
            panic!("expected provider action");
        };
        assert!(matches!(
            action,
            Some(Action::StreamAssistant { model, request, .. })
                if model.model == ModelId::from("durable-model")
                    && request.thinking == ThinkingLevel::Max
        ));
        let error = machine
            .handle(Input::Prompt {
                message: SessionMessage::Assistant(assistant(StopReason::Stop, Vec::new())),
                op: OpId::from("op"),
                stamp: stamp("entry"),
                origin: Origin::External,
                host: None,
                queue: None,
                steer_stamps: Vec::new(),
            })
            .unwrap_err();
        assert_eq!(error, MachineError::InvalidPrompt);
    }

    fn compaction_entries() -> Vec<Entry> {
        let mut prior = None;
        [
            SessionMessage::user("old question"),
            SessionMessage::Assistant(AssistantMessage {
                blocks: vec![ContentBlock::text("old answer")],
                stop: StopReason::Stop,
                usage: Usage {
                    input_tokens: 200,
                    ..Usage::default()
                },
                provider: ProviderId::from("p"),
                model: ModelId::from("m"),
            }),
            SessionMessage::user("retain this"),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, message)| {
            let id = EntryId::from(format!("e{}", index + 1));
            let entry = Entry {
                seq: u64::try_from(index + 1).unwrap(),
                id: id.clone(),
                parent: prior.clone(),
                lane: LaneName::main(),
                op: None,
                source_queue: None,
                at: Timestamp::from("t"),
                body: EntryBody::Message { message },
            };
            prior = Some(id);
            entry
        })
        .collect()
    }

    fn compaction_config() -> MachineConfig {
        MachineConfig {
            compaction: Some(CompactionConfig {
                threshold_tokens: 100,
                retain_messages: 1,
                system_prompt: "Summarize the history faithfully.".to_owned(),
            }),
            ..config()
        }
    }

    #[test]
    fn compaction_is_a_durable_operation_with_an_isolated_request() {
        let machine = SessionMachine::new(compaction_config(), compaction_entries()).unwrap();
        assert_eq!(machine.compaction_due(), Some(200));
        let (machine, Step::Do { effects, action }) = machine
            .handle(Input::Compact {
                op: OpId::from("compact"),
                at: Timestamp::from("t4"),
                origin: Origin::External,
                host: None,
            })
            .unwrap()
        else {
            panic!("expected compaction action");
        };
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::AppendRecord(NewRecord {
                body: RecordBody::CompactionStarted { work, .. },
                ..
            }) if work.tokens_before == 200 && work.retained_tail.len() == 1
        )));
        let Some(Action::Summarize { request, .. }) = action else {
            panic!("expected summarization request");
        };
        assert_eq!(request.system, "Summarize the history faithfully.");
        assert_eq!(request.messages.len(), 2);
        assert!(request.tools.is_empty());

        let (machine, Step::Do { effects, action }) = machine
            .resolve(ActionOutcome::Summary {
                result: Ok(CompactionSummary {
                    text: "old history summary".to_owned(),
                    usage: Usage {
                        input_tokens: 50,
                        output_tokens: 10,
                        ..Usage::default()
                    },
                }),
                stamp: stamp("checkpoint"),
            })
            .unwrap()
        else {
            panic!("expected terminal compaction effects");
        };
        assert!(action.is_none());
        assert!(machine.is_idle());
        assert_eq!(machine.compaction_due(), None);
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::AppendEntry(NewEntry {
                body: EntryBody::Compaction {
                    summary,
                    retained_tail,
                    tokens_before: 200,
                    ..
                },
                ..
            }) if summary == "old history summary" && retained_tail.len() == 1
        )));
    }

    #[test]
    fn resume_finishes_a_durable_compaction_checkpoint_without_reissuing_it() {
        let op = OpId::from("compact");
        let work = CompactionWork {
            compacted: vec![SessionMessage::user("old")],
            retained_tail: vec![SessionMessage::user("tail")],
            first_kept: Some(EntryId::from("e3")),
            tokens_before: 200,
        };
        let mut entries = compaction_entries();
        entries.push(Entry {
            seq: 6,
            id: EntryId::from("checkpoint"),
            parent: Some(EntryId::from("e3")),
            lane: LaneName::main(),
            op: Some(op.clone()),
            source_queue: None,
            at: Timestamp::from("t6"),
            body: EntryBody::Compaction {
                summary: "summary".to_owned(),
                first_kept: work.first_kept.clone(),
                retained_tail: work.retained_tail.clone(),
                tokens_before: work.tokens_before,
                usage: Usage::default(),
            },
        });
        let items = vec![
            Item::Entry(entries[0].clone()),
            Item::Entry(entries[1].clone()),
            Item::Entry(entries[2].clone()),
            Item::Record(crate::Record {
                seq: 4,
                lane: LaneName::main(),
                at: Timestamp::from("t4"),
                body: RecordBody::OpStarted {
                    op: op.clone(),
                    intent: OpIntent::Compaction,
                    origin: Origin::External,
                    host: None,
                },
            }),
            Item::Record(crate::Record {
                seq: 5,
                lane: LaneName::main(),
                at: Timestamp::from("t5"),
                body: RecordBody::CompactionStarted {
                    op: op.clone(),
                    work,
                },
            }),
            Item::Record(crate::Record {
                seq: 6,
                lane: LaneName::main(),
                at: Timestamp::from("t5"),
                body: RecordBody::Step {
                    op: op.clone(),
                    n: 1,
                },
            }),
            Item::Entry(Entry {
                seq: 7,
                ..entries[3].clone()
            }),
        ];
        let status = crate::reduce_lane_status(&items, &LaneName::main());
        assert!(matches!(
            &status,
            LaneStatus::Suspended(suspended)
                if matches!(
                    suspended.compaction.as_deref(),
                    Some(crate::SuspendedCompaction {
                        completed: Some(_),
                        ..
                    })
                )
        ));
        let machine = SessionMachine::new(compaction_config(), entries).unwrap();
        let (machine, Step::Do { effects, action }) = machine
            .handle(Input::Resume {
                status,
                at: Timestamp::from("t8"),
                steer_stamps: Vec::new(),
            })
            .unwrap()
        else {
            panic!("expected recovery effects");
        };
        assert!(machine.is_idle());
        assert!(action.is_none());
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::AppendRecord(NewRecord {
                body: RecordBody::OpFinished {
                    outcome: OpOutcome::Completed,
                    ..
                },
                ..
            })
        )));
    }
}
