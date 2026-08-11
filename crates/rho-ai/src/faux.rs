//! Deterministic scripted provider for tests and manually driven hosts.

use std::{
    collections::VecDeque,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use futures_core::Stream;

use crate::{
    CancellationToken, ModelInfo, Provider, ProviderError, ProviderStream, Request, StreamEvent,
};

/// One expected request and its scripted event sequence.
#[derive(Clone, Debug)]
pub struct Script {
    /// Exact request expected by the faux provider.
    pub request: Request,
    /// Events returned when the request matches.
    pub events: Vec<StreamEvent>,
}

/// Deterministic provider that consumes exact scripts in order.
#[derive(Clone, Debug)]
pub struct FauxProvider {
    models: Vec<ModelInfo>,
    scripts: Arc<Mutex<VecDeque<Script>>>,
}

impl FauxProvider {
    /// Creates a provider from an ordered script.
    #[must_use]
    pub fn new(models: Vec<ModelInfo>, scripts: impl IntoIterator<Item = Script>) -> Self {
        Self {
            models,
            scripts: Arc::new(Mutex::new(scripts.into_iter().collect())),
        }
    }

    /// Returns how many scripted requests remain.
    #[must_use]
    pub fn remaining(&self) -> usize {
        match self.scripts.lock() {
            Ok(scripts) => scripts.len(),
            Err(poisoned) => poisoned.into_inner().len(),
        }
    }
}

impl Provider for FauxProvider {
    fn models(&self) -> &[ModelInfo] {
        &self.models
    }

    fn stream(&self, request: Request, cancellation: CancellationToken) -> ProviderStream {
        let script = match self.scripts.lock() {
            Ok(mut scripts) => scripts.pop_front(),
            Err(poisoned) => poisoned.into_inner().pop_front(),
        };
        let events = match script {
            Some(script) if script.request == request => script.events,
            Some(script) => vec![StreamEvent::Error(ProviderError::invalid_response(
                format!(
                    "faux request mismatch: expected {:?}, received {:?}",
                    script.request, request
                ),
            ))],
            None => vec![StreamEvent::Error(ProviderError::invalid_response(
                "faux provider script exhausted",
            ))],
        };
        Box::pin(FauxStream::from_events(events, cancellation))
    }
}

#[derive(Debug)]
struct FauxStream {
    events: VecDeque<StreamEvent>,
    cancellation: CancellationToken,
    terminal_emitted: bool,
}

impl FauxStream {
    fn from_events(
        events: impl IntoIterator<Item = StreamEvent>,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            events: events.into_iter().collect(),
            cancellation,
            terminal_emitted: false,
        }
    }
}

impl Stream for FauxStream {
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
        if matches!(event, StreamEvent::Done(_) | StreamEvent::Error(_)) {
            self.events.clear();
            self.terminal_emitted = true;
        }
        Poll::Ready(Some(event))
    }
}

#[cfg(test)]
mod tests {
    use std::{pin::pin, task::Poll};

    use crate::{Message, ModelId, ProviderId, Request, ThinkingLevel};

    use super::*;

    fn request() -> Request {
        Request {
            system: "test".to_owned(),
            messages: vec![Message::user("hello")],
            tools: Vec::new(),
            model: ModelId::from("faux-1"),
            max_output_tokens: 10,
            thinking: ThinkingLevel::None,
        }
    }

    #[test]
    fn exact_script_is_consumed_deterministically() {
        let provider = FauxProvider::new(
            vec![ModelInfo {
                id: ModelId::from("faux-1"),
                display_name: "Faux".to_owned(),
                context_tokens: None,
                max_output_tokens: None,
            }],
            [Script {
                request: request(),
                events: vec![StreamEvent::Start],
            }],
        );
        let mut stream = pin!(provider.stream(request(), CancellationToken::new()));
        let waker = std::task::Waker::noop();
        let mut context = Context::from_waker(waker);
        assert!(matches!(
            stream.as_mut().poll_next(&mut context),
            Poll::Ready(Some(StreamEvent::Start))
        ));
        assert_eq!(provider.remaining(), 0);
        assert_eq!(provider.models()[0].id, ModelId::from("faux-1"));
        assert_eq!(ProviderId::from("faux").as_str(), "faux");
    }

    #[test]
    fn cancellation_interrupts_a_partially_consumed_script() {
        let provider = FauxProvider::new(
            Vec::new(),
            [Script {
                request: request(),
                events: vec![StreamEvent::Start, StreamEvent::Start],
            }],
        );
        let cancellation = CancellationToken::new();
        let mut stream = pin!(provider.stream(request(), cancellation.clone()));
        let waker = std::task::Waker::noop();
        let mut context = Context::from_waker(waker);
        assert!(matches!(
            stream.as_mut().poll_next(&mut context),
            Poll::Ready(Some(StreamEvent::Start))
        ));
        cancellation.cancel();
        assert!(matches!(
            stream.as_mut().poll_next(&mut context),
            Poll::Ready(Some(StreamEvent::Error(ProviderError {
                kind: crate::ErrorKind::Cancelled,
                ..
            })))
        ));
        assert!(matches!(
            stream.as_mut().poll_next(&mut context),
            Poll::Ready(None)
        ));
    }

    #[test]
    fn a_scripted_terminal_event_is_final_even_if_cancelled_later() {
        let provider = FauxProvider::new(
            Vec::new(),
            [Script {
                request: request(),
                events: vec![StreamEvent::Error(ProviderError::invalid_response("stop"))],
            }],
        );
        let cancellation = CancellationToken::new();
        let mut stream = pin!(provider.stream(request(), cancellation.clone()));
        let waker = std::task::Waker::noop();
        let mut context = Context::from_waker(waker);
        assert!(matches!(
            stream.as_mut().poll_next(&mut context),
            Poll::Ready(Some(StreamEvent::Error(_)))
        ));

        cancellation.cancel();
        assert!(matches!(
            stream.as_mut().poll_next(&mut context),
            Poll::Ready(None)
        ));
    }

    #[test]
    fn an_unterminated_script_becomes_a_terminal_boundary_error() {
        let provider = FauxProvider::new(
            Vec::new(),
            [Script {
                request: request(),
                events: vec![StreamEvent::Start],
            }],
        );
        let mut stream = pin!(provider.stream(request(), CancellationToken::new()));
        let waker = std::task::Waker::noop();
        let mut context = Context::from_waker(waker);
        assert!(matches!(
            stream.as_mut().poll_next(&mut context),
            Poll::Ready(Some(StreamEvent::Start))
        ));
        assert!(matches!(
            stream.as_mut().poll_next(&mut context),
            Poll::Ready(Some(StreamEvent::Error(ProviderError {
                kind: crate::ErrorKind::InvalidResponse,
                ..
            })))
        ));
        assert!(matches!(
            stream.as_mut().poll_next(&mut context),
            Poll::Ready(None)
        ));
    }
}
