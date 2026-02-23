use std::pin::Pin;

use futures_core::Stream;
use thiserror::Error;

use crate::{message::Message, stream::ProviderEvent};

pub mod anthropic;
pub mod openai;

pub type ProviderStream = Pin<Box<dyn Stream<Item = Result<ProviderEvent, ProviderError>> + Send>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProviderKind {
    OpenAi,
    Anthropic,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRequest {
    pub model: String,
    pub messages: Vec<Message>,
}

impl ProviderRequest {
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            messages,
        }
    }
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("missing API key in environment variable {0}")]
    MissingApiKey(&'static str),
    #[error("invalid API key in environment variable {0}")]
    InvalidApiKey(&'static str),
    #[error("provider {0} is not implemented")]
    NotImplemented(&'static str),
    #[error("provider transport error: {0}")]
    Transport(String),
}

pub(crate) fn validate_api_key(
    env_var: &'static str,
    api_key: Option<String>,
) -> Result<String, ProviderError> {
    let api_key = api_key.ok_or(ProviderError::MissingApiKey(env_var))?;
    if api_key.trim().is_empty() {
        return Err(ProviderError::InvalidApiKey(env_var));
    }
    Ok(api_key)
}

pub trait Provider: Send + Sync {
    fn kind(&self) -> ProviderKind;

    fn stream(&self, request: ProviderRequest) -> ProviderStream;
}
