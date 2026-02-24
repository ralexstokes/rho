use std::sync::{Arc, Mutex, OnceLock};

use futures_channel::oneshot;
use futures_util::{FutureExt, StreamExt};
use rig::{
    client::completion::CompletionClient, completion::CompletionModel, providers::anthropic,
};

use crate::{
    Message, MessageRole,
    providers::{
        Provider, ProviderCancelHandle, ProviderError, ProviderKind, ProviderRequest,
        ProviderStream, apply_common_request_options, map_rig_completion_error, map_rig_http_error,
        map_streamed_assistant_chunk, rig_choice_text, to_rig_chat_request, validate_api_key,
    },
    stream::ProviderEvent,
};

const ANTHROPIC_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";

#[derive(Debug, Clone, Default)]
pub struct AnthropicProvider {
    client: Arc<OnceLock<anthropic::Client>>,
}

impl AnthropicProvider {
    pub fn new() -> Self {
        Self::default()
    }

    fn client(&self) -> Result<anthropic::Client, ProviderError> {
        if let Some(client) = self.client.get() {
            return Ok(client.clone());
        }

        let api_key = anthropic_api_key()?;
        let client = anthropic_client(api_key)?;
        let _ = self.client.set(client);

        Ok(self
            .client
            .get()
            .expect("anthropic client should be initialized")
            .clone())
    }
}

impl Provider for AnthropicProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Anthropic
    }

    fn stream(&self, request: ProviderRequest<'_>) -> ProviderStream {
        let rig_request = to_rig_chat_request(request);
        let client = self.client();
        let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
        let cancel_tx = Arc::new(Mutex::new(Some(cancel_tx)));
        let cancel_handle = ProviderCancelHandle::new(move || {
            if let Some(cancel_tx) = cancel_tx
                .lock()
                .expect("cancel sender mutex should be available")
                .take()
            {
                let _ = cancel_tx.send(());
            }
        });

        let stream = async_stream::try_stream! {
            let client = client?;
            let rig_request = rig_request?;

            let model = client
                .completion_model(rig_request.model.clone())
                .with_prompt_caching();
            let builder = model
                .completion_request(rig_request.prompt)
                .messages(rig_request.history);
            let builder =
                apply_common_request_options(builder, rig_request.preamble, rig_request.tools);

            let mut stream = builder
                .stream()
                .await
                .map_err(|error| map_rig_completion_error(ANTHROPIC_API_KEY_ENV, error))?;
            let mut cancel_rx = cancel_rx.fuse();
            let mut cancelled = false;

            loop {
                let next_chunk = futures_util::select! {
                    _ = cancel_rx => {
                        cancelled = true;
                        stream.cancel();
                        None
                    }
                    next_chunk = stream.next().fuse() => next_chunk,
                };

                let Some(next_chunk) = next_chunk else { break };
                let chunk = next_chunk
                    .map_err(|error| map_rig_completion_error(ANTHROPIC_API_KEY_ENV, error))?;
                if let Some(event) = map_streamed_assistant_chunk(chunk) {
                    yield event;
                }
            }

            if !cancelled {
                let message = Message::new(MessageRole::Assistant, rig_choice_text(&stream.choice));
                yield ProviderEvent::Message { message };
                yield ProviderEvent::Finished;
            }
        };

        ProviderStream::with_cancel_handle(Box::pin(stream), cancel_handle)
    }
}

fn anthropic_api_key() -> Result<String, ProviderError> {
    validate_api_key(
        ANTHROPIC_API_KEY_ENV,
        std::env::var(ANTHROPIC_API_KEY_ENV).ok(),
    )
}

fn anthropic_client(api_key: String) -> Result<anthropic::Client, ProviderError> {
    anthropic::Client::builder()
        .api_key(api_key)
        .build()
        .map_err(|error| map_rig_http_error(ANTHROPIC_API_KEY_ENV, error))
}
