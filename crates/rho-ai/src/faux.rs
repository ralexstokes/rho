//! Deterministic scripted provider factory and session for tests.

use std::{
    collections::VecDeque,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use futures_core::Stream;

use crate::{
    CancellationToken, ErrorKind, Message, ModelInfo, OpenProvider, Provider, ProviderError,
    ProviderFactory, ProviderId, ProviderStream, Request, SessionConfig, StreamEvent,
    ThinkingLevel, ToolDefinition,
};

/// Whether a scripted generation continued acknowledged state or rebased.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionDecision {
    /// The request extended the transcript acknowledged by the live session.
    Continue,
    /// The request could not safely continue the live session.
    Rebase,
}

/// One expected request and its scripted event sequence.
#[derive(Clone, Debug)]
pub struct Script {
    /// Exact request expected by the faux provider.
    pub request: Request,
    /// Events returned when the request matches.
    pub events: Vec<StreamEvent>,
}

/// Shared factory for deterministic scripted sessions.
#[derive(Clone, Debug)]
pub struct FauxFactory {
    provider_id: ProviderId,
    models: Vec<ModelInfo>,
    scripts: Arc<Mutex<VecDeque<Script>>>,
    decisions: Arc<Mutex<Vec<SessionDecision>>>,
}

impl FauxFactory {
    /// Creates a factory from an ordered script shared by opened sessions.
    #[must_use]
    pub fn new(models: Vec<ModelInfo>, scripts: impl IntoIterator<Item = Script>) -> Self {
        Self {
            provider_id: ProviderId::from("faux"),
            models,
            scripts: Arc::new(Mutex::new(scripts.into_iter().collect())),
            decisions: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Overrides the stable provider identity represented by this factory.
    #[must_use]
    pub fn with_provider_id(mut self, provider_id: ProviderId) -> Self {
        self.provider_id = provider_id;
        self
    }

    /// Returns how many scripted requests remain.
    #[must_use]
    pub fn remaining(&self) -> usize {
        match self.scripts.lock() {
            Ok(scripts) => scripts.len(),
            Err(poisoned) => poisoned.into_inner().len(),
        }
    }

    /// Returns the continue/rebase decisions recorded so far.
    #[must_use]
    pub fn decisions(&self) -> Vec<SessionDecision> {
        match self.decisions.lock() {
            Ok(decisions) => decisions.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn open_session(&self, config: SessionConfig) -> Result<FauxProvider, ProviderError> {
        if !self.models.iter().any(|model| model.id == config.model) {
            return Err(invalid_request(format!(
                "faux model {:?} is unsupported",
                config.model.as_str()
            )));
        }
        Ok(FauxProvider {
            config,
            scripts: Arc::clone(&self.scripts),
            decisions: Arc::clone(&self.decisions),
            acknowledged: Vec::new(),
            shape: None,
            poisoned: false,
        })
    }
}

impl ProviderFactory for FauxFactory {
    fn provider_id(&self) -> ProviderId {
        self.provider_id.clone()
    }

    fn models(&self) -> &[ModelInfo] {
        &self.models
    }

    fn open(&self, config: SessionConfig) -> OpenProvider<'_> {
        let result = self
            .open_session(config)
            .map(|provider| Box::new(provider) as Box<dyn Provider>);
        Box::pin(async move { result })
    }
}

/// One live deterministic scripted session.
#[derive(Debug)]
pub struct FauxProvider {
    config: SessionConfig,
    scripts: Arc<Mutex<VecDeque<Script>>>,
    decisions: Arc<Mutex<Vec<SessionDecision>>>,
    acknowledged: Vec<Message>,
    shape: Option<FauxShape>,
    poisoned: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct FauxShape {
    system: String,
    tools: Vec<ToolDefinition>,
    thinking: ThinkingLevel,
}

impl From<&Request> for FauxShape {
    fn from(request: &Request) -> Self {
        Self {
            system: request.system.clone(),
            tools: request.tools.clone(),
            thinking: request.thinking,
        }
    }
}

impl Provider for FauxProvider {
    fn generate(
        &mut self,
        request: Request,
        cancellation: CancellationToken,
    ) -> ProviderStream<'_> {
        let decision = if !self.poisoned
            && !self.acknowledged.is_empty()
            && self.shape.as_ref() == Some(&FauxShape::from(&request))
            && request.messages.starts_with(&self.acknowledged)
        {
            SessionDecision::Continue
        } else {
            SessionDecision::Rebase
        };
        match self.decisions.lock() {
            Ok(mut decisions) => decisions.push(decision),
            Err(poisoned) => poisoned.into_inner().push(decision),
        }

        // Until an authoritative Done is observed, dropping, cancelling, or
        // failing this stream leaves continuation state ambiguous.
        self.poisoned = true;
        let script = match self.scripts.lock() {
            Ok(mut scripts) => scripts.pop_front(),
            Err(poisoned) => poisoned.into_inner().pop_front(),
        };
        let events = match script {
            Some(script) if script.request == request => script.events,
            Some(script) => vec![StreamEvent::Error(ProviderError::invalid_response(
                format!(
                    "faux request mismatch for model {:?}: expected {:?}, received {:?}",
                    self.config.model.as_str(),
                    script.request,
                    request
                ),
            ))],
            None => vec![StreamEvent::Error(ProviderError::invalid_response(
                "faux provider script exhausted",
            ))],
        };
        Box::pin(FauxStream::new(self, request, events, cancellation))
    }
}

#[derive(Debug)]
struct FauxStream<'session> {
    provider: &'session mut FauxProvider,
    request: Request,
    events: VecDeque<StreamEvent>,
    cancellation: CancellationToken,
    terminal_emitted: bool,
}

impl<'session> FauxStream<'session> {
    fn new(
        provider: &'session mut FauxProvider,
        request: Request,
        events: impl IntoIterator<Item = StreamEvent>,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            provider,
            request,
            events: events.into_iter().collect(),
            cancellation,
            terminal_emitted: false,
        }
    }
}

impl Stream for FauxStream<'_> {
    type Item = StreamEvent;

    fn poll_next(mut self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.terminal_emitted {
            return Poll::Ready(None);
        }
        if self.cancellation.is_cancelled() {
            self.events.clear();
            self.terminal_emitted = true;
            return Poll::Ready(Some(StreamEvent::Error(ProviderError::cancelled())));
        }
        let event = self.events.pop_front().unwrap_or_else(|| {
            StreamEvent::Error(ProviderError::invalid_response(
                "faux provider script ended without Done or Error",
            ))
        });
        if let StreamEvent::Done(message) = &event {
            let mut acknowledged = self.request.messages.clone();
            acknowledged.push(Message::Assistant(message.clone()));
            self.provider.acknowledged = acknowledged;
            self.provider.shape = Some(FauxShape::from(&self.request));
            self.provider.poisoned = false;
        }
        if matches!(event, StreamEvent::Done(_) | StreamEvent::Error(_)) {
            self.events.clear();
            self.terminal_emitted = true;
        }
        Poll::Ready(Some(event))
    }
}

fn invalid_request(message: impl Into<String>) -> ProviderError {
    ProviderError {
        retryable: false,
        kind: ErrorKind::InvalidRequest,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::{pin::pin, task::Poll};

    use crate::{AssistantMessage, Message, ModelId, ProviderId, StopReason, ThinkingLevel, Usage};

    use super::*;

    fn model() -> ModelInfo {
        ModelInfo {
            id: ModelId::from("faux-1"),
            display_name: "Faux".to_owned(),
            context_tokens: None,
            max_output_tokens: None,
        }
    }

    fn config() -> SessionConfig {
        SessionConfig { model: model().id }
    }

    fn request(messages: Vec<Message>) -> Request {
        Request {
            system: "test".to_owned(),
            messages,
            tools: Vec::new(),
            max_output_tokens: 10,
            thinking: ThinkingLevel::None,
        }
    }

    fn done(text: &str) -> AssistantMessage {
        AssistantMessage {
            blocks: vec![crate::ContentBlock::text(text)],
            stop: StopReason::Stop,
            usage: Usage::default(),
            provider: ProviderId::from("faux"),
            model: ModelId::from("faux-1"),
        }
    }

    fn poll_event(stream: &mut Pin<&mut ProviderStream<'_>>) -> Poll<Option<StreamEvent>> {
        let waker = std::task::Waker::noop();
        let mut context = Context::from_waker(waker);
        stream.as_mut().poll_next(&mut context)
    }

    #[test]
    fn exact_script_is_consumed_deterministically() {
        let initial = request(vec![Message::user("hello")]);
        let factory = FauxFactory::new(
            vec![model()],
            [Script {
                request: initial.clone(),
                events: vec![StreamEvent::Start],
            }],
        );
        let mut provider = factory.open_session(config()).unwrap();
        let mut stream = pin!(provider.generate(initial, CancellationToken::new()));
        assert!(matches!(
            poll_event(&mut stream),
            Poll::Ready(Some(StreamEvent::Start))
        ));
        assert_eq!(factory.remaining(), 0);
        assert_eq!(factory.models()[0].id, ModelId::from("faux-1"));
        assert_eq!(factory.decisions(), [SessionDecision::Rebase]);
    }

    #[test]
    fn prefix_continues_but_branch_and_error_rebase() {
        let initial = request(vec![Message::user("hello")]);
        let first_done = done("first");
        let mut continued_messages = initial.messages.clone();
        continued_messages.push(Message::Assistant(first_done.clone()));
        continued_messages.push(Message::user("continue"));
        let continued = request(continued_messages);
        let branch = request(vec![Message::user("branch")]);
        let retry = request(vec![Message::user("retry")]);
        let factory = FauxFactory::new(
            vec![model()],
            [
                Script {
                    request: initial.clone(),
                    events: vec![StreamEvent::Done(first_done)],
                },
                Script {
                    request: continued.clone(),
                    events: vec![StreamEvent::Done(done("continued"))],
                },
                Script {
                    request: branch.clone(),
                    events: vec![StreamEvent::Error(ProviderError::invalid_response(
                        "failed",
                    ))],
                },
                Script {
                    request: retry.clone(),
                    events: vec![StreamEvent::Done(done("retried"))],
                },
            ],
        );
        let mut provider = factory.open_session(config()).unwrap();

        for request in [initial, continued, branch, retry] {
            let mut stream = pin!(provider.generate(request, CancellationToken::new()));
            assert!(matches!(
                poll_event(&mut stream),
                Poll::Ready(Some(StreamEvent::Done(_) | StreamEvent::Error(_)))
            ));
        }

        assert_eq!(
            factory.decisions(),
            [
                SessionDecision::Rebase,
                SessionDecision::Continue,
                SessionDecision::Rebase,
                SessionDecision::Rebase,
            ]
        );
    }

    #[test]
    fn continue_and_rebase_are_authoritatively_equivalent() {
        let initial = request(vec![Message::user("hello")]);
        let first_done = done("first");
        let mut target_messages = initial.messages.clone();
        target_messages.push(Message::Assistant(first_done.clone()));
        target_messages.push(Message::user("continue"));
        let target = request(target_messages);
        let target_done = done("same authoritative result");
        let factory = FauxFactory::new(
            vec![model()],
            [
                Script {
                    request: initial.clone(),
                    events: vec![StreamEvent::Done(first_done)],
                },
                Script {
                    request: target.clone(),
                    events: vec![StreamEvent::Done(target_done.clone())],
                },
                Script {
                    request: target.clone(),
                    events: vec![StreamEvent::Done(target_done)],
                },
            ],
        );

        let mut continued_provider = factory.open_session(config()).unwrap();
        {
            let mut stream = pin!(continued_provider.generate(initial, CancellationToken::new()));
            assert!(matches!(
                poll_event(&mut stream),
                Poll::Ready(Some(StreamEvent::Done(_)))
            ));
        }
        let continued_done = {
            let mut stream =
                pin!(continued_provider.generate(target.clone(), CancellationToken::new()));
            match poll_event(&mut stream) {
                Poll::Ready(Some(StreamEvent::Done(message))) => message,
                event => panic!("expected continued Done, received {event:?}"),
            }
        };

        let mut rebased_provider = factory.open_session(config()).unwrap();
        let rebased_done = {
            let mut stream = pin!(rebased_provider.generate(target, CancellationToken::new()));
            match poll_event(&mut stream) {
                Poll::Ready(Some(StreamEvent::Done(message))) => message,
                event => panic!("expected rebased Done, received {event:?}"),
            }
        };

        assert_eq!(continued_done, rebased_done);
        assert_eq!(
            factory.decisions(),
            [
                SessionDecision::Rebase,
                SessionDecision::Continue,
                SessionDecision::Rebase,
            ]
        );
    }

    #[test]
    fn cancellation_poisoning_forces_the_next_generation_to_rebase() {
        let initial = request(vec![Message::user("hello")]);
        let retry = request(vec![Message::user("retry")]);
        let factory = FauxFactory::new(
            vec![model()],
            [
                Script {
                    request: initial.clone(),
                    events: vec![StreamEvent::Start, StreamEvent::Done(done("ignored"))],
                },
                Script {
                    request: retry.clone(),
                    events: vec![StreamEvent::Done(done("ok"))],
                },
            ],
        );
        let mut provider = factory.open_session(config()).unwrap();
        let cancellation = CancellationToken::new();
        {
            let mut stream = pin!(provider.generate(initial, cancellation.clone()));
            assert!(matches!(
                poll_event(&mut stream),
                Poll::Ready(Some(StreamEvent::Start))
            ));
            cancellation.cancel();
            assert!(matches!(
                poll_event(&mut stream),
                Poll::Ready(Some(StreamEvent::Error(ProviderError {
                    kind: ErrorKind::Cancelled,
                    ..
                })))
            ));
        }

        let mut stream = pin!(provider.generate(retry, CancellationToken::new()));
        assert!(matches!(
            poll_event(&mut stream),
            Poll::Ready(Some(StreamEvent::Done(_)))
        ));
        assert_eq!(
            factory.decisions(),
            [SessionDecision::Rebase, SessionDecision::Rebase]
        );
    }

    #[test]
    fn a_scripted_terminal_event_is_final_even_if_cancelled_later() {
        let request = request(vec![Message::user("hello")]);
        let factory = FauxFactory::new(
            vec![model()],
            [Script {
                request: request.clone(),
                events: vec![StreamEvent::Error(ProviderError::invalid_response("stop"))],
            }],
        );
        let mut provider = factory.open_session(config()).unwrap();
        let cancellation = CancellationToken::new();
        let mut stream = pin!(provider.generate(request, cancellation.clone()));
        assert!(matches!(
            poll_event(&mut stream),
            Poll::Ready(Some(StreamEvent::Error(_)))
        ));
        cancellation.cancel();
        assert!(matches!(poll_event(&mut stream), Poll::Ready(None)));
    }

    #[test]
    fn an_unterminated_script_becomes_a_terminal_boundary_error() {
        let request = request(vec![Message::user("hello")]);
        let factory = FauxFactory::new(
            vec![model()],
            [Script {
                request: request.clone(),
                events: vec![StreamEvent::Start],
            }],
        );
        let mut provider = factory.open_session(config()).unwrap();
        let mut stream = pin!(provider.generate(request, CancellationToken::new()));
        assert!(matches!(
            poll_event(&mut stream),
            Poll::Ready(Some(StreamEvent::Start))
        ));
        assert!(matches!(
            poll_event(&mut stream),
            Poll::Ready(Some(StreamEvent::Error(ProviderError {
                kind: ErrorKind::InvalidResponse,
                ..
            })))
        ));
        assert!(matches!(poll_event(&mut stream), Poll::Ready(None)));
    }
}
