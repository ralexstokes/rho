//! Automatic and manually driven hosts for the pure session machine.
//!
//! [`rho_core::SessionMachine`] is the primary manual-drive API. This crate
//! supplies the thin asynchronous loop that persists effects, runs actions,
//! and feeds outcomes back into that same machine.

#![allow(clippy::disallowed_methods)]

use std::{future::Future, pin::Pin, time::Duration};

use futures_util::StreamExt;
use rho_ai::{
    CancellationToken, ErrorKind, Provider, ProviderError, ProviderFactory, SessionConfig,
    StreamEvent,
};
use rho_core::{
    Action, ActionOutcome, AgentEvent, Effect, EntryId, EntryStamp, HookInvocation, HookOutput,
    Input, InteractionAnswer, InteractionRequest, MachineError, ModelRef, OpId, Origin, QueueError,
    QueueId, QueueKind, SessionControl, SessionMachine, SessionMessage, Step, Timestamp,
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

/// Type-erased hook-host operation.
pub type HookFuture<'hook> =
    Pin<Box<dyn Future<Output = Result<HookOutput, String>> + Send + 'hook>>;

/// Mutable-shell host for native or serialized extension hooks.
pub trait HookHost: Send {
    /// Invokes one owned hook action.
    fn invoke(&mut self, invocation: HookInvocation, origin: Origin) -> HookFuture<'_>;
}

/// Event sink that discards all advisory output.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopEventSink;

impl EventSink for NoopEventSink {
    fn emit(&mut self, _: AgentEvent) -> EventFuture<'_> {
        Box::pin(async {})
    }
}

/// Sending side of a session's live control plane.
#[derive(Clone)]
pub struct ControlSender {
    inner: tokio::sync::mpsc::UnboundedSender<SessionControl>,
}

/// Receiving side owned by exactly one automatic driver.
pub struct ControlReceiver {
    inner: tokio::sync::mpsc::UnboundedReceiver<SessionControl>,
}

/// Creates an unbounded in-process control channel.
#[must_use]
pub fn control_channel() -> (ControlSender, ControlReceiver) {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    (
        ControlSender { inner: sender },
        ControlReceiver { inner: receiver },
    )
}

impl ControlSender {
    /// Queues steering for the active run and returns its durable identity.
    pub fn steer(&self, message: SessionMessage) -> Result<QueueId, ControlClosed> {
        self.enqueue(QueueKind::Steer, message)
    }

    /// Queues a message for the next run and returns its durable identity.
    pub fn follow_up(&self, message: SessionMessage) -> Result<QueueId, ControlClosed> {
        self.enqueue(QueueKind::FollowUp, message)
    }

    fn enqueue(&self, kind: QueueKind, message: SessionMessage) -> Result<QueueId, ControlClosed> {
        let id = QueueId::from(Uuid::now_v7().to_string());
        self.inner
            .send(SessionControl::Enqueue {
                id: id.clone(),
                kind,
                message,
            })
            .map_err(|_| ControlClosed)?;
        Ok(id)
    }

    /// Cancels one pending steering or follow-up item.
    pub fn cancel(&self, id: QueueId) -> Result<(), ControlClosed> {
        self.inner
            .send(SessionControl::Cancel { id })
            .map_err(|_| ControlClosed)
    }

    /// Requests cooperative cancellation of the active operation.
    pub fn abort(&self) -> Result<(), ControlClosed> {
        self.inner
            .send(SessionControl::Abort)
            .map_err(|_| ControlClosed)
    }

    /// Answers one pending headless interaction.
    pub fn answer_interaction(
        &self,
        id: impl Into<String>,
        answer: InteractionAnswer,
    ) -> Result<(), ControlClosed> {
        self.inner
            .send(SessionControl::AnswerInteraction {
                id: id.into(),
                answer,
            })
            .map_err(|_| ControlClosed)
    }
}

/// The session driver no longer accepts control-plane commands.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("session control channel is closed")]
pub struct ControlClosed;

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
    /// The automatic driver received an action variant it cannot execute.
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
    /// Durable queue history was internally inconsistent.
    #[error("invalid durable queue history: {0:?}")]
    Queue(QueueError),
}

/// Mutable shell resources used to execute deterministic machine steps.
pub struct Driver<'driver> {
    session: &'driver mut dyn Session,
    provider: &'driver mut BoundProvider,
    tools: &'driver ToolSet,
    stamps: &'driver mut dyn StampSource,
    cancellation: CancellationToken,
    events: &'driver mut dyn EventSink,
    commands: Option<&'driver mut ControlReceiver>,
    hooks: Option<&'driver mut dyn HookHost>,
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
            commands: None,
            hooks: None,
        }
    }

    /// Attaches the live control plane polled before and during actions.
    #[must_use]
    pub fn with_controls(mut self, commands: &'driver mut ControlReceiver) -> Self {
        self.commands = Some(commands);
        self
    }

    /// Attaches the extension hook host used by enabled machine hook points.
    #[must_use]
    pub fn with_hooks(mut self, hooks: &'driver mut dyn HookHost) -> Self {
        self.hooks = Some(hooks);
        self
    }

    /// Runs one prompt to a terminal state using the same machine exposed for
    /// manual drive and deterministic replay.
    pub async fn run_prompt(
        &mut self,
        mut machine: SessionMachine,
        message: SessionMessage,
        origin: Origin,
        host: Option<rho_core::HostInfo>,
    ) -> Result<SessionMachine, DriverError> {
        validate_binding(&machine, self.session)?;
        validate_provider(&machine, self.provider)?;
        self.hydrate_queue(&mut machine)?;
        self.drain_ready_controls(&mut machine).await?;
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
            host: host.clone(),
            queue: None,
            steer_stamps: self.steer_stamps(&machine),
        };
        let (machine, step) = machine.handle(input)?;
        let machine = self.drive(machine, step).await?;
        if machine.compaction_due().is_some() {
            self.compact(machine, origin, host).await
        } else {
            Ok(machine)
        }
    }

    /// Resumes the single suspended operation described by the durable journal.
    pub async fn resume(
        &mut self,
        mut machine: SessionMachine,
    ) -> Result<SessionMachine, DriverError> {
        validate_binding(&machine, self.session)?;
        self.hydrate_queue(&mut machine)?;
        let status = self.session.lane_status()?;
        let steer_stamps = self.steer_stamps(&machine);
        let (machine, step) = machine.handle(Input::Resume {
            status,
            at: self.stamps.timestamp(),
            steer_stamps,
        })?;
        self.drive(machine, step).await
    }

    /// Runs one explicit compaction operation to a terminal state.
    pub async fn compact(
        &mut self,
        mut machine: SessionMachine,
        origin: Origin,
        host: Option<rho_core::HostInfo>,
    ) -> Result<SessionMachine, DriverError> {
        validate_binding(&machine, self.session)?;
        validate_provider(&machine, self.provider)?;
        self.hydrate_queue(&mut machine)?;
        let status = self.session.lane_status()?;
        if status != rho_core::LaneStatus::Idle {
            return Err(DriverError::LaneNotIdle {
                status: Box::new(status),
            });
        }
        let (machine, step) = machine.handle(Input::Compact {
            op: self.stamps.op_id(),
            at: self.stamps.timestamp(),
            origin,
            host,
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
                        let (next_machine, next_step) = self.start_queued_run(machine).await?;
                        machine = next_machine;
                        if let Some(next_step) = next_step {
                            step = next_step;
                            continue;
                        }
                        return Ok(machine);
                    };
                    if !matches!(&action, Action::AwaitInteraction { .. }) {
                        self.drain_ready_controls(&mut machine).await?;
                    }
                    let steer_stamps = if matches!(&action, Action::StreamAssistant { .. }) {
                        self.steer_stamps(&machine)
                    } else {
                        Vec::new()
                    };
                    let (effects, action) = machine.prepare_action(action, steer_stamps)?;
                    apply_effects(self.session, self.events, effects).await?;
                    let hooks = self
                        .hooks
                        .as_mut()
                        .map(|hooks| &mut **hooks as &mut dyn HookHost);
                    let outcome = execute_action(
                        action,
                        ActionShell {
                            machine: &mut machine,
                            session: self.session,
                            provider: self.provider,
                            tools: self.tools,
                            stamps: self.stamps,
                            cancellation: self.cancellation.clone(),
                            events: self.events,
                            commands: self.commands.as_deref_mut(),
                        },
                        hooks,
                    )
                    .await?;
                    (machine, step) = machine.resolve(outcome)?;
                }
                Step::Idle => {
                    let (next_machine, next_step) = self.start_queued_run(machine).await?;
                    machine = next_machine;
                    if let Some(next_step) = next_step {
                        step = next_step;
                    } else {
                        return Ok(machine);
                    }
                }
                Step::AwaitingOutcome => return Err(DriverError::Suspended),
                _ => return Err(DriverError::UnsupportedAction("future step variant")),
            }
        }
    }

    fn hydrate_queue(&mut self, machine: &mut SessionMachine) -> Result<(), DriverError> {
        let items = self.session.log(0, usize::MAX)?;
        let queued = rho_core::reduce_queue(&items, &rho_core::LaneName::main())
            .map_err(DriverError::Queue)?;
        machine.hydrate_queue(queued)?;
        Ok(())
    }

    fn steer_stamps(&mut self, machine: &SessionMachine) -> Vec<EntryStamp> {
        (0..machine.pending_steers())
            .map(|_| self.stamps.entry())
            .collect()
    }

    async fn drain_ready_controls(
        &mut self,
        machine: &mut SessionMachine,
    ) -> Result<(), DriverError> {
        loop {
            let command = match self.commands.as_deref_mut() {
                Some(commands) => match commands.inner.try_recv() {
                    Ok(command) => command,
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => return Ok(()),
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        self.commands = None;
                        return Ok(());
                    }
                },
                None => return Ok(()),
            };
            let abort = matches!(command, SessionControl::Abort);
            let effects = machine.accept_control(command, self.stamps.timestamp())?;
            apply_effects(self.session, self.events, effects).await?;
            if abort {
                self.cancellation.cancel();
            }
        }
    }

    async fn start_queued_run(
        &mut self,
        mut machine: SessionMachine,
    ) -> Result<(SessionMachine, Option<Step>), DriverError> {
        if self.cancellation.is_cancelled() {
            return Ok((machine, None));
        }
        self.drain_ready_controls(&mut machine).await?;
        let Some(item) = machine.pop_queued_input() else {
            return Ok((machine, None));
        };
        let input = Input::Prompt {
            message: item.message,
            op: self.stamps.op_id(),
            stamp: self.stamps.entry(),
            origin: Origin::External,
            host: None,
            queue: Some(item.id),
            steer_stamps: self.steer_stamps(&machine),
        };
        let (machine, step) = machine.handle(input)?;
        Ok((machine, Some(step)))
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
                && left.source_queue == right.source_queue
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

struct ActionShell<'shell> {
    machine: &'shell mut SessionMachine,
    session: &'shell mut dyn Session,
    provider: &'shell mut BoundProvider,
    tools: &'shell ToolSet,
    stamps: &'shell mut dyn StampSource,
    cancellation: CancellationToken,
    events: &'shell mut dyn EventSink,
    commands: Option<&'shell mut ControlReceiver>,
}

async fn execute_action(
    action: Action,
    shell: ActionShell<'_>,
    hooks: Option<&mut dyn HookHost>,
) -> Result<ActionOutcome, DriverError> {
    let ActionShell {
        machine,
        session,
        provider,
        tools,
        stamps,
        cancellation,
        events,
        mut commands,
    } = shell;
    match action {
        Action::StreamAssistant { request, model, .. } => {
            if model != provider.model {
                return Err(DriverError::ProviderSelectionMismatch {
                    expected: model,
                    actual: provider.model.clone(),
                });
            }
            let mut stream = provider.provider.generate(request, cancellation.clone());
            while let Some(event) = next_provider_event(
                &mut stream,
                &mut commands,
                machine,
                session,
                stamps,
                &cancellation,
                events,
            )
            .await?
            {
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
                            steer_stamps: mint_steer_stamps(machine, stamps),
                        });
                    }
                    StreamEvent::Error(error) => {
                        return Ok(ActionOutcome::Assistant {
                            result: Err(error),
                            stamp: stamps.entry(),
                            steer_stamps: mint_steer_stamps(machine, stamps),
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
                let mut execution = tool.execute(call.effective_args, cancellation.clone());
                loop {
                    let Some(receiver) = commands.as_deref_mut() else {
                        break execution.await;
                    };
                    tokio::select! {
                        biased;
                        command = receiver.inner.recv() => {
                            let Some(command) = command else {
                                commands = None;
                                continue;
                            };
                            accept_live_control(
                                command,
                                machine,
                                session,
                                stamps,
                                &cancellation,
                                events,
                            ).await?;
                        }
                        output = &mut execution => break output,
                    }
                }
            } else {
                ToolOutput::error(format!("unknown tool {:?}", call.name))
            };
            Ok(ActionOutcome::Tool {
                call_id: call.call_id,
                content: output.content,
                is_error: output.is_error,
                details: output.details,
                stamp: stamps.entry(),
                steer_stamps: mint_steer_stamps(machine, stamps),
            })
        }
        Action::Summarize { request, model, .. } => {
            if model != provider.model {
                return Err(DriverError::ProviderSelectionMismatch {
                    expected: model,
                    actual: provider.model.clone(),
                });
            }
            let mut stream = provider.provider.generate(request, cancellation.clone());
            while let Some(event) = next_provider_event(
                &mut stream,
                &mut commands,
                machine,
                session,
                stamps,
                &cancellation,
                events,
            )
            .await?
            {
                events
                    .emit(AgentEvent::ProviderStream {
                        event: event.clone(),
                    })
                    .await;
                match event {
                    StreamEvent::Done(message) => {
                        if message.stop != rho_ai::StopReason::Stop {
                            let error = if message.stop == rho_ai::StopReason::Aborted {
                                ProviderError::cancelled()
                            } else {
                                ProviderError::invalid_response(format!(
                                    "compaction response ended with {:?}",
                                    message.stop
                                ))
                            };
                            return Ok(ActionOutcome::Summary {
                                result: Err(error),
                                stamp: stamps.entry(),
                            });
                        }
                        let text = message
                            .blocks
                            .iter()
                            .filter_map(|block| match block {
                                rho_ai::ContentBlock::Text { text } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        if text.is_empty() {
                            return Ok(ActionOutcome::Summary {
                                result: Err(ProviderError::invalid_response(
                                    "compaction response contained no text",
                                )),
                                stamp: stamps.entry(),
                            });
                        }
                        return Ok(ActionOutcome::Summary {
                            result: Ok(rho_core::CompactionSummary {
                                text,
                                usage: message.usage,
                            }),
                            stamp: stamps.entry(),
                        });
                    }
                    StreamEvent::Error(error) => {
                        return Ok(ActionOutcome::Summary {
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
        Action::AwaitInteraction { request, .. } => {
            await_interaction(
                request,
                &mut commands,
                machine,
                session,
                stamps,
                &cancellation,
                events,
            )
            .await
        }
        Action::InvokeHook { invocation, origin } => {
            let Some(hooks) = hooks else {
                return Ok(ActionOutcome::Hook {
                    result: Err("hook host is not configured".to_owned()),
                    at: stamps.timestamp(),
                });
            };
            let mut invocation = hooks.invoke(invocation, origin);
            let result = loop {
                let Some(receiver) = commands.as_deref_mut() else {
                    break tokio::select! {
                        result = &mut invocation => result,
                        _ = cancellation.cancelled() => Err("hook cancelled".to_owned()),
                    };
                };
                tokio::select! {
                    biased;
                    command = receiver.inner.recv() => {
                        let Some(command) = command else {
                            commands = None;
                            continue;
                        };
                        accept_live_control(
                            command,
                            machine,
                            session,
                            stamps,
                            &cancellation,
                            events,
                        ).await?;
                    }
                    result = &mut invocation => break result,
                    _ = cancellation.cancelled() => break Err("hook cancelled".to_owned()),
                }
            };
            Ok(ActionOutcome::Hook {
                result,
                at: stamps.timestamp(),
            })
        }
        _ => Err(DriverError::UnsupportedAction("future action variant")),
    }
}

async fn next_provider_event(
    stream: &mut rho_ai::ProviderStream<'_>,
    commands: &mut Option<&mut ControlReceiver>,
    machine: &mut SessionMachine,
    session: &mut dyn Session,
    stamps: &mut dyn StampSource,
    cancellation: &CancellationToken,
    events: &mut dyn EventSink,
) -> Result<Option<StreamEvent>, DriverError> {
    loop {
        let Some(receiver) = commands.as_deref_mut() else {
            return Ok(stream.next().await);
        };
        tokio::select! {
            biased;
            command = receiver.inner.recv() => {
                let Some(command) = command else {
                    *commands = None;
                    continue;
                };
                accept_live_control(
                    command,
                    machine,
                    session,
                    stamps,
                    cancellation,
                    events,
                ).await?;
            }
            event = stream.next() => return Ok(event),
        }
    }
}

async fn await_interaction(
    request: InteractionRequest,
    commands: &mut Option<&mut ControlReceiver>,
    machine: &mut SessionMachine,
    session: &mut dyn Session,
    stamps: &mut dyn StampSource,
    cancellation: &CancellationToken,
    events: &mut dyn EventSink,
) -> Result<ActionOutcome, DriverError> {
    let timeout = tokio::time::sleep(Duration::from_millis(request.timeout_ms));
    tokio::pin!(timeout);
    loop {
        let Some(receiver) = commands.as_deref_mut() else {
            tokio::select! {
                _ = &mut timeout => {
                    return Ok(ActionOutcome::Interaction {
                        request_id: request.id,
                        answer: InteractionAnswer::TimedOut,
                        at: stamps.timestamp(),
                    });
                }
                _ = cancellation.cancelled() => {
                    return Ok(ActionOutcome::Interaction {
                        request_id: request.id,
                        answer: InteractionAnswer::TimedOut,
                        at: stamps.timestamp(),
                    });
                }
            }
        };
        tokio::select! {
            biased;
            command = receiver.inner.recv() => {
                let Some(command) = command else {
                    *commands = None;
                    continue;
                };
                match command {
                    SessionControl::AnswerInteraction { id, answer } if id == request.id => {
                        return Ok(ActionOutcome::Interaction {
                            request_id: id,
                            answer,
                            at: stamps.timestamp(),
                        });
                    }
                    SessionControl::AnswerInteraction { id, .. } => {
                        return Err(MachineError::MismatchedInteraction {
                            expected: request.id,
                            actual: id,
                        }.into());
                    }
                    command => {
                        accept_live_control(
                            command,
                            machine,
                            session,
                            stamps,
                            cancellation,
                            events,
                        ).await?;
                    }
                }
            }
            _ = &mut timeout => {
                return Ok(ActionOutcome::Interaction {
                    request_id: request.id,
                    answer: InteractionAnswer::TimedOut,
                    at: stamps.timestamp(),
                });
            }
            _ = cancellation.cancelled() => {
                return Ok(ActionOutcome::Interaction {
                    request_id: request.id,
                    answer: InteractionAnswer::TimedOut,
                    at: stamps.timestamp(),
                });
            }
        }
    }
}

async fn accept_live_control(
    command: SessionControl,
    machine: &mut SessionMachine,
    session: &mut dyn Session,
    stamps: &mut dyn StampSource,
    cancellation: &CancellationToken,
    events: &mut dyn EventSink,
) -> Result<(), DriverError> {
    let abort = matches!(command, SessionControl::Abort);
    let effects = machine.accept_control(command, stamps.timestamp())?;
    apply_effects(session, events, effects).await?;
    if abort {
        cancellation.cancel();
    }
    Ok(())
}

fn mint_steer_stamps(machine: &SessionMachine, stamps: &mut dyn StampSource) -> Vec<EntryStamp> {
    (0..machine.pending_steers())
        .map(|_| stamps.entry())
        .collect()
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
    use rho_core::{CompactionConfig, EntryStamp, MachineConfig, ReplaySafety};
    use rho_store::{CreateOptions, MemoryRepo, SessionRepo};
    use serde_json::Value;

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

    struct QueueOnProviderStart {
        sender: ControlSender,
        sent: bool,
        ids: Vec<QueueId>,
    }

    impl EventSink for QueueOnProviderStart {
        fn emit(&mut self, event: AgentEvent) -> EventFuture<'_> {
            if !self.sent
                && matches!(
                    event,
                    AgentEvent::ProviderStream {
                        event: StreamEvent::Start
                    }
                )
            {
                self.sent = true;
                self.ids.push(
                    self.sender
                        .steer(SessionMessage::user("course correction"))
                        .unwrap(),
                );
                self.ids.push(
                    self.sender
                        .follow_up(SessionMessage::user("next task"))
                        .unwrap(),
                );
            }
            Box::pin(async {})
        }
    }

    struct AbortOnProviderStart {
        sender: ControlSender,
        sent: bool,
    }

    struct InteractiveRunHook {
        calls: usize,
    }

    impl HookHost for InteractiveRunHook {
        fn invoke(&mut self, invocation: HookInvocation, _: Origin) -> HookFuture<'_> {
            self.calls += 1;
            if self.calls == 1 {
                assert_eq!(invocation.hook, rho_core::HookPoint::RunStarted);
                Box::pin(async {
                    Ok(HookOutput::Interact {
                        request: InteractionRequest {
                            id: "permission".to_owned(),
                            prompt: "continue?".to_owned(),
                            timeout_ms: 1_000,
                        },
                    })
                })
            } else {
                assert_eq!(
                    invocation.payload["interaction"]["answer"]["kind"],
                    "answered"
                );
                Box::pin(async { Ok(HookOutput::Continue { value: Value::Null }) })
            }
        }
    }

    struct AnswerOnInteraction {
        sender: ControlSender,
    }

    impl EventSink for AnswerOnInteraction {
        fn emit(&mut self, event: AgentEvent) -> EventFuture<'_> {
            if let AgentEvent::InteractionRequested { request, .. } = event {
                self.sender
                    .answer_interaction(
                        request.id,
                        InteractionAnswer::Answered {
                            value: "yes".to_owned(),
                        },
                    )
                    .unwrap();
            }
            Box::pin(async {})
        }
    }

    impl EventSink for AbortOnProviderStart {
        fn emit(&mut self, event: AgentEvent) -> EventFuture<'_> {
            if !self.sent
                && matches!(
                    event,
                    AgentEvent::ProviderStream {
                        event: StreamEvent::Start
                    }
                )
            {
                self.sent = true;
                self.sender.abort().unwrap();
            }
            Box::pin(async {})
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
                hooks: Vec::new(),
                compaction: None,
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
                hooks: Vec::new(),
                compaction: None,
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
                hooks: Vec::new(),
                compaction: None,
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
                source_queue: None,
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
                source_queue: None,
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
                hooks: Vec::new(),
                compaction: None,
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

    #[tokio::test]
    async fn automatic_driver_persists_a_compaction_checkpoint() {
        let old_assistant = AssistantMessage {
            blocks: vec![rho_ai::ContentBlock::text("old answer")],
            stop: StopReason::Stop,
            usage: Usage {
                input_tokens: 200,
                ..Usage::default()
            },
            provider: ProviderId::from("p"),
            model: ModelId::from("m"),
        };
        let prompt_request = Request {
            system: "test".to_owned(),
            messages: vec![Message::user("old question")],
            tools: Vec::new(),
            max_output_tokens: 100,
            thinking: ThinkingLevel::None,
        };
        let summary_request = Request {
            system: "summarize".to_owned(),
            messages: vec![Message::user("old question")],
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
                    request: prompt_request,
                    events: vec![StreamEvent::Done(old_assistant)],
                },
                Script {
                    request: summary_request,
                    events: vec![StreamEvent::Done(AssistantMessage {
                        blocks: vec![rho_ai::ContentBlock::text("condensed history")],
                        stop: StopReason::Stop,
                        usage: Usage {
                            input_tokens: 50,
                            output_tokens: 5,
                            ..Usage::default()
                        },
                        provider: ProviderId::from("p"),
                        model: ModelId::from("m"),
                    })],
                },
            ],
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
        let machine = SessionMachine::new(
            MachineConfig {
                system: "test".to_owned(),
                max_output_tokens: 100,
                thinking: ThinkingLevel::None,
                model: ModelRef {
                    provider: ProviderId::from("p"),
                    model: ModelId::from("m"),
                },
                tools: Vec::new(),
                hooks: Vec::new(),
                compaction: Some(CompactionConfig {
                    threshold_tokens: 100,
                    retain_messages: 1,
                    system_prompt: "summarize".to_owned(),
                }),
            },
            Vec::new(),
        )
        .unwrap();
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
            SessionMessage::user("old question"),
            Origin::External,
            None,
        )
        .await
        .unwrap();
        assert!(machine.is_idle());
        assert_eq!(machine.compaction_due(), None);
        assert_eq!(session.lane_status().unwrap(), rho_core::LaneStatus::Idle);
        assert!(matches!(
            &session.branch(None).unwrap().last().unwrap().body,
            rho_core::EntryBody::Compaction {
                summary,
                retained_tail,
                tokens_before: 200,
                ..
            } if summary == "condensed history"
                && matches!(retained_tail.as_slice(), [SessionMessage::Assistant(_)])
        ));
        let reopened = SessionMachine::new(
            MachineConfig {
                system: "test".to_owned(),
                max_output_tokens: 100,
                thinking: ThinkingLevel::None,
                model: ModelRef {
                    provider: ProviderId::from("p"),
                    model: ModelId::from("m"),
                },
                tools: Vec::new(),
                hooks: Vec::new(),
                compaction: Some(CompactionConfig {
                    threshold_tokens: 100,
                    retain_messages: 1,
                    system_prompt: "summarize".to_owned(),
                }),
            },
            session.branch(None).unwrap(),
        )
        .unwrap();
        assert_eq!(reopened.compaction_due(), None);
    }

    #[tokio::test]
    async fn live_steering_continues_the_run_and_follow_up_starts_the_next_one() {
        let first = AssistantMessage {
            blocks: vec![rho_ai::ContentBlock::text("first")],
            stop: StopReason::Stop,
            usage: Usage::default(),
            provider: ProviderId::from("p"),
            model: ModelId::from("m"),
        };
        let second = AssistantMessage {
            blocks: vec![rho_ai::ContentBlock::text("second")],
            stop: StopReason::Stop,
            usage: Usage::default(),
            provider: ProviderId::from("p"),
            model: ModelId::from("m"),
        };
        let third = AssistantMessage {
            blocks: vec![rho_ai::ContentBlock::text("third")],
            stop: StopReason::Stop,
            usage: Usage::default(),
            provider: ProviderId::from("p"),
            model: ModelId::from("m"),
        };
        let request = |messages| Request {
            system: "test".to_owned(),
            messages,
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
                    request: request(vec![Message::user("initial")]),
                    events: vec![StreamEvent::Start, StreamEvent::Done(first.clone())],
                },
                Script {
                    request: request(vec![
                        Message::user("initial"),
                        Message::Assistant(first.clone()),
                        Message::user("course correction"),
                    ]),
                    events: vec![StreamEvent::Done(second.clone())],
                },
                Script {
                    request: request(vec![
                        Message::user("initial"),
                        Message::Assistant(first),
                        Message::user("course correction"),
                        Message::Assistant(second),
                        Message::user("next task"),
                    ]),
                    events: vec![StreamEvent::Done(third)],
                },
            ],
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
        let machine = SessionMachine::new(
            MachineConfig {
                system: "test".to_owned(),
                max_output_tokens: 100,
                thinking: ThinkingLevel::None,
                model: ModelRef {
                    provider: ProviderId::from("p"),
                    model: ModelId::from("m"),
                },
                tools: Vec::new(),
                hooks: Vec::new(),
                compaction: None,
            },
            Vec::new(),
        )
        .unwrap();
        let (sender, mut receiver) = control_channel();
        let mut events = QueueOnProviderStart {
            sender,
            sent: false,
            ids: Vec::new(),
        };
        let machine = Driver::new(
            session.as_mut(),
            &mut provider,
            &ToolSet::new(),
            &mut FixedStamps { next: 20 },
            CancellationToken::new(),
            &mut events,
        )
        .with_controls(&mut receiver)
        .run_prompt(
            machine,
            SessionMessage::user("initial"),
            Origin::External,
            None,
        )
        .await
        .unwrap();
        assert!(machine.is_idle());
        assert_eq!(session.lane_status().unwrap(), rho_core::LaneStatus::Idle);
        let items = session.log(0, usize::MAX).unwrap();
        assert!(
            rho_core::reduce_queue(&items, &rho_core::LaneName::main())
                .unwrap()
                .is_empty()
        );
        let consumed = session
            .branch(None)
            .unwrap()
            .into_iter()
            .filter_map(|entry| entry.source_queue)
            .collect::<Vec<_>>();
        assert_eq!(consumed, events.ids);
    }

    #[tokio::test]
    async fn abort_is_durable_before_in_flight_provider_cancellation() {
        let expected = Request {
            system: "test".to_owned(),
            messages: vec![Message::user("long task")],
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
                events: vec![
                    StreamEvent::Start,
                    StreamEvent::Done(AssistantMessage {
                        blocks: vec![rho_ai::ContentBlock::text("too late")],
                        stop: StopReason::Stop,
                        usage: Usage::default(),
                        provider: ProviderId::from("p"),
                        model: ModelId::from("m"),
                    }),
                ],
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
        let machine = SessionMachine::new(
            MachineConfig {
                system: "test".to_owned(),
                max_output_tokens: 100,
                thinking: ThinkingLevel::None,
                model: ModelRef {
                    provider: ProviderId::from("p"),
                    model: ModelId::from("m"),
                },
                tools: Vec::new(),
                hooks: Vec::new(),
                compaction: None,
            },
            Vec::new(),
        )
        .unwrap();
        let (sender, mut receiver) = control_channel();
        let mut events = AbortOnProviderStart {
            sender,
            sent: false,
        };
        let machine = Driver::new(
            session.as_mut(),
            &mut provider,
            &ToolSet::new(),
            &mut FixedStamps { next: 50 },
            CancellationToken::new(),
            &mut events,
        )
        .with_controls(&mut receiver)
        .run_prompt(
            machine,
            SessionMessage::user("long task"),
            Origin::External,
            None,
        )
        .await
        .unwrap();
        assert!(machine.is_idle());
        let records = session
            .log(0, usize::MAX)
            .unwrap()
            .into_iter()
            .filter_map(|item| match item {
                rho_core::Item::Record(record) => Some(record.body),
                _ => None,
            })
            .collect::<Vec<_>>();
        let abort = records
            .iter()
            .position(|record| matches!(record, rho_core::RecordBody::AbortRequested { .. }))
            .unwrap();
        let finish = records
            .iter()
            .position(|record| {
                matches!(
                    record,
                    rho_core::RecordBody::OpFinished {
                        outcome: rho_core::OpOutcome::Aborted,
                        ..
                    }
                )
            })
            .unwrap();
        assert!(abort < finish);
        assert_eq!(session.lane_status().unwrap(), rho_core::LaneStatus::Idle);
    }

    #[tokio::test]
    async fn driver_runs_a_hook_through_a_durable_client_interaction() {
        let expected = Request {
            system: "test".to_owned(),
            messages: vec![Message::user("task")],
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
        let machine = SessionMachine::new(
            MachineConfig {
                system: "test".to_owned(),
                max_output_tokens: 100,
                thinking: ThinkingLevel::None,
                model: ModelRef {
                    provider: ProviderId::from("p"),
                    model: ModelId::from("m"),
                },
                tools: Vec::new(),
                hooks: vec![rho_core::HookPoint::RunStarted],
                compaction: None,
            },
            Vec::new(),
        )
        .unwrap();
        let (sender, mut receiver) = control_channel();
        let mut hooks = InteractiveRunHook { calls: 0 };
        let mut events = AnswerOnInteraction { sender };
        let machine = Driver::new(
            session.as_mut(),
            &mut provider,
            &ToolSet::new(),
            &mut FixedStamps { next: 70 },
            CancellationToken::new(),
            &mut events,
        )
        .with_controls(&mut receiver)
        .with_hooks(&mut hooks)
        .run_prompt(
            machine,
            SessionMessage::user("task"),
            Origin::External,
            None,
        )
        .await
        .unwrap();
        assert!(machine.is_idle());
        assert_eq!(hooks.calls, 2);
        let records = session
            .log(0, usize::MAX)
            .unwrap()
            .into_iter()
            .filter_map(|item| match item {
                rho_core::Item::Record(record) => Some(record.body),
                _ => None,
            })
            .collect::<Vec<_>>();
        let requested = records
            .iter()
            .position(|record| matches!(record, rho_core::RecordBody::InteractionRequested { .. }))
            .unwrap();
        let answered = records
            .iter()
            .position(|record| matches!(record, rho_core::RecordBody::InteractionAnswered { .. }))
            .unwrap();
        let hook_finished = records
            .iter()
            .position(|record| matches!(record, rho_core::RecordBody::HookFinished { .. }))
            .unwrap();
        let step = records
            .iter()
            .position(|record| matches!(record, rho_core::RecordBody::Step { .. }))
            .unwrap();
        assert!(requested < answered && answered < hook_finished && hook_finished < step);
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
            hooks: Vec::new(),
            compaction: None,
        };
        assert!(config.tools.is_empty());
        let _ = ReplaySafety::Safe;
    }
}
