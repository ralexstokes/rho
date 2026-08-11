//! Provider-independent AI boundary types, validation, and provider traits.
//!
//! This crate is a pure core: it performs no I/O and observes no ambient state.

mod cancellation;
mod credentials;
pub mod faux;
mod provider;
mod types;
mod validation;

pub use cancellation::{CancellationToken, Cancelled};
pub use credentials::{
    Credential, CredentialError, CredentialSource, CredentialStore, StoredCredential,
};
pub use provider::{Provider, ProviderStream};
pub use types::{
    AssistantMessage, ContentBlock, DeltaKind, ErrorKind, Message, ModelId, ModelInfo, OpaqueBlob,
    ProviderError, ProviderId, Request, StopReason, StreamEvent, ThinkingLevel, ToolArgumentError,
    ToolCallId, ToolDefinition, ToolResult, Usage,
};
pub use validation::{SchemaError, validate_tool_arguments, validate_tool_definition};
