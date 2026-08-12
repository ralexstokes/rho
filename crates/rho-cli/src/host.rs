use std::{
    collections::BTreeMap,
    io::Write as _,
    path::Path,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow};
use rho_agent::{
    BoundProvider, ControlSender, Driver, DriverError, EventFuture, EventSink, StampSource,
    SystemStamps, control_channel,
};
use rho_ai::{ContentBlock, ModelId, ProviderFactory, ProviderId, StreamEvent, ThinkingLevel};
use rho_ai_anthropic::AnthropicFactory;
use rho_ai_openai::OpenAiFactory;
use rho_core::{
    AgentEvent, EntryBody, EntryStamp, InteractionAnswer, LaneName, LaneStatus, MachineConfig,
    ModelRef, NewEntry, Origin, QueueId, SessionId, SessionMachine, SessionMessage,
};
use rho_rpc::{
    ClientRequest, ClientResponse, ErrorObject, HandlerFuture, ResponsePayload, RpcHandler, RpcId,
    RpcSender,
};
use rho_shelterwood::{
    Actor, ActorDef, ActorRef, CallError, CallErrorKind, Context as ActorContext, ExitError,
    ExitResult, Incarnation, Mailbox, Reply, RestartPolicy, SessionHandle,
    ShelterwoodCancellationToken, StopContext, SupervisedSessions,
};
use rho_store::{CreateOptions, ForkPoint, JsonlRepo, Session, SessionError, SessionRepo};
use rho_tools::{McpConnection, ToolSet, coding_tools};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::{config::HostConfig, credentials::credential_source};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RunOutput {
    Text,
    Json,
}

/// Concrete provider, storage, tool, and session-actor host behind rho RPC.
pub(crate) struct HeadlessHost {
    repo: Arc<dyn SessionRepo>,
    config: HostConfig,
    factories: Factories,
    actors: Mutex<BTreeMap<SessionId, ActorSlot>>,
    supervisor: SupervisedSessions,
}

#[derive(Clone)]
struct Factories {
    values: Arc<BTreeMap<String, Arc<dyn ProviderFactory>>>,
}

impl Factories {
    fn new() -> Result<Self> {
        let credentials = credential_source();
        let openai: Arc<dyn ProviderFactory> = Arc::new(OpenAiFactory::new(credentials.clone()));
        let anthropic: Arc<dyn ProviderFactory> =
            Arc::new(AnthropicFactory::new(credentials).context("build Anthropic provider")?);
        let mut values = BTreeMap::new();
        values.insert("openai".to_owned(), openai);
        values.insert("anthropic".to_owned(), anthropic);
        Ok(Self {
            values: Arc::new(values),
        })
    }

    fn get(&self, provider: &ProviderId) -> Result<&dyn ProviderFactory, ErrorObject> {
        self.values
            .get(provider.as_str())
            .map(AsRef::as_ref)
            .ok_or_else(|| {
                ErrorObject::new(
                    "invalid_params",
                    format!("provider {:?} is not configured", provider.as_str()),
                )
            })
    }

    fn validate_model(&self, model: &ModelRef) -> Result<(), ErrorObject> {
        let factory = self.get(&model.provider)?;
        if factory.models().iter().any(|known| known.id == model.model) {
            Ok(())
        } else {
            Err(ErrorObject::new(
                "invalid_params",
                format!(
                    "model {:?} is not supported by provider {:?}",
                    model.model.as_str(),
                    model.provider.as_str()
                ),
            ))
        }
    }

    fn default_model(&self, provider: &ProviderId) -> Result<ModelId, ErrorObject> {
        self.get(provider)?
            .models()
            .first()
            .map(|model| model.id.clone())
            .ok_or_else(|| {
                ErrorObject::new(
                    "invalid_params",
                    format!("provider {:?} exposes no models", provider.as_str()),
                )
            })
    }
}

#[derive(Clone)]
struct ActorHandle {
    actor: ActorRef<ActorCommand>,
    controls: Arc<ControlHub>,
    busy: Arc<AtomicBool>,
}

struct ActorSlot {
    handle: ActorHandle,
    _session: SessionHandle<ActorCommand>,
}

enum ActorCommand {
    Prompt {
        text: String,
        accepted: Reply<Result<(), ErrorObject>>,
    },
    Compact {
        accepted: Reply<Result<(), ErrorObject>>,
    },
    Configure {
        provider: Option<String>,
        model: Option<String>,
        thinking: Option<ThinkingLevel>,
        completed: Reply<Result<(), ErrorObject>>,
    },
}

#[derive(Default)]
struct ControlHub {
    current: StdMutex<Option<ControlSender>>,
}

impl ControlHub {
    fn install(&self, sender: ControlSender) {
        *self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sender);
    }

    fn sender(&self) -> Result<ControlSender, ErrorObject> {
        self.current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or_else(|| ErrorObject::new("internal", "session actor is between incarnations"))
    }
}

impl HeadlessHost {
    pub(crate) async fn new(
        directory: impl Into<std::path::PathBuf>,
        config: HostConfig,
    ) -> Result<Self> {
        let factories = Factories::new()?;
        factories
            .validate_model(&config.model)
            .map_err(|error| anyhow!(error.message))?;
        let supervisor = SupervisedSessions::start().await?;
        Ok(Self {
            repo: Arc::new(JsonlRepo::new(directory)),
            config,
            factories,
            actors: Mutex::new(BTreeMap::new()),
            supervisor,
        })
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        let actors = {
            let mut actors = self.actors.lock().await;
            std::mem::take(&mut *actors)
        };
        for actor in actors.values() {
            if let Ok(controls) = actor.handle.controls.sender() {
                let _ = controls.abort();
            }
        }
        self.supervisor.shutdown().await?;
        Ok(())
    }

    async fn create(&self, cwd: String, outbound: RpcSender) -> Result<Value, ErrorObject> {
        let cwd = tokio::fs::canonicalize(&cwd).await.map_err(|error| {
            ErrorObject::new(
                "invalid_params",
                format!("resolve working directory {cwd:?}: {error}"),
            )
        })?;
        let session = self
            .repo
            .create(CreateOptions {
                cwd: cwd.to_string_lossy().into_owned(),
            })
            .await
            .map_err(store_error)?;
        let id = session.header().id.clone();
        if let Err(error) = self.attach(session, outbound).await {
            let _ = self.repo.delete(id.clone()).await;
            return Err(error);
        }
        self.snapshot_value(id).await
    }

    async fn open(&self, id: SessionId, outbound: RpcSender) -> Result<Value, ErrorObject> {
        if self.actors.lock().await.contains_key(&id) {
            return self.snapshot_value(id).await;
        }
        let session = self.repo.open(id.clone()).await.map_err(store_error)?;
        self.attach(session, outbound).await?;
        self.snapshot_value(id).await
    }

    async fn fork(
        &self,
        id: SessionId,
        entry: Option<rho_core::EntryId>,
        outbound: RpcSender,
    ) -> Result<Value, ErrorObject> {
        let session = self
            .repo
            .fork(id, entry.map_or(ForkPoint::Leaf, ForkPoint::Entry))
            .await
            .map_err(store_error)?;
        let fork = session.header().id.clone();
        if let Err(error) = self.attach(session, outbound).await {
            let _ = self.repo.delete(fork.clone()).await;
            return Err(error);
        }
        self.snapshot_value(fork).await
    }

    async fn attach(
        &self,
        session: Box<dyn Session>,
        outbound: RpcSender,
    ) -> Result<(), ErrorObject> {
        let id = session.header().id.clone();
        drop(session);
        let controls = Arc::new(ControlHub::default());
        let busy = Arc::new(AtomicBool::new(true));
        let startup_error = Arc::new(StdMutex::new(None));
        let args = SessionArgs {
            id: id.clone(),
            config: self.config.clone(),
            factories: self.factories.clone(),
            repo: Arc::clone(&self.repo),
            outbound,
            controls: Arc::clone(&controls),
            busy: Arc::clone(&busy),
            startup_error: Arc::clone(&startup_error),
        };
        let definition = ActorDef::<SessionActor>::cloned(args)
            .mailbox(Mailbox::queue(32).map_err(supervisor_error)?)
            .restart(RestartPolicy::default());
        let session = match self
            .supervisor
            .add(format!("session-{id}"), definition)
            .await
        {
            Ok(session) => session,
            Err(error) => {
                if let Some(error) = startup_error
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                {
                    return Err(error);
                }
                return Err(supervisor_error(error));
            }
        };
        let handle = ActorHandle {
            actor: session.actor().clone(),
            controls,
            busy,
        };
        let mut actors = self.actors.lock().await;
        match actors.entry(id.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(ActorSlot {
                    handle,
                    _session: session,
                });
            }
            std::collections::btree_map::Entry::Occupied(_) => {
                drop(actors);
                let _ = self.supervisor.remove(&session).await;
                return Err(ErrorObject::new(
                    "conflict",
                    format!("session {id} was attached concurrently"),
                ));
            }
        }
        Ok(())
    }

    async fn actor(&self, id: &SessionId) -> Result<ActorHandle, ErrorObject> {
        self.actors
            .lock()
            .await
            .get(id)
            .map(|actor| actor.handle.clone())
            .ok_or_else(|| {
                ErrorObject::new(
                    "not_found",
                    format!("session {id} is not attached; call session.open first"),
                )
            })
    }

    async fn snapshot_value(&self, id: SessionId) -> Result<Value, ErrorObject> {
        let snapshot = self.repo.inspect(id).await.map_err(store_error)?;
        serde_json::to_value(snapshot)
            .map_err(|error| ErrorObject::new("internal", error.to_string()))
    }

    async fn start_prompt(&self, id: SessionId, text: String) -> Result<Value, ErrorObject> {
        if text.trim().is_empty() {
            return Err(ErrorObject::new("invalid_params", "text must not be empty"));
        }
        let actor = self.actor(&id).await?;
        reserve(&actor.busy)?;
        let response = actor
            .actor
            .call(
                move |accepted| ActorCommand::Prompt { text, accepted },
                Duration::MAX,
            )
            .await;
        let response = match response {
            Ok(response) => response.value,
            Err(error) => {
                release_unaccepted(&actor.busy, &error);
                return Err(call_error(error));
            }
        };
        if let Err(error) = response {
            actor.busy.store(false, Ordering::Release);
            return Err(error);
        }
        Ok(json!({"accepted": true}))
    }

    async fn compact(&self, id: SessionId) -> Result<Value, ErrorObject> {
        let actor = self.actor(&id).await?;
        reserve(&actor.busy)?;
        let response = actor
            .actor
            .call(|accepted| ActorCommand::Compact { accepted }, Duration::MAX)
            .await;
        let response = match response {
            Ok(response) => response.value,
            Err(error) => {
                release_unaccepted(&actor.busy, &error);
                return Err(call_error(error));
            }
        };
        if let Err(error) = response {
            actor.busy.store(false, Ordering::Release);
            return Err(error);
        }
        Ok(json!({"accepted": true}))
    }

    async fn configure(
        &self,
        id: SessionId,
        provider: Option<String>,
        model: Option<String>,
        thinking: Option<ThinkingLevel>,
    ) -> Result<Value, ErrorObject> {
        if provider.is_none() && model.is_none() && thinking.is_none() {
            return Err(ErrorObject::new(
                "invalid_params",
                "configure requires provider, model, or thinking",
            ));
        }
        let actor = self.actor(&id).await?;
        reserve(&actor.busy)?;
        let response = actor
            .actor
            .call(
                move |completed| ActorCommand::Configure {
                    provider,
                    model,
                    thinking,
                    completed,
                },
                Duration::MAX,
            )
            .await;
        let response = match response {
            Ok(response) => response.value,
            Err(error) => {
                release_unaccepted(&actor.busy, &error);
                return Err(call_error(error));
            }
        };
        if let Err(error) = response {
            actor.busy.store(false, Ordering::Release);
            return Err(error);
        }
        self.snapshot_value(id).await
    }
}

impl RpcHandler for HeadlessHost {
    fn request(
        &self,
        request: ClientRequest,
        outbound: RpcSender,
    ) -> HandlerFuture<'_, Result<Value, ErrorObject>> {
        Box::pin(async move {
            match request.method.as_str() {
                "session.create" => {
                    let params: CreateParams = params(request.params)?;
                    self.create(params.cwd, outbound).await
                }
                "session.open" => {
                    let params: SessionParams = params(request.params)?;
                    self.open(SessionId::from(params.session_id), outbound)
                        .await
                }
                "session.list" => {
                    empty_params(request.params)?;
                    let sessions = self.repo.list().await.map_err(store_error)?;
                    serde_json::to_value(sessions)
                        .map_err(|error| ErrorObject::new("internal", error.to_string()))
                }
                "session.fork" => {
                    let params: ForkParams = params(request.params)?;
                    self.fork(
                        SessionId::from(params.session_id),
                        params.entry_id.map(rho_core::EntryId::from),
                        outbound,
                    )
                    .await
                }
                "session.delete" => {
                    let params: SessionParams = params(request.params)?;
                    let id = SessionId::from(params.session_id);
                    if self.actors.lock().await.contains_key(&id) {
                        return Err(ErrorObject::new(
                            "locked",
                            "attached sessions must be released by ending the host",
                        ));
                    }
                    self.repo.delete(id).await.map_err(store_error)?;
                    Ok(json!({"deleted": true}))
                }
                "session.get_snapshot" => {
                    let params: SessionParams = params(request.params)?;
                    self.snapshot_value(SessionId::from(params.session_id))
                        .await
                }
                "session.prompt" => {
                    let params: TextParams = params(request.params)?;
                    self.start_prompt(SessionId::from(params.session_id), params.text)
                        .await
                }
                "session.steer" | "session.follow_up" => {
                    let params: TextParams = params(request.params)?;
                    if params.text.trim().is_empty() {
                        return Err(ErrorObject::new("invalid_params", "text must not be empty"));
                    }
                    let id = SessionId::from(params.session_id);
                    let actor = self.actor(&id).await?;
                    if !actor.busy.load(Ordering::Acquire) {
                        return Err(ErrorObject::new("busy", "session has no active operation"));
                    }
                    let controls = actor.controls.sender()?;
                    let queue = if request.method == "session.steer" {
                        controls.steer(SessionMessage::user(params.text))
                    } else {
                        controls.follow_up(SessionMessage::user(params.text))
                    }
                    .map_err(|_| actor_closed(&id))?;
                    Ok(json!({"queue_id": queue}))
                }
                "session.cancel_queued" => {
                    let params: CancelParams = params(request.params)?;
                    let id = SessionId::from(params.session_id);
                    let actor = self.actor(&id).await?;
                    if !actor.busy.load(Ordering::Acquire) {
                        return Err(ErrorObject::new("busy", "session has no active operation"));
                    }
                    actor
                        .controls
                        .sender()?
                        .cancel(QueueId::from(params.queue_id))
                        .map_err(|_| actor_closed(&id))?;
                    Ok(json!({"accepted": true}))
                }
                "session.abort" => {
                    let params: SessionParams = params(request.params)?;
                    let id = SessionId::from(params.session_id);
                    let actor = self.actor(&id).await?;
                    if !actor.busy.load(Ordering::Acquire) {
                        return Err(ErrorObject::new("busy", "session has no active operation"));
                    }
                    actor
                        .controls
                        .sender()?
                        .abort()
                        .map_err(|_| actor_closed(&id))?;
                    Ok(json!({"accepted": true}))
                }
                "session.compact" => {
                    let params: SessionParams = params(request.params)?;
                    self.compact(SessionId::from(params.session_id)).await
                }
                "session.configure" => {
                    let params: ConfigureParams = params(request.params)?;
                    self.configure(
                        SessionId::from(params.session_id),
                        params.provider,
                        params.model,
                        params.thinking,
                    )
                    .await
                }
                _ => Err(ErrorObject::new(
                    "method_not_found",
                    format!("unknown RPC method {:?}", request.method),
                )),
            }
        })
    }

    fn response(
        &self,
        response: ClientResponse,
        _: RpcSender,
    ) -> HandlerFuture<'_, Result<(), ErrorObject>> {
        Box::pin(async move {
            let RpcId::String(id) = response.id else {
                return Err(ErrorObject::new(
                    "invalid_request",
                    "interaction response ID must be a string",
                ));
            };
            let Some((session, request_id)) = id.split_once(':') else {
                return Err(ErrorObject::new(
                    "invalid_request",
                    "unknown server request ID",
                ));
            };
            let answer = match response.payload {
                ResponsePayload::Failure(_) => InteractionAnswer::Declined,
                ResponsePayload::Success(result) => {
                    let answer = result
                        .get("answer")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            ErrorObject::new(
                                "invalid_params",
                                "interaction result requires string field answer",
                            )
                        })?;
                    if answer == "declined" {
                        InteractionAnswer::Declined
                    } else {
                        InteractionAnswer::Answered {
                            value: answer.to_owned(),
                        }
                    }
                }
            };
            let session = SessionId::from(session);
            self.actor(&session)
                .await?
                .controls
                .sender()?
                .answer_interaction(request_id, answer)
                .map_err(|_| actor_closed(&session))
        })
    }
}

#[derive(Clone)]
struct SessionArgs {
    id: SessionId,
    config: HostConfig,
    factories: Factories,
    repo: Arc<dyn SessionRepo>,
    outbound: RpcSender,
    controls: Arc<ControlHub>,
    busy: Arc<AtomicBool>,
    startup_error: Arc<StdMutex<Option<ErrorObject>>>,
}

struct SessionActor {
    id: SessionId,
    session: Box<dyn Session>,
    machine: SessionMachine,
    provider: BoundProvider,
    tools: ToolSet,
    _mcp: Vec<McpConnection>,
    base: MachineConfig,
    factories: Factories,
    repo: Arc<dyn SessionRepo>,
    outbound: RpcSender,
    controls: rho_agent::ControlReceiver,
    control_hub: Arc<ControlHub>,
    busy: Arc<AtomicBool>,
    shutdown: ShelterwoodCancellationToken,
    incarnation: Incarnation,
}

impl Actor for SessionActor {
    type Msg = ActorCommand;
    type Args = SessionArgs;

    async fn init(
        args: Self::Args,
        context: &mut ActorContext<'_, Self>,
    ) -> Result<Self, ExitError> {
        args.busy.store(true, Ordering::Release);
        let startup_error = Arc::clone(&args.startup_error);
        match Self::initialize(args, context).await {
            Ok(actor) => {
                *startup_error
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                Ok(actor)
            }
            Err(error) => {
                *startup_error
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error.clone());
                Err(ExitError::message(error.message))
            }
        }
    }

    async fn handle(&mut self, command: Self::Msg, _: &mut ActorContext<'_, Self>) -> ExitResult {
        match command {
            ActorCommand::Prompt { text, accepted } => {
                let result = self.drive_prompt(text, accepted).await;
                match result {
                    Ok(machine) => self.machine = machine,
                    Err(error) => {
                        let message = error.to_string();
                        self.emit_failure(&error).await;
                        self.emit_snapshot().await;
                        return Err(ExitError::message(message));
                    }
                }
                self.busy.store(false, Ordering::Release);
                self.emit_snapshot().await;
            }
            ActorCommand::Compact { accepted } => {
                let result = self.drive_compaction(accepted).await;
                match result {
                    Ok(machine) => self.machine = machine,
                    Err(error) => {
                        let message = error.to_string();
                        self.emit_failure(&error).await;
                        self.emit_snapshot().await;
                        return Err(ExitError::message(message));
                    }
                }
                self.busy.store(false, Ordering::Release);
                self.emit_snapshot().await;
            }
            ActorCommand::Configure {
                provider,
                model,
                thinking,
                completed,
            } => {
                let result = self.apply_configuration(provider, model, thinking).await;
                self.busy.store(false, Ordering::Release);
                completed.send(result);
                self.emit_snapshot().await;
            }
        }
        Ok(())
    }

    async fn on_stop(&mut self, _: &mut StopContext<'_, Self>) {
        self.busy.store(true, Ordering::Release);
        if let Ok(controls) = self.control_hub.sender() {
            let _ = controls.abort();
        }
    }
}

impl SessionActor {
    async fn initialize(
        args: SessionArgs,
        context: &mut ActorContext<'_, Self>,
    ) -> Result<Self, ErrorObject> {
        let session = args.repo.open(args.id.clone()).await.map_err(store_error)?;
        let cwd = session.header().cwd.clone();
        let (tools, mcp) = build_tools(&args.config, &cwd).await?;
        let base = args.config.machine(tools.specs());
        let entries = session.branch(None).map_err(store_error)?;
        let machine = SessionMachine::new(base.clone(), entries).map_err(|error| {
            ErrorObject::new("conflict", format!("invalid session context: {error}"))
        })?;
        let status = session.lane_status().map_err(store_error)?;
        if let LaneStatus::Corrupt(reason) = &status {
            return Err(ErrorObject::new(
                "conflict",
                format!("session journal is corrupt: {reason:?}"),
            ));
        }
        let provider = BoundProvider::open(
            args.factories.get(&machine.model().provider)?,
            machine.model().clone(),
        )
        .await
        .map_err(|error| ErrorObject::new("invalid_params", error.to_string()))?;
        let (controls, control_receiver) = control_channel();
        args.controls.install(controls);
        let mut actor = Self {
            id: args.id,
            session,
            machine,
            provider,
            tools,
            _mcp: mcp,
            base,
            factories: args.factories,
            repo: args.repo,
            outbound: args.outbound,
            controls: control_receiver,
            control_hub: args.controls,
            busy: args.busy,
            shutdown: context.shutdown_token(),
            incarnation: context.incarnation(),
        };
        if !matches!(status, LaneStatus::Idle) {
            actor.machine = actor
                .drive_resume()
                .await
                .map_err(|error| driver_error(&error))?;
        }
        actor.busy.store(false, Ordering::Release);
        let _ = actor
            .outbound
            .event(
                "agent.supervision",
                json!({
                    "session_id": actor.id,
                    "state": "ready",
                    "incarnation": format!("{:?}", actor.incarnation),
                }),
            )
            .await;
        Ok(actor)
    }

    async fn drive_resume(&mut self) -> Result<SessionMachine, DriverError> {
        let (cancellation, bridge) = self.bridged_cancellation();
        let mut stamps = SystemStamps;
        let mut events = RpcEventSink::new(
            self.id.clone(),
            Arc::clone(&self.repo),
            self.outbound.clone(),
        );
        let result = Driver::new(
            self.session.as_mut(),
            &mut self.provider,
            &self.tools,
            &mut stamps,
            cancellation,
            &mut events,
        )
        .with_controls(&mut self.controls)
        .resume(self.machine.clone())
        .await;
        bridge.abort();
        result
    }

    async fn drive_prompt(
        &mut self,
        text: String,
        accepted: Reply<Result<(), ErrorObject>>,
    ) -> Result<SessionMachine, DriverError> {
        let (cancellation, bridge) = self.bridged_cancellation();
        let mut stamps = SystemStamps;
        let mut events = RpcEventSink::new(
            self.id.clone(),
            Arc::clone(&self.repo),
            self.outbound.clone(),
        )
        .with_acceptance(accepted);
        let host = self.host_info();
        let result = Driver::new(
            self.session.as_mut(),
            &mut self.provider,
            &self.tools,
            &mut stamps,
            cancellation,
            &mut events,
        )
        .with_controls(&mut self.controls)
        .run_prompt(
            self.machine.clone(),
            SessionMessage::user(text),
            Origin::External,
            Some(host),
        )
        .await;
        bridge.abort();
        events.finish_acceptance(&result);
        result
    }

    async fn drive_compaction(
        &mut self,
        accepted: Reply<Result<(), ErrorObject>>,
    ) -> Result<SessionMachine, DriverError> {
        let (cancellation, bridge) = self.bridged_cancellation();
        let mut stamps = SystemStamps;
        let mut events = RpcEventSink::new(
            self.id.clone(),
            Arc::clone(&self.repo),
            self.outbound.clone(),
        )
        .with_acceptance(accepted);
        let host = self.host_info();
        let result = Driver::new(
            self.session.as_mut(),
            &mut self.provider,
            &self.tools,
            &mut stamps,
            cancellation,
            &mut events,
        )
        .with_controls(&mut self.controls)
        .compact(self.machine.clone(), Origin::External, Some(host))
        .await;
        bridge.abort();
        events.finish_acceptance(&result);
        result
    }

    fn bridged_cancellation(&self) -> (rho_ai::CancellationToken, tokio::task::JoinHandle<()>) {
        let cancellation = rho_ai::CancellationToken::new();
        let signal = cancellation.clone();
        let shutdown = self.shutdown.clone();
        let bridge = tokio::spawn(async move {
            shutdown.cancelled().await;
            signal.cancel();
        });
        (cancellation, bridge)
    }

    fn host_info(&self) -> Value {
        json!({
            "host": "rho-rpc",
            "version": env!("CARGO_PKG_VERSION"),
            "shelterwood_incarnation": format!("{:?}", self.incarnation),
        })
    }

    async fn apply_configuration(
        &mut self,
        provider: Option<String>,
        model: Option<String>,
        thinking: Option<ThinkingLevel>,
    ) -> Result<(), ErrorObject> {
        if self.session.lane_status().map_err(store_error)? != LaneStatus::Idle {
            return Err(ErrorObject::new("busy", "session lane is not idle"));
        }
        let current = self.machine.model().clone();
        let target_provider = provider
            .as_deref()
            .map(ProviderId::from)
            .unwrap_or_else(|| current.provider.clone());
        let target_model = match model {
            Some(model) => ModelId::from(model),
            None if target_provider == current.provider => current.model.clone(),
            None => self.factories.default_model(&target_provider)?,
        };
        let target = ModelRef {
            provider: target_provider,
            model: target_model,
        };
        self.factories.validate_model(&target)?;
        if target == current && thinking.is_none() {
            return Err(ErrorObject::new(
                "invalid_params",
                "configuration does not change model or thinking",
            ));
        }
        let mut replacement = if target != current {
            Some(
                BoundProvider::open(self.factories.get(&target.provider)?, target.clone())
                    .await
                    .map_err(|error| ErrorObject::new("invalid_params", error.to_string()))?,
            )
        } else {
            None
        };
        let mut stamps = SystemStamps;
        let EntryStamp { id, at } = stamps.entry();
        self.session
            .append_entry(NewEntry {
                id,
                parent: self.session.leaf(),
                lane: LaneName::main(),
                op: None,
                source_queue: None,
                at,
                body: EntryBody::SettingsChange {
                    model: (target != current).then_some(target),
                    thinking,
                },
            })
            .map_err(store_error)?;
        self.machine = SessionMachine::new(
            self.base.clone(),
            self.session.branch(None).map_err(store_error)?,
        )
        .map_err(|error| ErrorObject::new("conflict", error.to_string()))?;
        if let Some(provider) = replacement.take() {
            self.provider = provider;
        }
        Ok(())
    }

    async fn emit_failure(&mut self, error: &DriverError) {
        let _ = self
            .outbound
            .event(
                "agent.failed",
                json!({"session_id": self.id, "error": error.to_string()}),
            )
            .await;
    }

    async fn emit_snapshot(&mut self) {
        if let Ok(snapshot) = self.repo.inspect(self.id.clone()).await {
            let _ = self
                .outbound
                .event(
                    "session.snapshot",
                    serde_json::to_value(snapshot).unwrap_or(Value::Null),
                )
                .await;
        }
    }
}

struct RpcEventSink {
    session: SessionId,
    repo: Arc<dyn SessionRepo>,
    outbound: RpcSender,
    accepted: Option<Reply<Result<(), ErrorObject>>>,
}

impl RpcEventSink {
    fn new(session: SessionId, repo: Arc<dyn SessionRepo>, outbound: RpcSender) -> Self {
        Self {
            session,
            repo,
            outbound,
            accepted: None,
        }
    }

    fn with_acceptance(mut self, accepted: Reply<Result<(), ErrorObject>>) -> Self {
        self.accepted = Some(accepted);
        self
    }

    fn finish_acceptance(&mut self, result: &Result<SessionMachine, DriverError>) {
        let Some(accepted) = self.accepted.take() else {
            return;
        };
        let error = match result {
            Ok(_) => ErrorObject::new(
                "internal",
                "operation finished without a durable start event",
            ),
            Err(error) => driver_error(error),
        };
        accepted.send(Err(error));
    }
}

impl EventSink for RpcEventSink {
    fn emit(&mut self, event: AgentEvent) -> EventFuture<'_> {
        if matches!(&event, AgentEvent::OperationStarted { .. })
            && let Some(accepted) = self.accepted.take()
        {
            accepted.send(Ok(()));
        }
        let session = self.session.clone();
        let repo = Arc::clone(&self.repo);
        let outbound = self.outbound.clone();
        Box::pin(async move {
            if let AgentEvent::InteractionRequested { request, .. } = &event {
                let _ = outbound
                    .request(
                        format!("{}:{}", session, request.id),
                        "interaction.answer",
                        json!({
                            "session_id": session,
                            "prompt": request.prompt,
                            "timeout_ms": request.timeout_ms,
                        }),
                    )
                    .await;
            }
            let durable = !matches!(event, AgentEvent::ProviderStream { .. });
            let _ = outbound
                .event(
                    "agent.event",
                    json!({"session_id": session, "event": event}),
                )
                .await;
            if durable && let Ok(snapshot) = repo.inspect(session).await {
                let _ = outbound
                    .event(
                        "session.snapshot",
                        serde_json::to_value(snapshot).unwrap_or(Value::Null),
                    )
                    .await;
            }
        })
    }
}

pub(crate) async fn run_once(
    directory: &Path,
    config: HostConfig,
    cwd: String,
    prompt: String,
    output: RunOutput,
) -> Result<SessionId> {
    let repo = JsonlRepo::new(directory);
    let (tools, _mcp) = build_tools(&config, &cwd)
        .await
        .map_err(|error| anyhow!(error.message))?;
    let machine = SessionMachine::new(config.machine(tools.specs()), Vec::new())?;
    let factories = Factories::new()?;
    factories
        .validate_model(&config.model)
        .map_err(|error| anyhow!(error.message))?;
    let mut provider = BoundProvider::open(
        factories
            .get(&config.model.provider)
            .map_err(|error| anyhow!(error.message))?,
        config.model.clone(),
    )
    .await?;
    let mut session = repo
        .create(CreateOptions { cwd: cwd.clone() })
        .await
        .context("create session")?;
    let id = session.header().id.clone();
    let mut stamps = SystemStamps;
    let mut events = OneShotSink::new(output, id.clone());
    let cancellation = rho_ai::CancellationToken::new();
    let signal = cancellation.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal.cancel();
        }
    });
    Driver::new(
        session.as_mut(),
        &mut provider,
        &tools,
        &mut stamps,
        cancellation,
        &mut events,
    )
    .run_prompt(
        machine,
        SessionMessage::user(prompt),
        Origin::External,
        Some(json!({"host": "rho-run", "version": env!("CARGO_PKG_VERSION")})),
    )
    .await?;
    let snapshot = repo.inspect(id.clone()).await.context("inspect session")?;
    events.finish(snapshot)?;
    Ok(id)
}

enum OneShotSink {
    Text(ConsoleSink),
    Json(JsonSink),
}

impl OneShotSink {
    fn new(output: RunOutput, session: SessionId) -> Self {
        match output {
            RunOutput::Text => Self::Text(ConsoleSink::default()),
            RunOutput::Json => Self::Json(JsonSink {
                session,
                error: None,
            }),
        }
    }

    fn finish(&mut self, snapshot: rho_store::SessionSnapshot) -> Result<()> {
        match self {
            Self::Text(console) if console.wrote_text => println!(),
            Self::Text(_) => {}
            Self::Json(json) => {
                json.write(json!({"v": 1, "event": "session.snapshot", "data": snapshot}));
                if let Some(error) = json.error.take() {
                    return Err(anyhow!("write JSON event stream: {error}"));
                }
            }
        }
        Ok(())
    }
}

impl EventSink for OneShotSink {
    fn emit(&mut self, event: AgentEvent) -> EventFuture<'_> {
        match self {
            Self::Text(console) => console.emit(event),
            Self::Json(json) => {
                json.write(json!({
                    "v": 1,
                    "event": "agent.event",
                    "data": {"session_id": json.session, "event": event},
                }));
                Box::pin(async {})
            }
        }
    }
}

struct JsonSink {
    session: SessionId,
    error: Option<String>,
}

impl JsonSink {
    fn write(&mut self, value: Value) {
        if self.error.is_some() {
            return;
        }
        let mut stdout = std::io::stdout().lock();
        if let Err(error) = serde_json::to_writer(&mut stdout, &value) {
            self.error = Some(error.to_string());
            return;
        }
        if let Err(error) = writeln!(stdout) {
            self.error = Some(error.to_string());
            return;
        }
        if let Err(error) = stdout.flush() {
            self.error = Some(error.to_string());
        }
    }
}

#[derive(Default)]
struct ConsoleSink {
    wrote_text: bool,
    streamed_this_message: bool,
}

impl EventSink for ConsoleSink {
    fn emit(&mut self, event: AgentEvent) -> EventFuture<'_> {
        match event {
            AgentEvent::OperationStarted { .. } => self.streamed_this_message = false,
            AgentEvent::ProviderStream {
                event: StreamEvent::Start,
            } => self.streamed_this_message = false,
            AgentEvent::ProviderStream {
                event:
                    StreamEvent::Delta {
                        kind: rho_ai::DeltaKind::Text,
                        delta,
                        ..
                    },
            } => {
                print!("{delta}");
                let _ = std::io::stdout().flush();
                self.wrote_text = true;
                self.streamed_this_message = true;
            }
            AgentEvent::MessageAppended {
                message: SessionMessage::Assistant(message),
                ..
            } if !self.streamed_this_message => {
                for block in message.blocks {
                    if let ContentBlock::Text { text } = block {
                        print!("{text}");
                        self.wrote_text = true;
                    }
                }
                let _ = std::io::stdout().flush();
            }
            _ => {}
        }
        Box::pin(async {})
    }
}

async fn build_tools(
    config: &HostConfig,
    cwd: &str,
) -> Result<(ToolSet, Vec<McpConnection>), ErrorObject> {
    let mut tools =
        coding_tools(cwd).map_err(|error| ErrorObject::new("internal", error.to_string()))?;
    let mut connections = Vec::with_capacity(config.mcp.len());
    for template in &config.mcp {
        let connection = McpConnection::connect(template.for_session(cwd))
            .await
            .map_err(|error| ErrorObject::new("internal", error.to_string()))?;
        for tool in connection.tools() {
            tools.register(tool.clone()).map_err(|error| {
                ErrorObject::new("conflict", format!("MCP tool collision: {error}"))
            })?;
        }
        connections.push(connection);
    }
    Ok((tools, connections))
}

fn reserve(busy: &AtomicBool) -> Result<(), ErrorObject> {
    busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| ())
        .map_err(|_| ErrorObject::new("busy", "session already has an active operation"))
}

fn actor_closed(id: &SessionId) -> ErrorObject {
    ErrorObject::new("internal", format!("session actor {id} is closed"))
}

fn call_error(error: CallError) -> ErrorObject {
    let code = match error.kind {
        CallErrorKind::ResponseTimedOut | CallErrorKind::ReplyDropped => "conflict",
        CallErrorKind::Terminated | CallErrorKind::AcceptanceTimedOut => "internal",
        _ => "internal",
    };
    ErrorObject::new(code, format!("session actor call failed: {error}"))
}

fn release_unaccepted(busy: &AtomicBool, error: &CallError) {
    if matches!(
        error.kind,
        CallErrorKind::AcceptanceTimedOut | CallErrorKind::Terminated
    ) {
        busy.store(false, Ordering::Release);
    }
}

fn supervisor_error(error: impl std::fmt::Display) -> ErrorObject {
    ErrorObject::new("internal", format!("session supervision failed: {error}"))
}

fn driver_error(error: &DriverError) -> ErrorObject {
    match error {
        DriverError::LaneNotIdle { .. } | DriverError::Suspended => {
            ErrorObject::new("busy", error.to_string())
        }
        DriverError::Store(SessionError::Locked(_)) => {
            ErrorObject::new("locked", error.to_string())
        }
        _ => ErrorObject::new("internal", error.to_string()),
    }
}

fn store_error(error: SessionError) -> ErrorObject {
    let code = match error {
        SessionError::NotFound(_) => "not_found",
        SessionError::Locked(_) => "locked",
        SessionError::AlreadyExists(_)
        | SessionError::InvalidEntryParent { .. }
        | SessionError::DuplicateEntry(_)
        | SessionError::IncompleteToolTurn { .. } => "conflict",
        SessionError::InvalidSessionId(_)
        | SessionError::UnsupportedLane(_)
        | SessionError::UnknownEntry(_) => "invalid_params",
        _ => "internal",
    };
    ErrorObject::new(code, error.to_string())
}

fn params<T: DeserializeOwned>(value: Value) -> Result<T, ErrorObject> {
    serde_json::from_value(value)
        .map_err(|error| ErrorObject::new("invalid_params", error.to_string()))
}

fn empty_params(value: Value) -> Result<(), ErrorObject> {
    if value.is_null() || value.as_object().is_some_and(serde_json::Map::is_empty) {
        Ok(())
    } else {
        Err(ErrorObject::new(
            "invalid_params",
            "method takes no parameters",
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateParams {
    cwd: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionParams {
    session_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ForkParams {
    session_id: String,
    #[serde(default)]
    entry_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TextParams {
    session_id: String,
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelParams {
    session_id: String,
    queue_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigureParams {
    session_id: String,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    thinking: Option<ThinkingLevel>,
}

#[cfg(test)]
mod tests {
    use rho_ai::{
        AssistantMessage, CancellationToken, Message, ModelInfo, OpenProvider, Provider,
        ProviderId, ProviderStream, Request, SessionConfig, StopReason, Usage,
        faux::{FauxFactory, Script},
    };
    use rho_store::MemoryRepo;
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

    use super::*;

    #[test]
    fn store_failures_have_stable_rpc_classes() {
        assert_eq!(
            store_error(SessionError::NotFound(SessionId::from("missing"))).code,
            "not_found"
        );
        assert_eq!(
            store_error(SessionError::Locked(SessionId::from("locked"))).code,
            "locked"
        );
    }

    #[test]
    fn method_params_reject_unknown_fields() {
        assert!(params::<SessionParams>(json!({"session_id": "s", "extra": true})).is_err());
    }

    #[tokio::test]
    async fn rpc_create_prompt_and_snapshot_drive_the_real_session_machine() {
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        let tools = coding_tools(&cwd).unwrap();
        let config = HostConfig {
            model: ModelRef {
                provider: ProviderId::from("faux"),
                model: ModelId::from("faux-model"),
            },
            thinking: ThinkingLevel::None,
            max_output_tokens: 1_000,
            system: "test system".to_owned(),
            compaction: None,
            mcp: Vec::new(),
        };
        let expected = Request {
            system: config.system.clone(),
            messages: vec![Message::user("inspect")],
            tools: tools
                .specs()
                .into_iter()
                .map(|spec| spec.definition)
                .collect(),
            max_output_tokens: config.max_output_tokens,
            thinking: config.thinking,
        };
        let done = AssistantMessage {
            blocks: vec![ContentBlock::text("done")],
            stop: StopReason::Stop,
            usage: Usage::default(),
            provider: ProviderId::from("faux"),
            model: ModelId::from("faux-model"),
        };
        let factory = FauxFactory::new(
            vec![ModelInfo {
                id: ModelId::from("faux-model"),
                display_name: "Faux".to_owned(),
                context_tokens: None,
                max_output_tokens: None,
            }],
            [Script {
                request: expected,
                events: vec![StreamEvent::Start, StreamEvent::Done(done)],
            }],
        );
        let mut factories = BTreeMap::new();
        factories.insert(
            "faux".to_owned(),
            Arc::new(factory.clone()) as Arc<dyn ProviderFactory>,
        );
        let repo = MemoryRepo::default();
        let host = Arc::new(HeadlessHost {
            repo: Arc::new(repo.clone()),
            config,
            factories: Factories {
                values: Arc::new(factories),
            },
            actors: Mutex::new(BTreeMap::new()),
            supervisor: SupervisedSessions::start().await.unwrap(),
        });
        let observed = Arc::clone(&host);
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server);
        let server = tokio::spawn(rho_rpc::serve(server_read, server_write, host));
        let (client_read, mut client_write) = tokio::io::split(client);
        let mut client_read = BufReader::new(client_read);

        client_write
            .write_all(
                format!(
                    "{{\"v\":1,\"id\":\"create\",\"method\":\"session.create\",\"params\":{{\"cwd\":{}}}}}\n",
                    serde_json::to_string(&cwd).unwrap()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let created = read_until(&mut client_read, |value| {
            value.get("id") == Some(&json!("create"))
        })
        .await;
        let id = created
            .pointer("/result/header/id")
            .and_then(Value::as_str)
            .unwrap()
            .to_owned();
        client_write
            .write_all(
                format!(
                    "{{\"v\":1,\"id\":\"prompt\",\"method\":\"session.prompt\",\"params\":{{\"session_id\":\"{id}\",\"text\":\"inspect\"}}}}\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        read_until(&mut client_read, |value| {
            value.pointer("/data/event/kind") == Some(&json!("operation_finished"))
        })
        .await;

        let snapshot = repo.inspect(SessionId::from(id)).await.unwrap();
        assert_eq!(snapshot.status, LaneStatus::Idle);
        assert!(snapshot.items.iter().any(|item| matches!(
            item,
            rho_core::Item::Entry(rho_core::Entry {
                body: EntryBody::Message {
                    message: SessionMessage::Assistant(message)
                },
                ..
            }) if message.blocks == [ContentBlock::text("done")]
        )));
        assert_eq!(factory.remaining(), 0);

        drop(client_write);
        drop(client_read);
        server.await.unwrap().unwrap();
        observed.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shelterwood_restart_resumes_the_suspended_journal() {
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        let config = HostConfig {
            model: ModelRef {
                provider: ProviderId::from("crash-once"),
                model: ModelId::from("crash-model"),
            },
            thinking: ThinkingLevel::None,
            max_output_tokens: 1_000,
            system: "test system".to_owned(),
            compaction: None,
            mcp: Vec::new(),
        };
        let done = AssistantMessage {
            blocks: vec![ContentBlock::text("recovered")],
            stop: StopReason::Stop,
            usage: Usage::default(),
            provider: ProviderId::from("crash-once"),
            model: ModelId::from("crash-model"),
        };
        let factory = CrashOnceFactory::new(done);
        let mut factories = BTreeMap::new();
        factories.insert(
            "crash-once".to_owned(),
            Arc::new(factory.clone()) as Arc<dyn ProviderFactory>,
        );
        let repo = MemoryRepo::default();
        let host = Arc::new(HeadlessHost {
            repo: Arc::new(repo.clone()),
            config,
            factories: Factories {
                values: Arc::new(factories),
            },
            actors: Mutex::new(BTreeMap::new()),
            supervisor: SupervisedSessions::start().await.unwrap(),
        });
        let observed = Arc::clone(&host);
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server);
        let server = tokio::spawn(rho_rpc::serve(server_read, server_write, host));
        let (client_read, mut client_write) = tokio::io::split(client);
        let mut client_read = BufReader::new(client_read);

        client_write
            .write_all(
                format!(
                    "{{\"v\":1,\"id\":\"create\",\"method\":\"session.create\",\"params\":{{\"cwd\":{}}}}}\n",
                    serde_json::to_string(&cwd).unwrap()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let created = read_until(&mut client_read, |value| {
            value.get("id") == Some(&json!("create"))
        })
        .await;
        let id = created
            .pointer("/result/header/id")
            .and_then(Value::as_str)
            .unwrap()
            .to_owned();
        client_write
            .write_all(
                format!(
                    "{{\"v\":1,\"id\":\"prompt\",\"method\":\"session.prompt\",\"params\":{{\"session_id\":\"{id}\",\"text\":\"recover\"}}}}\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        read_until(&mut client_read, |value| {
            value.pointer("/data/event/kind") == Some(&json!("operation_finished"))
        })
        .await;

        let snapshot = repo.inspect(SessionId::from(id)).await.unwrap();
        assert_eq!(snapshot.status, LaneStatus::Idle);
        assert!(snapshot.items.iter().any(|item| matches!(
            item,
            rho_core::Item::Entry(rho_core::Entry {
                body: EntryBody::Message {
                    message: SessionMessage::Assistant(message)
                },
                ..
            }) if message.blocks == [ContentBlock::text("recovered")]
        )));
        assert_eq!(factory.opens.load(Ordering::SeqCst), 2);
        assert!(
            serde_json::to_string(&snapshot)
                .unwrap()
                .contains("shelterwood_incarnation")
        );

        drop(client_write);
        drop(client_read);
        server.await.unwrap().unwrap();
        observed.shutdown().await.unwrap();
    }

    #[derive(Clone)]
    struct CrashOnceFactory {
        models: Vec<ModelInfo>,
        crashed: Arc<AtomicBool>,
        opens: Arc<std::sync::atomic::AtomicUsize>,
        done: AssistantMessage,
    }

    impl CrashOnceFactory {
        fn new(done: AssistantMessage) -> Self {
            Self {
                models: vec![ModelInfo {
                    id: ModelId::from("crash-model"),
                    display_name: "Crash model".to_owned(),
                    context_tokens: None,
                    max_output_tokens: None,
                }],
                crashed: Arc::new(AtomicBool::new(false)),
                opens: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                done,
            }
        }
    }

    impl ProviderFactory for CrashOnceFactory {
        fn provider_id(&self) -> ProviderId {
            ProviderId::from("crash-once")
        }

        fn models(&self) -> &[ModelInfo] {
            &self.models
        }

        fn open(&self, _: SessionConfig) -> OpenProvider<'_> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            let provider = CrashOnceProvider {
                crashed: Arc::clone(&self.crashed),
                done: self.done.clone(),
            };
            Box::pin(async move { Ok(Box::new(provider) as Box<dyn Provider>) })
        }
    }

    struct CrashOnceProvider {
        crashed: Arc<AtomicBool>,
        done: AssistantMessage,
    }

    impl Provider for CrashOnceProvider {
        fn generate(&mut self, _: Request, _: CancellationToken) -> ProviderStream<'_> {
            assert!(
                self.crashed.swap(true, Ordering::SeqCst),
                "injected provider panic after the durable operation start"
            );
            Box::pin(futures_util::stream::iter([
                StreamEvent::Start,
                StreamEvent::Done(self.done.clone()),
            ]))
        }
    }

    async fn read_until<R>(reader: &mut BufReader<R>, predicate: impl Fn(&Value) -> bool) -> Value
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                assert!(
                    !line.is_empty(),
                    "RPC server closed before expected message"
                );
                let value: Value = serde_json::from_str(&line).unwrap();
                if predicate(&value) {
                    return value;
                }
            }
        })
        .await
        .unwrap()
    }
}
