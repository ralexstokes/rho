use std::collections::VecDeque;

use rho_ai::{
    AssistantMessage, ContentBlock, ErrorKind, ProviderError, Request, StopReason, ThinkingLevel,
    ToolCallId, ToolDefinition, Usage,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::{
    CompactionWork, ContextError, Entry, EntryBody, EntryId, HostInfo, LaneName, LaneStatus,
    NewEntry, NewRecord, OpId, OpIntent, OpOutcome, Origin, RecordBody, ReplaySafety,
    SessionMessage, Timestamp, assemble_context, plan_compaction,
};

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

/// Headless interaction request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InteractionRequest {
    /// Stable request identifier.
    pub id: String,
    /// User-facing prompt.
    pub prompt: String,
    /// Timeout in milliseconds.
    pub timeout_ms: u64,
}

/// Durable answer to a headless interaction.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InteractionAnswer {
    /// User supplied a value.
    Answered {
        /// Answer text.
        value: String,
    },
    /// User declined.
    Declined,
    /// No answer arrived before the deadline.
    TimedOut,
}

/// Owned, serializable hook invocation payload.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct HookInvocation {
    /// Hook name.
    pub hook: String,
    /// Hook-specific owned payload.
    pub payload: Value,
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
        /// Durable answer.
        answer: InteractionAnswer,
    },
    /// A hook completed.
    Hook {
        /// Hook-specific owned result.
        result: Result<Value, String>,
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

    /// Handles an external or recovery command.
    pub fn handle(mut self, input: Input) -> Result<(Self, Step), MachineError> {
        match input {
            Input::Prompt {
                message,
                op,
                stamp,
                origin,
                host,
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
                    at: stamp.at.clone(),
                    body: EntryBody::Message {
                        message: message.clone(),
                    },
                };
                self.remember_entry(&entry);
                self.phase = Phase::AwaitingAssistant {
                    op: op.clone(),
                    step: 1,
                    origin,
                };
                let effects = vec![
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
                    record_effect(
                        &stamp.at,
                        RecordBody::Step {
                            op: op.clone(),
                            n: 1,
                        },
                    ),
                    Effect::Emit(AgentEvent::OperationStarted { op, origin }),
                ];
                let action = Action::StreamAssistant {
                    request: self.request(),
                    model: self.config.model.clone(),
                    origin,
                };
                Ok((
                    self,
                    Step::Do {
                        effects,
                        action: Some(action),
                    },
                ))
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
                self.phase = Phase::AwaitingSummary {
                    op: op.clone(),
                    step: 1,
                    work: work.clone(),
                    origin,
                };
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
                    record_effect(
                        &at,
                        RecordBody::Step {
                            op: op.clone(),
                            n: 1,
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
                let action = Action::Summarize {
                    request: self.summary_request(&work),
                    model: self.config.model.clone(),
                    origin,
                };
                Ok((
                    self,
                    Step::Do {
                        effects,
                        action: Some(action),
                    },
                ))
            }
            Input::Resume { status, at } => match status {
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

                    if suspended.intent == OpIntent::Compaction {
                        return self.resume_compaction(suspended, at, effects);
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
                            effects.extend(finish_effects(&op, OpOutcome::Aborted, &at));
                            self.phase = Phase::Idle;
                            return Ok((
                                self,
                                Step::Do {
                                    effects,
                                    action: None,
                                },
                            ));
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
                        return self.resume_after_assistant(suspended, message, at, effects);
                    }

                    let next = suspended.last_step.unwrap_or(0) + 1;
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
            },
        }
    }

    fn resume_compaction(
        mut self,
        suspended: crate::SuspendedOp,
        at: Timestamp,
        mut effects: Vec<Effect>,
    ) -> Result<(Self, Step), MachineError> {
        let op = suspended.op;
        if suspended.abort_requested {
            effects.extend(finish_effects(&op, OpOutcome::Aborted, &at));
            self.phase = Phase::Idle;
            return Ok((
                self,
                Step::Do {
                    effects,
                    action: None,
                },
            ));
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
            effects.extend(finish_effects(&op, OpOutcome::Completed, &at));
            self.phase = Phase::Idle;
            self.last_input_tokens = 0;
            return Ok((
                self,
                Step::Do {
                    effects,
                    action: None,
                },
            ));
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
        mut self,
        suspended: crate::SuspendedOp,
        message: AssistantMessage,
        at: Timestamp,
        mut effects: Vec<Effect>,
    ) -> Result<(Self, Step), MachineError> {
        let op = suspended.op;
        let step = suspended.last_step.unwrap_or(0);
        let open_tools = suspended.open_tools;
        let resolved_tool_calls = suspended.resolved_tool_calls;
        match message.stop {
            StopReason::Stop => {
                effects.extend(finish_effects(&op, OpOutcome::Completed, &at));
                self.phase = Phase::Idle;
                Ok((
                    self,
                    Step::Do {
                        effects,
                        action: None,
                    },
                ))
            }
            StopReason::ToolUse | StopReason::Length => {
                let prepared = self.prepare_calls(&message, message.stop == StopReason::Length);
                if prepared.is_empty() {
                    let error = if message.stop == StopReason::Length {
                        "provider output was truncated before completing the turn"
                    } else {
                        "provider stopped for tool use without returning a tool call"
                    };
                    effects.extend(finish_effects(
                        &op,
                        OpOutcome::Failed {
                            error: error.to_owned(),
                        },
                        &at,
                    ));
                    self.phase = Phase::Idle;
                    return Ok((
                        self,
                        Step::Do {
                            effects,
                            action: None,
                        },
                    ));
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
                    effects.push(record_effect(
                        &at,
                        RecordBody::Step {
                            op: op.clone(),
                            n: next,
                        },
                    ));
                    self.phase = Phase::AwaitingAssistant {
                        op,
                        step: next,
                        origin: Origin::Replay,
                    };
                    let action = Action::StreamAssistant {
                        request: self.request(),
                        model: self.config.model.clone(),
                        origin: Origin::Replay,
                    };
                    return Ok((
                        self,
                        Step::Do {
                            effects,
                            action: Some(action),
                        },
                    ));
                };
                if current.journal_start {
                    effects.extend(start_tool_effects(&op, &current.call, &at));
                }
                let action = Action::ExecuteTool {
                    call: current.call.clone(),
                    origin: Origin::Replay,
                };
                self.phase = Phase::AwaitingTool(AwaitingTool {
                    op,
                    step,
                    current,
                    remaining: calls,
                    after: AfterTools::Stream,
                    origin: Origin::Replay,
                });
                Ok((
                    self,
                    Step::Do {
                        effects,
                        action: Some(action),
                    },
                ))
            }
            StopReason::Paused => {
                let next = step + 1;
                effects.push(record_effect(
                    &at,
                    RecordBody::Step {
                        op: op.clone(),
                        n: next,
                    },
                ));
                self.phase = Phase::AwaitingAssistant {
                    op,
                    step: next,
                    origin: Origin::Replay,
                };
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
            StopReason::Aborted => {
                effects.extend(finish_effects(&op, OpOutcome::Aborted, &at));
                self.phase = Phase::Idle;
                Ok((
                    self,
                    Step::Do {
                        effects,
                        action: None,
                    },
                ))
            }
            StopReason::Refusal | StopReason::Error => {
                effects.extend(finish_effects(
                    &op,
                    OpOutcome::Failed {
                        error: format!("provider ended the generation with {:?}", message.stop),
                    },
                    &at,
                ));
                self.phase = Phase::Idle;
                Ok((
                    self,
                    Step::Do {
                        effects,
                        action: None,
                    },
                ))
            }
            _ => {
                effects.extend(finish_effects(
                    &op,
                    OpOutcome::Failed {
                        error: "provider returned an unsupported stop reason".to_owned(),
                    },
                    &at,
                ));
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

    /// Resolves the single action currently in flight.
    pub fn resolve(self, outcome: ActionOutcome) -> Result<(Self, Step), MachineError> {
        match (self.phase.clone(), outcome) {
            (
                Phase::AwaitingAssistant { op, step, origin },
                ActionOutcome::Assistant { result, stamp },
            ) => self.resolve_assistant(op, step, origin, result, stamp),
            (
                Phase::AwaitingTool(pending),
                ActionOutcome::Tool {
                    call_id,
                    content,
                    is_error,
                    details,
                    stamp,
                },
            ) => {
                if call_id != pending.current.call.call_id {
                    return Err(MachineError::MismatchedToolCall {
                        expected: pending.current.call.call_id,
                        actual: call_id,
                    });
                }
                self.resolve_tool(pending, content, is_error, details, stamp)
            }
            (
                Phase::AwaitingSummary {
                    op,
                    step: _,
                    work,
                    origin: _,
                },
                ActionOutcome::Summary { result, stamp },
            ) => self.resolve_summary(op, work, result, stamp),
            (Phase::Idle, _) => Err(MachineError::UnexpectedOutcome),
            (
                Phase::AwaitingAssistant { .. }
                | Phase::AwaitingTool(_)
                | Phase::AwaitingSummary { .. },
                _,
            ) => Err(MachineError::MismatchedOutcome),
        }
    }

    fn resolve_assistant(
        mut self,
        op: OpId,
        step: u32,
        origin: Origin,
        result: Result<AssistantMessage, ProviderError>,
        stamp: EntryStamp,
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
                return Ok(self.finish(op, outcome, &stamp.at));
            }
        };
        self.last_input_tokens = message.usage.input_tokens;
        let stored = SessionMessage::Assistant(message.clone());
        let entry = NewEntry {
            id: stamp.id,
            parent: self.leaf.clone(),
            lane: LaneName::main(),
            op: Some(op.clone()),
            at: stamp.at.clone(),
            body: EntryBody::Message {
                message: stored.clone(),
            },
        };
        self.remember_entry(&entry);
        let mut effects = vec![
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

        match message.stop {
            StopReason::Stop => {
                effects.extend(finish_effects(&op, OpOutcome::Completed, &stamp.at));
                self.phase = Phase::Idle;
                Ok((
                    self,
                    Step::Do {
                        effects,
                        action: None,
                    },
                ))
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
                    effects.extend(finish_effects(
                        &op,
                        OpOutcome::Failed {
                            error: error.to_owned(),
                        },
                        &stamp.at,
                    ));
                    self.phase = Phase::Idle;
                    return Ok((
                        self,
                        Step::Do {
                            effects,
                            action: None,
                        },
                    ));
                };
                effects.extend(start_tool_effects(&op, &current.call, &stamp.at));
                let action = Action::ExecuteTool {
                    call: current.call.clone(),
                    origin,
                };
                self.phase = Phase::AwaitingTool(AwaitingTool {
                    op,
                    step,
                    current,
                    remaining: calls,
                    after: AfterTools::Stream,
                    origin,
                });
                Ok((
                    self,
                    Step::Do {
                        effects,
                        action: Some(action),
                    },
                ))
            }
            StopReason::Paused => {
                let next = step + 1;
                effects.push(record_effect(
                    &stamp.at,
                    RecordBody::Step {
                        op: op.clone(),
                        n: next,
                    },
                ));
                self.phase = Phase::AwaitingAssistant {
                    op,
                    step: next,
                    origin,
                };
                let action = Action::StreamAssistant {
                    request: self.request(),
                    model: self.config.model.clone(),
                    origin,
                };
                Ok((
                    self,
                    Step::Do {
                        effects,
                        action: Some(action),
                    },
                ))
            }
            StopReason::Aborted => {
                effects.extend(finish_effects(&op, OpOutcome::Aborted, &stamp.at));
                self.phase = Phase::Idle;
                Ok((
                    self,
                    Step::Do {
                        effects,
                        action: None,
                    },
                ))
            }
            StopReason::Refusal | StopReason::Error => {
                let error = format!("provider ended the generation with {:?}", message.stop);
                effects.extend(finish_effects(&op, OpOutcome::Failed { error }, &stamp.at));
                self.phase = Phase::Idle;
                Ok((
                    self,
                    Step::Do {
                        effects,
                        action: None,
                    },
                ))
            }
            _ => {
                effects.extend(finish_effects(
                    &op,
                    OpOutcome::Failed {
                        error: "provider returned an unsupported stop reason".to_owned(),
                    },
                    &stamp.at,
                ));
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

    fn resolve_tool(
        mut self,
        pending: AwaitingTool,
        content: Vec<ContentBlock>,
        is_error: bool,
        details: Option<Value>,
        stamp: EntryStamp,
    ) -> Result<(Self, Step), MachineError> {
        let AwaitingTool {
            op,
            step,
            current,
            mut remaining,
            after,
            origin,
        } = pending;
        let message = SessionMessage::ToolResult {
            call_id: current.call.call_id,
            content,
            is_error,
            details,
        };
        let entry = NewEntry {
            id: stamp.id,
            parent: self.leaf.clone(),
            lane: LaneName::main(),
            op: Some(op.clone()),
            at: stamp.at.clone(),
            body: EntryBody::Message {
                message: message.clone(),
            },
        };
        self.remember_entry(&entry);
        let mut effects = vec![
            Effect::AppendEntry(entry),
            Effect::Emit(AgentEvent::MessageAppended {
                op: op.clone(),
                message,
            }),
        ];

        if let Some(next) = remaining.pop_front() {
            if next.journal_start {
                effects.extend(start_tool_effects(&op, &next.call, &stamp.at));
            }
            let action = Action::ExecuteTool {
                call: next.call.clone(),
                origin,
            };
            self.phase = Phase::AwaitingTool(AwaitingTool {
                op,
                step,
                current: next,
                remaining,
                after,
                origin,
            });
            return Ok((
                self,
                Step::Do {
                    effects,
                    action: Some(action),
                },
            ));
        }

        match after {
            AfterTools::Stream => {
                let next_step = step + 1;
                effects.push(record_effect(
                    &stamp.at,
                    RecordBody::Step {
                        op: op.clone(),
                        n: next_step,
                    },
                ));
                self.phase = Phase::AwaitingAssistant {
                    op,
                    step: next_step,
                    origin,
                };
                let action = Action::StreamAssistant {
                    request: self.request(),
                    model: self.config.model.clone(),
                    origin,
                };
                Ok((
                    self,
                    Step::Do {
                        effects,
                        action: Some(action),
                    },
                ))
            }
            AfterTools::Finish(outcome) => {
                effects.extend(finish_effects(&op, outcome, &stamp.at));
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

    fn resolve_summary(
        mut self,
        op: OpId,
        work: CompactionWork,
        result: Result<CompactionSummary, ProviderError>,
        stamp: EntryStamp,
    ) -> Result<(Self, Step), MachineError> {
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
                return Ok(self.finish(op, outcome, &stamp.at));
            }
        };
        let entry = NewEntry {
            id: stamp.id,
            parent: self.leaf.clone(),
            lane: LaneName::main(),
            op: Some(op.clone()),
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
        let mut effects = vec![
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
        effects.extend(finish_effects(&op, OpOutcome::Completed, &stamp.at));
        self.phase = Phase::Idle;
        Ok((
            self,
            Step::Do {
                effects,
                action: None,
            },
        ))
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

    fn remember_entry(&mut self, entry: &NewEntry) {
        self.leaf = Some(entry.id.clone());
        self.entries.push(Entry {
            seq: 0,
            id: entry.id.clone(),
            parent: entry.parent.clone(),
            lane: entry.lane.clone(),
            op: entry.op.clone(),
            at: entry.at.clone(),
            body: entry.body.clone(),
        });
        self.provider_messages = assemble_context(&self.entries)
            .expect("machine entries remain a valid root-to-leaf path")
            .messages;
    }

    fn finish(mut self, op: OpId, outcome: OpOutcome, at: &Timestamp) -> (Self, Step) {
        self.phase = Phase::Idle;
        (
            self,
            Step::Do {
                effects: finish_effects(&op, outcome, at),
                action: None,
            },
        )
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
            compaction: None,
        }
    }

    fn stamp(id: &str) -> EntryStamp {
        EntryStamp {
            id: EntryId::from(id),
            at: Timestamp::from("t"),
        }
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
    fn tool_calls_are_journaled_before_the_shell_action() {
        let machine = SessionMachine::new(config(), Vec::new()).unwrap();
        let (machine, _) = machine
            .handle(Input::Prompt {
                message: SessionMessage::user("read"),
                op: OpId::from("op"),
                stamp: stamp("user"),
                origin: Origin::External,
                host: None,
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
                        args: json!({"path": "x"}),
                    }],
                )),
                stamp: stamp("assistant"),
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
        assert!(matches!(action, Some(Action::ExecuteTool { .. })));

        let (_, Step::Do { action, .. }) = machine
            .resolve(ActionOutcome::Tool {
                call_id,
                content: vec![ContentBlock::text("contents")],
                is_error: false,
                details: None,
                stamp: stamp("result"),
            })
            .unwrap()
        else {
            panic!("expected next provider action");
        };
        assert!(matches!(action, Some(Action::StreamAssistant { .. })));
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
