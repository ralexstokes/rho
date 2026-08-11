//! Anthropic Messages provider adapter.
//!
//! HTTP lives in the provider shell while [`decoder`] and response assembly
//! remain deterministic byte/value transformations with fixture-driven tests.

pub mod decoder;
mod response;

use std::{collections::BTreeSet, sync::Arc};

use async_stream::stream;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use futures_util::StreamExt as _;
use reqwest::StatusCode;
use rho_ai::{
    CancellationToken, ContentBlock, CredentialSource, ErrorKind, Message, ModelId, ModelInfo,
    OpaqueBlob, OpenProvider, Provider, ProviderError, ProviderFactory, ProviderId, ProviderStream,
    Request, SessionConfig, StreamEvent, ThinkingLevel, validate_tool_definition,
};
use serde_json::{Value, json};

use crate::{decoder::Decoder, response::ResponseAssembler};

const PROVIDER: &str = "anthropic";
const API_VERSION: &str = "2023-06-01";

/// Anthropic factory construction failure.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    /// HTTP client construction failed.
    #[error("failed to build Anthropic HTTP client: {0}")]
    Http(#[from] reqwest::Error),
}

/// Shared Anthropic configuration, credentials, and model catalog.
#[derive(Clone)]
pub struct AnthropicFactory {
    credentials: Arc<dyn CredentialSource>,
    base_url: String,
    client: reqwest::Client,
    models: Vec<ModelInfo>,
}

impl std::fmt::Debug for AnthropicFactory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AnthropicFactory")
            .field("credentials", &"[REDACTED]")
            .field("base_url", &self.base_url)
            .field("models", &self.models)
            .finish()
    }
}

impl AnthropicFactory {
    /// Creates a factory for the public Anthropic API.
    pub fn new(credentials: Arc<dyn CredentialSource>) -> Result<Self, BuildError> {
        Self::with_base_url(credentials, "https://api.anthropic.com")
    }

    /// Creates a factory with a custom base URL, primarily for compatible gateways and tests.
    pub fn with_base_url(
        credentials: Arc<dyn CredentialSource>,
        base_url: impl Into<String>,
    ) -> Result<Self, BuildError> {
        Ok(Self {
            credentials,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            client: http_client()?,
            models: current_models(),
        })
    }
}

impl ProviderFactory for AnthropicFactory {
    fn models(&self) -> &[ModelInfo] {
        &self.models
    }

    fn open(&self, config: SessionConfig) -> OpenProvider<'_> {
        let result = (|| {
            if !self.models.iter().any(|model| model.id == config.model) {
                return Err(invalid_request(format!(
                    "Anthropic model {:?} is unsupported",
                    config.model.as_str()
                )));
            }
            let credential = self
                .credentials
                .resolve(&ProviderId::from(PROVIDER))
                .map_err(authentication_error)?;
            Ok(Box::new(AnthropicProvider {
                api_key: credential.expose_api_key().to_owned(),
                base_url: self.base_url.clone(),
                client: self.client.clone(),
                model: config.model,
            }) as Box<dyn Provider>)
        })();
        Box::pin(async move { result })
    }
}

/// One logical Anthropic model session.
///
/// Messages API requests are always rebased from rho's complete transcript.
struct AnthropicProvider {
    api_key: String,
    base_url: String,
    client: reqwest::Client,
    model: ModelId,
}

impl std::fmt::Debug for AnthropicProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AnthropicProvider")
            .field("api_key", &"[REDACTED]")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .finish()
    }
}

fn http_client() -> Result<reqwest::Client, reqwest::Error> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add_parsable_certificates(webpki_root_certs::TLS_SERVER_ROOT_CERTS.iter().cloned());
    let tls = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .build()
}

impl Provider for AnthropicProvider {
    fn generate(
        &mut self,
        request: Request,
        cancellation: CancellationToken,
    ) -> ProviderStream<'_> {
        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let model = self.model.clone();
        let endpoint = format!("{}/v1/messages", self.base_url);
        Box::pin(stream! {
            if cancellation.is_cancelled() {
                yield StreamEvent::Error(ProviderError::cancelled());
                return;
            }
            let body = match request_body(&request, &model) {
                Ok(body) => body,
                Err(error) => {
                    yield StreamEvent::Error(error);
                    return;
                }
            };
            let send = client
                .post(endpoint)
                .header("x-api-key", api_key)
                .header("anthropic-version", API_VERSION)
                .json(&body)
                .send();
            let response = tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    yield StreamEvent::Error(ProviderError::cancelled());
                    return;
                }
                response = send => match response {
                    Ok(response) => response,
                    Err(error) => {
                        yield StreamEvent::Error(classify_transport_error(&error));
                        return;
                    }
                }
            };

            if !response.status().is_success() {
                let status = response.status();
                let text = tokio::select! {
                    biased;
                    () = cancellation.cancelled() => {
                        yield StreamEvent::Error(ProviderError::cancelled());
                        return;
                    }
                    text = response.text() => text.unwrap_or_else(|error| error.to_string()),
                };
                yield StreamEvent::Error(classify_http_error(status, text));
                return;
            }

            let mut bytes = response.bytes_stream();
            let mut decoder = Decoder::new();
            let mut assembler = ResponseAssembler::new(model, request.tools.clone());
            let mut terminal = false;
            loop {
                let next = tokio::select! {
                    biased;
                    () = cancellation.cancelled() => {
                        yield StreamEvent::Error(ProviderError::cancelled());
                        return;
                    }
                    next = bytes.next() => next,
                };
                let Some(next) = next else { break };
                let chunk = match next {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        yield StreamEvent::Error(classify_transport_error(&error));
                        return;
                    }
                };
                let decoded = match decoder.feed(&chunk) {
                    Ok(decoded) => decoded,
                    Err(error) => {
                        yield StreamEvent::Error(ProviderError::invalid_response(error.to_string()));
                        return;
                    }
                };
                for event in decoded {
                    if cancellation.is_cancelled() {
                        yield StreamEvent::Error(ProviderError::cancelled());
                        return;
                    }
                    let events = match assembler.accept(event) {
                        Ok(events) => events,
                        Err(error) => {
                            yield StreamEvent::Error(error);
                            return;
                        }
                    };
                    for event in events {
                        if cancellation.is_cancelled() {
                            yield StreamEvent::Error(ProviderError::cancelled());
                            return;
                        }
                        terminal |= matches!(event, StreamEvent::Done(_) | StreamEvent::Error(_));
                        yield event;
                    }
                    if terminal {
                        return;
                    }
                }
            }

            if cancellation.is_cancelled() {
                yield StreamEvent::Error(ProviderError::cancelled());
                return;
            }
            let decoded = match decoder.finish() {
                Ok(decoded) => decoded,
                Err(error) => {
                    yield StreamEvent::Error(ProviderError::invalid_response(error.to_string()));
                    return;
                }
            };
            for event in decoded {
                if cancellation.is_cancelled() {
                    yield StreamEvent::Error(ProviderError::cancelled());
                    return;
                }
                let events = match assembler.accept(event) {
                    Ok(events) => events,
                    Err(error) => {
                        yield StreamEvent::Error(error);
                        return;
                    }
                };
                for event in events {
                    if cancellation.is_cancelled() {
                        yield StreamEvent::Error(ProviderError::cancelled());
                        return;
                    }
                    terminal |= matches!(event, StreamEvent::Done(_) | StreamEvent::Error(_));
                    yield event;
                    if terminal {
                        return;
                    }
                }
            }
            if !terminal {
                yield StreamEvent::Error(ProviderError::invalid_response(
                    "Anthropic stream ended before message_stop",
                ));
            }
        })
    }
}

/// Pure request projection used by the HTTP shell and fixture tests.
pub fn request_body(request: &Request, model: &ModelId) -> Result<Value, ProviderError> {
    if request.system.trim().is_empty() {
        return Err(invalid_request("system instructions must not be empty"));
    }
    if request.max_output_tokens == 0 {
        return Err(invalid_request(
            "max_output_tokens must be greater than zero",
        ));
    }
    if model.as_str() == "claude-fable-5" && request.thinking == ThinkingLevel::None {
        return Err(invalid_request(
            "claude-fable-5 does not support disabling adaptive thinking",
        ));
    }
    let mut tool_names = BTreeSet::new();
    for tool in &request.tools {
        validate_tool_definition(tool)
            .map_err(|error| invalid_request(format!("tool {}: {error}", tool.name)))?;
        if !tool_names.insert(tool.name.as_str()) {
            return Err(invalid_request(format!(
                "duplicate tool definition {:?}",
                tool.name
            )));
        }
    }

    let messages = request
        .messages
        .iter()
        .map(message_value)
        .collect::<Result<Vec<_>, _>>()?;
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.parameters,
            })
        })
        .collect::<Vec<_>>();
    let mut body = json!({
        "model": model.as_str(),
        "max_tokens": request.max_output_tokens,
        "system": request.system,
        "messages": messages,
        "tools": tools,
        "stream": true,
    });
    let object = body.as_object_mut().expect("JSON object literal");
    match request.thinking {
        ThinkingLevel::None => {
            object.insert("thinking".to_owned(), json!({"type": "disabled"}));
        }
        level => {
            object.insert(
                "thinking".to_owned(),
                json!({"type": "adaptive", "display": "summarized"}),
            );
            object.insert(
                "output_config".to_owned(),
                json!({"effort": level.as_str()}),
            );
        }
    }
    Ok(body)
}

fn message_value(message: &Message) -> Result<Value, ProviderError> {
    match message {
        Message::User { content } => Ok(json!({
            "role": "user",
            "content": content
                .iter()
                .map(user_content_value)
                .collect::<Result<Vec<_>, _>>()?,
        })),
        Message::Assistant(message) => Ok(json!({
            "role": "assistant",
            "content": message
                .blocks
                .iter()
                .map(assistant_content_value)
                .collect::<Result<Vec<_>, _>>()?,
        })),
        Message::ToolResult(result) => Ok(json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": result.call_id.as_str(),
                "content": result.content,
                "is_error": result.is_error,
            }],
        })),
        _ => Err(invalid_request("unsupported transcript message variant")),
    }
}

fn user_content_value(block: &ContentBlock) -> Result<Value, ProviderError> {
    match block {
        ContentBlock::Text { text } => Ok(json!({"type": "text", "text": text})),
        ContentBlock::Image { data, mime } => Ok(json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": mime,
                "data": BASE64.encode(data),
            }
        })),
        _ => Err(invalid_request(
            "user messages may contain only text and image blocks",
        )),
    }
}

fn assistant_content_value(block: &ContentBlock) -> Result<Value, ProviderError> {
    match block {
        ContentBlock::Text { text } => Ok(json!({"type": "text", "text": text})),
        ContentBlock::Thinking { text, opaque } => Ok(anthropic_thinking_value(text, opaque)),
        ContentBlock::ToolCall { id, name, args } => Ok(json!({
            "type": "tool_use",
            "id": id.as_str(),
            "name": name,
            "input": args,
        })),
        ContentBlock::RejectedToolCall {
            id,
            name,
            args: Some(args),
            ..
        } => Ok(json!({
            "type": "tool_use",
            "id": id.as_str(),
            "name": name,
            "input": args,
        })),
        ContentBlock::RejectedToolCall {
            id,
            name,
            args: None,
            ..
        } => Ok(json!({
            "type": "tool_use",
            "id": id.as_str(),
            "name": name,
            "input": {},
        })),
        ContentBlock::Image { .. } => Err(invalid_request(
            "Anthropic assistant history cannot contain image blocks",
        )),
        _ => Err(invalid_request(
            "unsupported assistant content block variant",
        )),
    }
}

fn anthropic_thinking_value(text: &str, opaque: &Option<OpaqueBlob>) -> Value {
    match opaque {
        Some(opaque)
            if opaque.provider == ProviderId::from(PROVIDER)
                && opaque.kind == "redacted_thinking" =>
        {
            json!({"type": "redacted_thinking", "data": opaque.data})
        }
        Some(opaque)
            if opaque.provider == ProviderId::from(PROVIDER) && opaque.kind == "signature" =>
        {
            json!({"type": "thinking", "thinking": text, "signature": opaque.data})
        }
        _ => json!({"type": "text", "text": text}),
    }
}

fn current_models() -> Vec<ModelInfo> {
    [
        ("claude-fable-5", "Claude Fable 5", 1_000_000, 128_000),
        ("claude-opus-5", "Claude Opus 5", 1_000_000, 128_000),
        ("claude-sonnet-5", "Claude Sonnet 5", 1_000_000, 128_000),
    ]
    .into_iter()
    .map(
        |(id, display_name, context_tokens, max_output_tokens)| ModelInfo {
            id: ModelId::from(id),
            display_name: display_name.to_owned(),
            context_tokens: Some(context_tokens),
            max_output_tokens: Some(max_output_tokens),
        },
    )
    .collect()
}

fn invalid_request(message: impl Into<String>) -> ProviderError {
    ProviderError {
        retryable: false,
        kind: ErrorKind::InvalidRequest,
        message: message.into(),
    }
}

fn authentication_error(error: rho_ai::CredentialError) -> ProviderError {
    ProviderError {
        retryable: false,
        kind: ErrorKind::Authentication,
        message: error.to_string(),
    }
}

fn classify_transport_error(error: &reqwest::Error) -> ProviderError {
    ProviderError {
        retryable: error.is_timeout() || error.is_connect() || error.is_body(),
        kind: ErrorKind::Transport,
        message: error.to_string(),
    }
}

fn classify_http_error(status: StatusCode, body: String) -> ProviderError {
    let (kind, retryable) = match status.as_u16() {
        401 | 403 => (ErrorKind::Authentication, false),
        408 | 409 | 429 => (ErrorKind::RateLimited, true),
        529 => (ErrorKind::Overloaded, true),
        500..=599 => (ErrorKind::Transport, true),
        _ => (ErrorKind::InvalidRequest, false),
    };
    ProviderError {
        retryable,
        kind,
        message: format!("Anthropic HTTP {status}: {body}"),
    }
}

#[cfg(test)]
mod tests {
    use rho_ai::{AssistantMessage, StopReason, ToolCallId, ToolDefinition, ToolResult, Usage};

    use super::*;

    fn recorded_done() -> AssistantMessage {
        let fixture = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-sonnet-5\",",
            "\"usage\":{\"input_tokens\":8,\"output_tokens\":1}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,",
            "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,",
            "\"delta\":{\"type\":\"text_delta\",\"text\":\"answer\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},",
            "\"usage\":{\"output_tokens\":3}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let mut decoder = Decoder::new();
        let mut assembler = ResponseAssembler::new(ModelId::from("claude-sonnet-5"), Vec::new());
        let mut done = None;
        for event in decoder.feed(fixture.as_bytes()).unwrap() {
            for event in assembler.accept(event).unwrap() {
                if let StreamEvent::Done(message) = event {
                    done = Some(message);
                }
            }
        }
        done.expect("recorded fixture must produce Done")
    }

    #[test]
    fn request_contains_full_transcript_and_drops_foreign_opaque_state() {
        let request = Request {
            system: "Use tools carefully.".to_owned(),
            messages: vec![
                Message::user("hello"),
                Message::Assistant(AssistantMessage {
                    blocks: vec![ContentBlock::Thinking {
                        text: "summary".to_owned(),
                        opaque: Some(OpaqueBlob {
                            provider: ProviderId::from("openai"),
                            kind: "encrypted_content".to_owned(),
                            data: "secret-provider-state".to_owned(),
                        }),
                    }],
                    stop: StopReason::ToolUse,
                    usage: Usage::default(),
                    provider: ProviderId::from("openai"),
                    model: ModelId::from("gpt-5.6-sol"),
                }),
                Message::ToolResult(ToolResult {
                    call_id: ToolCallId::from("call-1"),
                    content: "ok".to_owned(),
                    is_error: false,
                }),
            ],
            tools: Vec::new(),
            max_output_tokens: 1024,
            thinking: ThinkingLevel::Medium,
        };

        let body = request_body(&request, &ModelId::from("claude-sonnet-5")).unwrap();
        let encoded = body.to_string();
        assert!(encoded.contains("summary"));
        assert!(encoded.contains("call-1"));
        assert!(!encoded.contains("secret-provider-state"));
        assert_eq!(body["output_config"]["effort"], "medium");
    }

    #[test]
    fn always_rebase_is_equivalent_for_the_same_authoritative_transcript() {
        let request = Request {
            system: "test".to_owned(),
            messages: vec![Message::user("first"), Message::user("second")],
            tools: Vec::new(),
            max_output_tokens: 100,
            thinking: ThinkingLevel::Medium,
        };
        let model = ModelId::from("claude-sonnet-5");

        // Anthropic has no native continuation state: every projection is the
        // rebase path, so identical authoritative inputs are structurally
        // identical before the recorded response assembler runs.
        assert_eq!(
            request_body(&request, &model).unwrap(),
            request_body(&request, &model).unwrap()
        );
        assert_eq!(recorded_done(), recorded_done());
    }

    #[test]
    fn always_rebase_prevents_failed_or_branched_state_from_leaking() {
        let model = ModelId::from("claude-sonnet-5");
        let failed = Request {
            system: "test".to_owned(),
            messages: vec![Message::user("failed transcript")],
            tools: Vec::new(),
            max_output_tokens: 0,
            thinking: ThinkingLevel::Medium,
        };
        assert!(request_body(&failed, &model).is_err());

        let branch = Request {
            system: "test".to_owned(),
            messages: vec![Message::user("branched transcript")],
            tools: Vec::new(),
            max_output_tokens: 100,
            thinking: ThinkingLevel::Medium,
        };
        let encoded = request_body(&branch, &model).unwrap().to_string();
        assert!(encoded.contains("branched transcript"));
        assert!(!encoded.contains("failed transcript"));
    }

    #[test]
    fn provider_debug_redacts_api_key() {
        let provider = AnthropicProvider {
            api_key: "secret-key".to_owned(),
            base_url: "https://api.anthropic.com".to_owned(),
            client: http_client().unwrap(),
            model: ModelId::from("claude-sonnet-5"),
        };
        let debug = format!("{provider:?}");
        assert!(!debug.contains("secret-key"));
        assert!(debug.contains("REDACTED"));
    }

    #[test]
    fn malformed_rejected_call_is_replayed_with_a_valid_tool_pair() {
        let request = Request {
            system: "test".to_owned(),
            messages: vec![
                Message::Assistant(AssistantMessage {
                    blocks: vec![ContentBlock::RejectedToolCall {
                        id: ToolCallId::from("toolu-1"),
                        name: "bash".to_owned(),
                        args: None,
                        error: rho_ai::ToolArgumentError {
                            kind: "json_parse".to_owned(),
                            message: "bad JSON".to_owned(),
                        },
                    }],
                    stop: StopReason::ToolUse,
                    usage: Usage::default(),
                    provider: ProviderId::from(PROVIDER),
                    model: ModelId::from("claude-sonnet-5"),
                }),
                Message::ToolResult(ToolResult {
                    call_id: ToolCallId::from("toolu-1"),
                    content: "tool arguments rejected".to_owned(),
                    is_error: true,
                }),
            ],
            tools: vec![ToolDefinition::new(
                "bash",
                "Run a command.",
                json!({"type": "object"}),
            )],
            max_output_tokens: 100,
            thinking: ThinkingLevel::None,
        };

        let body = request_body(&request, &ModelId::from("claude-sonnet-5")).unwrap();
        assert_eq!(body["messages"][0]["content"][0]["input"], json!({}));
        assert_eq!(body["messages"][1]["content"][0]["tool_use_id"], "toolu-1");
    }
}
