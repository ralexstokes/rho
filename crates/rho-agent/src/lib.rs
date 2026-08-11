//! Automatic and manually driven hosts for the pure session machine.
//!
//! [`rho_core::SessionMachine`] is the primary manual-drive API. This crate
//! supplies the thin asynchronous loop that persists effects, runs actions,
//! and feeds outcomes back into that same machine.

#![allow(clippy::disallowed_methods)]

use std::{
    future::Future,
    pin::Pin,
    time::{SystemTime, UNIX_EPOCH},
};

use futures_util::StreamExt;
use rho_ai::{CancellationToken, Provider, StreamEvent};
use rho_core::{
    Action, ActionOutcome, AgentEvent, Effect, EntryId, EntryStamp, Input, MachineError, OpId,
    Origin, SessionMachine, SessionMessage, Step, Timestamp,
};
use rho_store::{Session, SessionError};
use rho_tools::{ToolOutput, ToolSet};
use thiserror::Error;
use uuid::Uuid;

/// Type-erased event-sink operation.
pub type EventFuture<'sink> = Pin<Box<dyn Future<Output = ()> + Send + 'sink>>;

/// Awaited observer for deterministic and advisory agent events.
pub trait EventSink: Send {
    /// Publishes one event with listener-selected backpressure.
    fn emit(&mut self, event: AgentEvent) -> EventFuture<'_>;
}

/// Event sink that discards all advisory output.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopEventSink;

impl EventSink for NoopEventSink {
    fn emit(&mut self, _: AgentEvent) -> EventFuture<'_> {
        Box::pin(async {})
    }
}

/// Shell source for pre-minted deterministic transition stamps.
pub trait StampSource {
    /// Mints a UUIDv7 operation identity.
    fn op_id(&mut self) -> OpId;

    /// Mints a UUIDv7 entry identity and current UTC timestamp.
    fn entry(&mut self) -> EntryStamp;

    /// Mints a current UTC timestamp for a non-entry effect.
    fn timestamp(&mut self) -> Timestamp;
}

/// System clock and UUIDv7 stamp source for automatic hosts.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemStamps;

impl StampSource for SystemStamps {
    fn op_id(&mut self) -> OpId {
        OpId::from(Uuid::now_v7().to_string())
    }

    fn entry(&mut self) -> EntryStamp {
        EntryStamp {
            id: EntryId::from(Uuid::now_v7().to_string()),
            at: system_timestamp(),
        }
    }

    fn timestamp(&mut self) -> Timestamp {
        system_timestamp()
    }
}

/// Automatic driver failure outside the modeled operation result.
#[derive(Debug, Error)]
pub enum DriverError {
    /// Pure state transition was invalid.
    #[error("invalid session-machine transition: {0}")]
    Machine(#[from] MachineError),
    /// A durable effect could not be stored.
    #[error("could not persist session effect: {0}")]
    Store(#[from] SessionError),
    /// Provider stream violated the terminal-event contract.
    #[error("provider stream ended without Done or Error")]
    UnterminatedProviderStream,
    /// This first driver slice does not yet host hooks, interactions, or compaction.
    #[error("automatic driver does not support action {0}")]
    UnsupportedAction(&'static str),
    /// Recovery found work that requires explicit resume choreography.
    #[error("suspended operation requires resume choreography")]
    Suspended,
}

/// Runs one prompt to a terminal state using the same machine exposed for
/// manual drive and deterministic replay.
#[allow(clippy::too_many_arguments)]
pub async fn run_prompt(
    machine: SessionMachine,
    session: &mut dyn Session,
    provider: &mut dyn Provider,
    tools: &ToolSet,
    message: SessionMessage,
    origin: Origin,
    host: Option<rho_core::HostInfo>,
    stamps: &mut dyn StampSource,
    cancellation: CancellationToken,
    events: &mut dyn EventSink,
) -> Result<SessionMachine, DriverError> {
    let input = Input::Prompt {
        message,
        op: stamps.op_id(),
        stamp: stamps.entry(),
        origin,
        host,
    };
    let (machine, step) = machine.handle(input)?;
    drive(
        machine,
        step,
        session,
        provider,
        tools,
        stamps,
        cancellation,
        events,
    )
    .await
}

/// Resumes the single suspended operation described by the durable journal.
#[allow(clippy::too_many_arguments)]
pub async fn resume(
    machine: SessionMachine,
    session: &mut dyn Session,
    provider: &mut dyn Provider,
    tools: &ToolSet,
    stamps: &mut dyn StampSource,
    cancellation: CancellationToken,
    events: &mut dyn EventSink,
) -> Result<SessionMachine, DriverError> {
    let status = session.lane_status()?;
    let (machine, step) = machine.handle(Input::Resume {
        status,
        at: stamps.timestamp(),
    })?;
    drive(
        machine,
        step,
        session,
        provider,
        tools,
        stamps,
        cancellation,
        events,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn drive(
    mut machine: SessionMachine,
    mut step: Step,
    session: &mut dyn Session,
    provider: &mut dyn Provider,
    tools: &ToolSet,
    stamps: &mut dyn StampSource,
    cancellation: CancellationToken,
    events: &mut dyn EventSink,
) -> Result<SessionMachine, DriverError> {
    loop {
        match step {
            Step::Do { effects, action } => {
                apply_effects(session, events, effects).await?;
                let Some(action) = action else {
                    return Ok(machine);
                };
                let outcome = execute_action(
                    action,
                    provider,
                    tools,
                    stamps,
                    cancellation.clone(),
                    events,
                )
                .await?;
                (machine, step) = machine.resolve(outcome)?;
            }
            Step::Idle => return Ok(machine),
            Step::AwaitingOutcome => return Err(DriverError::Suspended),
            _ => return Err(DriverError::UnsupportedAction("future step variant")),
        }
    }
}

async fn apply_effects(
    session: &mut dyn Session,
    events: &mut dyn EventSink,
    effects: Vec<Effect>,
) -> Result<(), DriverError> {
    for effect in effects {
        match effect {
            Effect::AppendEntry(entry) => {
                session.append_entry(entry)?;
            }
            Effect::AppendRecord(record) => {
                session.append_record(record)?;
            }
            Effect::Emit(event) => events.emit(event).await,
            _ => return Err(DriverError::UnsupportedAction("future effect variant")),
        }
    }
    Ok(())
}

async fn execute_action(
    action: Action,
    provider: &mut dyn Provider,
    tools: &ToolSet,
    stamps: &mut dyn StampSource,
    cancellation: CancellationToken,
    events: &mut dyn EventSink,
) -> Result<ActionOutcome, DriverError> {
    match action {
        Action::StreamAssistant { request, .. } => {
            let mut stream = provider.generate(request, cancellation);
            while let Some(event) = stream.next().await {
                events
                    .emit(AgentEvent::ProviderStream {
                        event: event.clone(),
                    })
                    .await;
                match event {
                    StreamEvent::Done(message) => {
                        return Ok(ActionOutcome::Assistant {
                            result: Ok(message),
                            stamp: stamps.entry(),
                        });
                    }
                    StreamEvent::Error(error) => {
                        return Ok(ActionOutcome::Assistant {
                            result: Err(error),
                            stamp: stamps.entry(),
                        });
                    }
                    StreamEvent::Start
                    | StreamEvent::Delta { .. }
                    | StreamEvent::BlockDone { .. } => {}
                    _ => {}
                }
            }
            Err(DriverError::UnterminatedProviderStream)
        }
        Action::ExecuteTool { call, .. } => {
            let output = if let Some(error) = call.precomputed_error {
                ToolOutput::error(error)
            } else if let Some(tool) = tools.get(&call.name) {
                tool.execute(call.effective_args, cancellation).await
            } else {
                ToolOutput::error(format!("unknown tool {:?}", call.name))
            };
            Ok(ActionOutcome::Tool {
                call_id: call.call_id,
                content: output.content,
                is_error: output.is_error,
                details: output.details,
                stamp: stamps.entry(),
            })
        }
        Action::Summarize { .. } => Err(DriverError::UnsupportedAction("summarize")),
        Action::AwaitInteraction { .. } => Err(DriverError::UnsupportedAction("await_interaction")),
        Action::InvokeHook { .. } => Err(DriverError::UnsupportedAction("invoke_hook")),
        _ => Err(DriverError::UnsupportedAction("future action variant")),
    }
}

fn system_timestamp() -> Timestamp {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = i64::try_from(duration.as_secs()).unwrap_or(i64::MAX);
    let days = seconds.div_euclid(86_400);
    let day_seconds = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = day_seconds / 3_600;
    let minute = day_seconds % 3_600 / 60;
    let second = day_seconds % 60;
    let nanos = duration.subsec_nanos();
    Timestamp::from(if nanos == 0 {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
    } else {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{nanos:09}Z")
    })
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use rho_ai::{
        AssistantMessage, Message, ModelId, ModelInfo, ProviderFactory, ProviderId, Request,
        SessionConfig, StopReason, ThinkingLevel, Usage,
        faux::{FauxFactory, Script},
    };
    use rho_core::{EntryStamp, MachineConfig, ReplaySafety};
    use rho_store::{CreateOptions, MemoryRepo, SessionRepo};

    use super::*;

    struct FixedStamps {
        next: u64,
    }

    impl StampSource for FixedStamps {
        fn op_id(&mut self) -> OpId {
            OpId::from("00000000-0000-7000-8000-000000000001")
        }

        fn entry(&mut self) -> EntryStamp {
            self.next += 1;
            EntryStamp {
                id: EntryId::from(format!("00000000-0000-7000-8000-{:012}", self.next)),
                at: Timestamp::from(format!("t{}", self.next)),
            }
        }

        fn timestamp(&mut self) -> Timestamp {
            self.next += 1;
            Timestamp::from(format!("t{}", self.next))
        }
    }

    #[tokio::test]
    async fn automatic_driver_persists_the_same_machine_effects() {
        let expected = Request {
            system: "test".to_owned(),
            messages: vec![Message::user("hello")],
            tools: Vec::new(),
            max_output_tokens: 100,
            thinking: ThinkingLevel::None,
        };
        let factory = FauxFactory::new(
            vec![ModelInfo {
                id: ModelId::from("m"),
                display_name: "model".to_owned(),
                context_tokens: None,
                max_output_tokens: None,
            }],
            [Script {
                request: expected,
                events: vec![StreamEvent::Done(AssistantMessage {
                    blocks: vec![rho_ai::ContentBlock::text("done")],
                    stop: StopReason::Stop,
                    usage: Usage::default(),
                    provider: ProviderId::from("p"),
                    model: ModelId::from("m"),
                })],
            }],
        );
        let mut provider = factory
            .open(SessionConfig {
                model: ModelId::from("m"),
            })
            .await
            .unwrap();
        let repo = MemoryRepo::default();
        let mut session = repo
            .create(CreateOptions {
                cwd: "/workspace".to_owned(),
            })
            .await
            .unwrap();
        let machine = SessionMachine::new(
            MachineConfig {
                system: "test".to_owned(),
                max_output_tokens: 100,
                thinking: ThinkingLevel::None,
                tools: Vec::new(),
            },
            Vec::new(),
        )
        .unwrap();
        let machine = run_prompt(
            machine,
            session.as_mut(),
            provider.as_mut(),
            &ToolSet::new(),
            SessionMessage::user("hello"),
            Origin::External,
            None,
            &mut FixedStamps { next: 1 },
            CancellationToken::new(),
            &mut NoopEventSink,
        )
        .await
        .unwrap();
        assert!(machine.is_idle());
        assert_eq!(session.export_entries().unwrap().len(), 2);
        assert_eq!(session.lane_status().unwrap(), rho_core::LaneStatus::Idle);
    }

    #[tokio::test]
    async fn resume_synthesizes_unsafe_tool_failure_then_rebases_provider() {
        let call_id = rho_ai::ToolCallId::from("call");
        let interrupted_assistant = AssistantMessage {
            blocks: vec![rho_ai::ContentBlock::ToolCall {
                id: call_id.clone(),
                name: "write".to_owned(),
                args: serde_json::json!({"path": "x"}),
            }],
            stop: StopReason::ToolUse,
            usage: Usage::default(),
            provider: ProviderId::from("p"),
            model: ModelId::from("m"),
        };
        let expected = Request {
            system: "test".to_owned(),
            messages: vec![
                Message::user("change it"),
                Message::Assistant(interrupted_assistant.clone()),
                Message::ToolResult(rho_ai::ToolResult {
                    call_id: call_id.clone(),
                    content: "interrupted; tool is not safe to re-run".to_owned(),
                    is_error: true,
                }),
            ],
            tools: Vec::new(),
            max_output_tokens: 100,
            thinking: ThinkingLevel::None,
        };
        let factory = FauxFactory::new(
            vec![ModelInfo {
                id: ModelId::from("m"),
                display_name: "model".to_owned(),
                context_tokens: None,
                max_output_tokens: None,
            }],
            [Script {
                request: expected,
                events: vec![StreamEvent::Done(AssistantMessage {
                    blocks: vec![rho_ai::ContentBlock::text("recovered")],
                    stop: StopReason::Stop,
                    usage: Usage::default(),
                    provider: ProviderId::from("p"),
                    model: ModelId::from("m"),
                })],
            }],
        );
        let mut provider = factory
            .open(SessionConfig {
                model: ModelId::from("m"),
            })
            .await
            .unwrap();
        let repo = MemoryRepo::default();
        let mut session = repo
            .create(CreateOptions {
                cwd: "/workspace".to_owned(),
            })
            .await
            .unwrap();
        let op = OpId::from("00000000-0000-7000-8000-000000000001");
        let user_id = EntryId::from("00000000-0000-7000-8000-000000000002");
        session
            .append_entry(rho_core::NewEntry {
                id: user_id.clone(),
                parent: None,
                lane: rho_core::LaneName::main(),
                op: Some(op.clone()),
                at: Timestamp::from("t1"),
                body: rho_core::EntryBody::Message {
                    message: SessionMessage::user("change it"),
                },
            })
            .unwrap();
        session
            .append_record(rho_core::NewRecord {
                lane: rho_core::LaneName::main(),
                at: Timestamp::from("t1"),
                body: rho_core::RecordBody::OpStarted {
                    op: op.clone(),
                    intent: rho_core::OpIntent::Run,
                    origin: Origin::External,
                    host: None,
                },
            })
            .unwrap();
        session
            .append_record(rho_core::NewRecord {
                lane: rho_core::LaneName::main(),
                at: Timestamp::from("t2"),
                body: rho_core::RecordBody::Step {
                    op: op.clone(),
                    n: 1,
                },
            })
            .unwrap();
        session
            .append_entry(rho_core::NewEntry {
                id: EntryId::from("00000000-0000-7000-8000-000000000003"),
                parent: Some(user_id),
                lane: rho_core::LaneName::main(),
                op: Some(op.clone()),
                at: Timestamp::from("t3"),
                body: rho_core::EntryBody::Message {
                    message: SessionMessage::Assistant(interrupted_assistant),
                },
            })
            .unwrap();
        session
            .append_record(rho_core::NewRecord {
                lane: rho_core::LaneName::main(),
                at: Timestamp::from("t4"),
                body: rho_core::RecordBody::ToolStarted {
                    op,
                    call_id: call_id.clone(),
                    name: "write".to_owned(),
                    effective_args: serde_json::json!({"path": "x"}),
                    replay: rho_core::ReplaySafety::Never,
                },
            })
            .unwrap();
        let branch = session.branch(None).unwrap();
        let machine = SessionMachine::new(
            MachineConfig {
                system: "test".to_owned(),
                max_output_tokens: 100,
                thinking: ThinkingLevel::None,
                tools: Vec::new(),
            },
            branch,
        )
        .unwrap();

        let machine = resume(
            machine,
            session.as_mut(),
            provider.as_mut(),
            &ToolSet::new(),
            &mut FixedStamps { next: 10 },
            CancellationToken::new(),
            &mut NoopEventSink,
        )
        .await
        .unwrap();
        assert!(machine.is_idle());
        assert_eq!(session.lane_status().unwrap(), rho_core::LaneStatus::Idle);
        assert!(
            session
                .export_entries()
                .unwrap()
                .iter()
                .any(|entry| matches!(
                    &entry.body,
                    rho_core::EntryBody::Message {
                        message: SessionMessage::ToolResult {
                            call_id: result_call,
                            is_error: true,
                            ..
                        }
                    } if result_call == &call_id
                ))
        );
    }

    #[test]
    fn tool_specs_are_usable_without_builtin_tools() {
        let config = MachineConfig {
            system: String::new(),
            max_output_tokens: 1,
            thinking: ThinkingLevel::None,
            tools: Vec::new(),
        };
        assert!(config.tools.is_empty());
        let _ = ReplaySafety::Safe;
    }
}
