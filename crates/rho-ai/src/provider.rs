use std::pin::Pin;

use futures_core::Stream;

use crate::{CancellationToken, ModelInfo, Request, StreamEvent};

/// Type-erased stream returned by provider adapters.
pub type ProviderStream = Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>>;

/// Stateless transcript provider boundary.
pub trait Provider: Send + Sync {
    /// Models this configured adapter can serve.
    fn models(&self) -> &[ModelInfo];

    /// Streams one request. The request contains the complete authoritative transcript.
    fn stream(&self, request: Request, cancellation: CancellationToken) -> ProviderStream;
}
