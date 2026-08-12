use std::{future::Future, pin::Pin};

use futures_core::Stream;

use crate::{
    CancellationToken, ModelInfo, ProviderError, ProviderId, Request, SessionConfig, StreamEvent,
};

/// Type-erased stream returned by provider adapters.
pub type ProviderStream<'session> = Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'session>>;

/// Type-erased future returned when a factory opens a provider session.
pub type OpenProvider<'factory> =
    Pin<Box<dyn Future<Output = Result<Box<dyn Provider>, ProviderError>> + Send + 'factory>>;

/// Shared provider configuration, credentials, and model catalog.
pub trait ProviderFactory: Send + Sync {
    /// Stable adapter identity used to resolve durable model selections.
    fn provider_id(&self) -> ProviderId;

    /// Models this factory can open.
    fn models(&self) -> &[ModelInfo];

    /// Opens one live logical model session.
    fn open(&self, config: SessionConfig) -> OpenProvider<'_>;
}

/// One live logical model session owned by a rho session.
pub trait Provider: Send {
    /// Generates from the complete authoritative transcript.
    ///
    /// The returned stream borrows this session, serializing generations by
    /// construction.
    fn generate(&mut self, request: Request, cancellation: CancellationToken)
    -> ProviderStream<'_>;
}
