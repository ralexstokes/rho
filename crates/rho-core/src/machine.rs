use std::collections::VecDeque;

use rho_ai::{
    AssistantMessage, ContentBlock, ErrorKind, ProviderError, Request, StopReason, ThinkingLevel,
    ToolCallId, ToolDefinition,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::{
    ContextError, Entry, EntryBody, EntryId, HostInfo, LaneName, LaneStatus, NewEntry, NewRecord,
    OpId, OpIntent, OpOutcome, Origin, RecordBody, ReplaySafety, SessionMessage, Timestamp,
    assemble_context,
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
    /// Available tools and their durability metadata.
    pub tools: Vec<ToolSpec>,
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
        result: Result<String, String>,
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
}

#[derive(Clone, Debug, PartialEq)]
enum Phase {
    Idle,
    AwaitingAssistant {
        op: OpId,
        step: u32,
        origin: Origin,
    },
    AwaitingTool {
        op: OpId,
        step: u32,
        current: PreparedToolCall,
        remaining: VecDeque<PreparedToolCall>,
        after: AfterTools,
        origin: Origin,
    },
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
}

impl SessionMachine {
    /// Creates a machine from one current root-to-leaf branch.
    pub fn new(config: MachineConfig, entries: Vec<Entry>) -> Result<Self, ContextError> {
        let context = assemble_context(&entries)?;
        let leaf = entries.last().map(|entry| entry.id.clone());
        Ok(Self {
            config,
            entries,
            provider_messages: context.messages,
            leaf,
            phase: Phase::Idle,
        })
    }

    /// Returns whether no action or operation is active.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.phase == Phase::Idle
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
                LaneStatus::Suspended(suspended) => {
                    let op = suspended.op;
                    let finish = suspended.abort_requested.then_some(OpOutcome::Aborted);
                    let mut calls = suspended
                        .open_tools
                        .into_iter()
                        .map(|tool| PreparedToolCall {
                            call_id: tool.call_id,
                            name: tool.name,
                            effective_args: tool.effective_args,
                            replay: tool.replay,
                            precomputed_error: (suspended.abort_requested
                                || tool.replay == ReplaySafety::Never)
                                .then(|| {
                                    if suspended.abort_requested {
                                        "interrupted after abort was requested".to_owned()
                                    } else {
                                        "interrupted; tool is not safe to re-run".to_owned()
                                    }
                                }),
                        })
                        .collect::<VecDeque<_>>();
                    if let Some(current) = calls.pop_front() {
                        let action = Action::ExecuteTool {
                            call: current.clone(),
                            origin: Origin::Replay,
                        };
                        self.phase = Phase::AwaitingTool {
                            op: op.clone(),
                            step: suspended.last_step.unwrap_or(0),
                            current,
                            remaining: calls,
                            after: finish.map_or(AfterTools::Stream, AfterTools::Finish),
                            origin: Origin::Replay,
                        };
                        return Ok((
                            self,
                            Step::Do {
                                effects: vec![Effect::Emit(AgentEvent::OperationStarted {
                                    op,
                                    origin: Origin::Replay,
                                })],
                                action: Some(action),
                            },
                        ));
                    }
                    if let Some(outcome) = finish {
                        return Ok(self.finish(op, outcome, &at));
                    }
                    let next = suspended.last_step.unwrap_or(0) + 1;
                    self.phase = Phase::AwaitingAssistant {
                        op: op.clone(),
                        step: next,
                        origin: Origin::Replay,
                    };
                    let effects = vec![
                        Effect::Emit(AgentEvent::OperationStarted {
                            op: op.clone(),
                            origin: Origin::Replay,
                        }),
                        record_effect(
                            &at,
                            RecordBody::Step {
                                op: op.clone(),
                                n: next,
                            },
                        ),
                    ];
                    let action = Action::StreamAssistant {
                        request: self.request(),
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

    /// Resolves the single action currently in flight.
    pub fn resolve(self, outcome: ActionOutcome) -> Result<(Self, Step), MachineError> {
        match (self.phase.clone(), outcome) {
            (
                Phase::AwaitingAssistant { op, step, origin },
                ActionOutcome::Assistant { result, stamp },
            ) => self.resolve_assistant(op, step, origin, result, stamp),
            (
                Phase::AwaitingTool {
                    op,
                    step,
                    current,
                    remaining,
                    after,
                    origin,
                },
                ActionOutcome::Tool {
                    call_id,
                    content,
                    is_error,
                    details,
                    stamp,
                },
            ) => {
                if call_id != current.call_id {
                    return Err(MachineError::MismatchedToolCall {
                        expected: current.call_id,
                        actual: call_id,
                    });
                }
                self.resolve_tool(
                    op, step, current, remaining, after, origin, content, is_error, details, stamp,
                )
            }
            (Phase::Idle, _) => Err(MachineError::UnexpectedOutcome),
            (Phase::AwaitingAssistant { .. } | Phase::AwaitingTool { .. }, _) => {
                Err(MachineError::MismatchedOutcome)
            }
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
                let mut calls = self.prepare_calls(&message, message.stop == StopReason::Length);
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
                effects.extend(start_tool_effects(&op, &current, &stamp.at));
                let action = Action::ExecuteTool {
                    call: current.clone(),
                    origin,
                };
                self.phase = Phase::AwaitingTool {
                    op,
                    step,
                    current,
                    remaining: calls,
                    after: AfterTools::Stream,
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

    #[allow(clippy::too_many_arguments)]
    fn resolve_tool(
        mut self,
        op: OpId,
        step: u32,
        current: PreparedToolCall,
        mut remaining: VecDeque<PreparedToolCall>,
        after: AfterTools,
        origin: Origin,
        content: Vec<ContentBlock>,
        is_error: bool,
        details: Option<Value>,
        stamp: EntryStamp,
    ) -> Result<(Self, Step), MachineError> {
        let message = SessionMessage::ToolResult {
            call_id: current.call_id,
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
            effects.extend(start_tool_effects(&op, &next, &stamp.at));
            let action = Action::ExecuteTool {
                call: next.clone(),
                origin,
            };
            self.phase = Phase::AwaitingTool {
                op,
                step,
                current: next,
                remaining,
                after,
                origin,
            };
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

    fn remember_entry(&mut self, entry: &NewEntry) {
        if let EntryBody::Message { message } = &entry.body {
            self.provider_messages.push(message.to_provider());
        }
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

    use super::*;

    fn config() -> MachineConfig {
        MachineConfig {
            system: "test".to_owned(),
            max_output_tokens: 100,
            thinking: ThinkingLevel::None,
            tools: vec![ToolSpec {
                definition: ToolDefinition::new("read", "read", json!({"type": "object"})),
                replay: ReplaySafety::Safe,
            }],
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
}
