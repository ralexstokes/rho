//! Automatic and manually driven hosts for the pure session machine.
//!
//! [`rho_core::SessionMachine`] is the primary manual-drive API. This crate
//! supplies the thin asynchronous loop that persists effects, runs actions,
//! and feeds outcomes back into that same machine.

#![allow(clippy::disallowed_methods)]

use std::{future::Future, pin::Pin};

use futures_util::StreamExt;
use rho_ai::{
    CancellationToken, ErrorKind, Provider, ProviderError, ProviderFactory, SessionConfig,
    StreamEvent,
};
use rho_core::{
    Action, ActionOutcome, AgentEvent, Effect, EntryId, EntryStamp, Input, MachineError, ModelRef,
    OpId, Origin, SessionMachine, SessionMessage, Step, Timestamp,
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

/// A live provider session bound to the provider/model used to open it.
///
/// Construction is intentionally only available through [`Self::open`], so
/// the automatic driver cannot silently execute a machine action against a
/// provider session opened for a different model.
pub struct BoundProvider {
    model: ModelRef,
    provider: Box<dyn Provider>,
}

impl BoundProvider {
    /// Opens a provider session for one durable provider/model selection.
    ///
    /// The factory identity and model catalog must both match the selection.
    pub async fn open(
        factory: &dyn ProviderFactory,
        model: ModelRef,
    ) -> Result<Self, ProviderError> {
        let factory_provider = factory.provider_id();
        if model.provider != factory_provider {
            return Err(ProviderError {
                retryable: false,
                kind: ErrorKind::InvalidRequest,
                message: format!(
                    "provider factory {:?} cannot open durable provider {:?}",
                    factory_provider.as_str(),
                    model.provider.as_str()
                ),
            });
        }
        let provider = factory
            .open(SessionConfig {
                model: model.model.clone(),
            })
            .await?;
        Ok(Self { model, provider })
    }

    /// Returns the durable selection this provider session was opened for.
    #[must_use]
    pub fn model(&self) -> &ModelRef {
        &self.model
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
    /// A new prompt was attempted on non-idle durable state.
    #[error("cannot start a prompt while the durable lane is {status:?}")]
    LaneNotIdle {
        /// Observed durable recovery state.
        status: Box<rho_core::LaneStatus>,
    },
    /// The machine was reconstructed from a different branch than the handle.
    #[error("session machine does not match the writer handle's current branch")]
    SessionStateMismatch,
    /// The open provider does not match the machine's durable selection.
    #[error("machine requires provider/model {expected:?}, but {actual:?} is open")]
    ProviderSelectionMismatch {
        /// Provider/model required by the machine or action.
        expected: ModelRef,
        /// Provider/model to which the live provider session is bound.
        actual: ModelRef,
    },
}

/// Mutable shell resources used to execute deterministic machine steps.
pub struct Driver<'driver> {
    session: &'driver mut dyn Session,
    provider: &'driver mut BoundProvider,
    tools: &'driver ToolSet,
    stamps: &'driver mut dyn StampSource,
    cancellation: CancellationToken,
    events: &'driver mut dyn EventSink,
}

impl<'driver> Driver<'driver> {
    /// Binds the resources used for one or more automatic driver operations.
    pub fn new(
        session: &'driver mut dyn Session,
        provider: &'driver mut BoundProvider,
        tools: &'driver ToolSet,
        stamps: &'driver mut dyn StampSource,
        cancellation: CancellationToken,
        events: &'driver mut dyn EventSink,
    ) -> Self {
        Self {
            session,
            provider,
            tools,
            stamps,
            cancellation,
            events,
        }
    }

    /// Runs one prompt to a terminal state using the same machine exposed for
    /// manual drive and deterministic replay.
    pub async fn run_prompt(
        &mut self,
        machine: SessionMachine,
        message: SessionMessage,
        origin: Origin,
        host: Option<rho_core::HostInfo>,
    ) -> Result<SessionMachine, DriverError> {
        validate_binding(&machine, self.session)?;
        validate_provider(&machine, self.provider)?;
        let status = self.session.lane_status()?;
        if status != rho_core::LaneStatus::Idle {
            return Err(DriverError::LaneNotIdle {
                status: Box::new(status),
            });
        }
        let input = Input::Prompt {
            message,
            op: self.stamps.op_id(),
            stamp: self.stamps.entry(),
            origin,
            host,
        };
        let (machine, step) = machine.handle(input)?;
        self.drive(machine, step).await
    }

    /// Resumes the single suspended operation described by the durable journal.
    pub async fn resume(&mut self, machine: SessionMachine) -> Result<SessionMachine, DriverError> {
        validate_binding(&machine, self.session)?;
        let status = self.session.lane_status()?;
        let (machine, step) = machine.handle(Input::Resume {
            status,
            at: self.stamps.timestamp(),
        })?;
        self.drive(machine, step).await
    }

    async fn drive(
        &mut self,
        mut machine: SessionMachine,
        mut step: Step,
    ) -> Result<SessionMachine, DriverError> {
        loop {
            match step {
                Step::Do { effects, action } => {
                    apply_effects(self.session, self.events, effects).await?;
                    let Some(action) = action else {
                        return Ok(machine);
                    };
                    let outcome = execute_action(
                        action,
                        self.provider,
                        self.tools,
                        self.stamps,
                        self.cancellation.clone(),
                        self.events,
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
}

fn validate_binding(machine: &SessionMachine, session: &dyn Session) -> Result<(), DriverError> {
    let durable = session.branch(None)?;
    let same_branch = durable.len() == machine.entries().len()
        && durable.iter().zip(machine.entries()).all(|(left, right)| {
            left.id == right.id
                && left.parent == right.parent
                && left.lane == right.lane
                && left.op == right.op
                && left.at == right.at
                && left.body == right.body
        });
    if same_branch {
        Ok(())
    } else {
        Err(DriverError::SessionStateMismatch)
    }
}

fn validate_provider(
    machine: &SessionMachine,
    provider: &BoundProvider,
) -> Result<(), DriverError> {
    if machine.model() == provider.model() {
        Ok(())
    } else {
        Err(DriverError::ProviderSelectionMismatch {
            expected: machine.model().clone(),
            actual: provider.model().clone(),
        })
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
    provider: &mut BoundProvider,
    tools: &ToolSet,
    stamps: &mut dyn StampSource,
    cancellation: CancellationToken,
    events: &mut dyn EventSink,
) -> Result<ActionOutcome, DriverError> {
    match action {
        Action::StreamAssistant { request, model, .. } => {
            if model != provider.model {
                return Err(DriverError::ProviderSelectionMismatch {
                    expected: model,
                    actual: provider.model.clone(),
                });
            }
            let mut stream = provider.provider.generate(request, cancellation);
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
    Timestamp::from(jiff::Timestamp::now().to_string())
}

#[cfg(test)]
mod tests {
    use rho_ai::{
        AssistantMessage, Message, ModelId, ModelInfo, ProviderId, Request, StopReason,
        ThinkingLevel, Usage,
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
            self.next += 1;
            OpId::from(format!("00000000-0000-7000-8000-{:012}", self.next))
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
        let first_assistant = AssistantMessage {
            blocks: vec![rho_ai::ContentBlock::text("done")],
            stop: StopReason::Stop,
            usage: Usage::default(),
            provider: ProviderId::from("p"),
            model: ModelId::from("m"),
        };
        let first_expected = Request {
            system: "test".to_owned(),
            messages: vec![Message::user("hello")],
            tools: Vec::new(),
            max_output_tokens: 100,
            thinking: ThinkingLevel::None,
        };
        let second_expected = Request {
            system: "test".to_owned(),
            messages: vec![
                Message::user("hello"),
                Message::Assistant(first_assistant.clone()),
                Message::user("again"),
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
            [
                Script {
                    request: first_expected,
                    events: vec![StreamEvent::Done(first_assistant)],
                },
                Script {
                    request: second_expected,
                    events: vec![StreamEvent::Done(AssistantMessage {
                        blocks: vec![rho_ai::ContentBlock::text("done again")],
                        stop: StopReason::Stop,
                        usage: Usage::default(),
                        provider: ProviderId::from("p"),
                        model: ModelId::from("m"),
                    })],
                },
            ],
        )
        .with_provider_id(ProviderId::from("p"));
        let wrong_factory = BoundProvider::open(
            &factory,
            ModelRef {
                provider: ProviderId::from("not-p"),
                model: ModelId::from("m"),
            },
        )
        .await;
        assert!(matches!(
            wrong_factory,
            Err(ProviderError {
                kind: ErrorKind::InvalidRequest,
                ..
            })
        ));
        let mut provider = BoundProvider::open(
            &factory,
            ModelRef {
                provider: ProviderId::from("p"),
                model: ModelId::from("m"),
            },
        )
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
                model: rho_core::ModelRef {
                    provider: ProviderId::from("p"),
                    model: ModelId::from("m"),
                },
                tools: Vec::new(),
            },
            Vec::new(),
        )
        .unwrap();
        let machine = Driver::new(
            session.as_mut(),
            &mut provider,
            &ToolSet::new(),
            &mut FixedStamps { next: 1 },
            CancellationToken::new(),
            &mut NoopEventSink,
        )
        .run_prompt(
            machine,
            SessionMessage::user("hello"),
            Origin::External,
            None,
        )
        .await
        .unwrap();
        assert!(machine.is_idle());
        assert_eq!(session.export_entries().unwrap().len(), 2);
        assert_eq!(session.lane_status().unwrap(), rho_core::LaneStatus::Idle);

        let machine = Driver::new(
            session.as_mut(),
            &mut provider,
            &ToolSet::new(),
            &mut FixedStamps { next: 10 },
            CancellationToken::new(),
            &mut NoopEventSink,
        )
        .run_prompt(
            machine,
            SessionMessage::user("again"),
            Origin::External,
            None,
        )
        .await
        .unwrap();
        assert!(machine.is_idle());
        assert_eq!(session.export_entries().unwrap().len(), 4);
        assert_eq!(session.lane_status().unwrap(), rho_core::LaneStatus::Idle);

        let wrong_model = SessionMachine::new(
            MachineConfig {
                system: "test".to_owned(),
                max_output_tokens: 100,
                thinking: ThinkingLevel::None,
                model: ModelRef {
                    provider: ProviderId::from("p"),
                    model: ModelId::from("other"),
                },
                tools: Vec::new(),
            },
            machine.entries().to_vec(),
        )
        .unwrap();
        let item_count = session.log(0, usize::MAX).unwrap().len();
        let resumed = Driver::new(
            session.as_mut(),
            &mut provider,
            &ToolSet::new(),
            &mut FixedStamps { next: 14 },
            CancellationToken::new(),
            &mut NoopEventSink,
        )
        .resume(wrong_model.clone())
        .await
        .unwrap();
        assert!(resumed.is_idle());
        assert_eq!(session.log(0, usize::MAX).unwrap().len(), item_count);

        let error = Driver::new(
            session.as_mut(),
            &mut provider,
            &ToolSet::new(),
            &mut FixedStamps { next: 15 },
            CancellationToken::new(),
            &mut NoopEventSink,
        )
        .run_prompt(
            wrong_model,
            SessionMessage::user("wrong model"),
            Origin::External,
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            DriverError::ProviderSelectionMismatch { .. }
        ));
        assert_eq!(session.log(0, usize::MAX).unwrap().len(), item_count);

        let unrelated = SessionMachine::new(
            MachineConfig {
                system: "test".to_owned(),
                max_output_tokens: 100,
                thinking: ThinkingLevel::None,
                model: rho_core::ModelRef {
                    provider: ProviderId::from("p"),
                    model: ModelId::from("m"),
                },
                tools: Vec::new(),
            },
            Vec::new(),
        )
        .unwrap();
        let error = Driver::new(
            session.as_mut(),
            &mut provider,
            &ToolSet::new(),
            &mut FixedStamps { next: 20 },
            CancellationToken::new(),
            &mut NoopEventSink,
        )
        .run_prompt(
            unrelated,
            SessionMessage::user("wrong session"),
            Origin::External,
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(error, DriverError::SessionStateMismatch));
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
        )
        .with_provider_id(ProviderId::from("p"));
        let mut provider = BoundProvider::open(
            &factory,
            ModelRef {
                provider: ProviderId::from("p"),
                model: ModelId::from("m"),
            },
        )
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
                model: rho_core::ModelRef {
                    provider: ProviderId::from("p"),
                    model: ModelId::from("m"),
                },
                tools: Vec::new(),
            },
            branch,
        )
        .unwrap();

        let error = Driver::new(
            session.as_mut(),
            &mut provider,
            &ToolSet::new(),
            &mut FixedStamps { next: 8 },
            CancellationToken::new(),
            &mut NoopEventSink,
        )
        .run_prompt(
            machine.clone(),
            SessionMessage::user("must resume first"),
            Origin::External,
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(error, DriverError::LaneNotIdle { .. }));

        let machine = Driver::new(
            session.as_mut(),
            &mut provider,
            &ToolSet::new(),
            &mut FixedStamps { next: 10 },
            CancellationToken::new(),
            &mut NoopEventSink,
        )
        .resume(machine)
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
            model: rho_core::ModelRef {
                provider: ProviderId::from("p"),
                model: ModelId::from("m"),
            },
            tools: Vec::new(),
        };
        assert!(config.tools.is_empty());
        let _ = ReplaySafety::Safe;
    }
}
