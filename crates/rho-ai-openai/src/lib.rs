//! OpenAI provider adapter and quarantine boundary for Nanocodex.
//!
//! Each opened rho provider owns one lower-level Nanocodex session. The
//! adapter continues it only when rho's complete transcript extends the
//! acknowledged prefix; otherwise it rebuilds from the authoritative input.
//! Nanocodex's agent loop never crosses this crate boundary.

use std::{collections::BTreeSet, num::NonZeroU32, sync::Arc};

use async_stream::stream;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use futures_util::StreamExt as _;
use nanocodex_oai_api::{
    Model as OpenAiModel, OpenAi, ResponseError, ResponseErrorKind, ResponseEvent as OpenAiEvent,
    Thinking as OpenAiThinking,
    responses::{
        ContentItem, FunctionOutputBody, JsonSchema, MessageRole,
        ReasoningContent as OpenAiReasoningContent, ReasoningSummary, ResponseItem, ResponseItemId,
        ToolDefinition as OpenAiToolDefinition, Usage as OpenAiUsage,
    },
    session::ResponseInput,
    tower::DefaultResponsesService,
    transport::ResponsesHistory,
};
use rho_ai::{
    AssistantMessage, CancellationToken, ContentBlock, CredentialSource, DeltaKind, ErrorKind,
    Message, ModelId, ModelInfo, OpaqueBlob, OpenProvider, Provider, ProviderError,
    ProviderFactory, ProviderId, ProviderStream, Request, SessionConfig, StopReason, StreamEvent,
    ThinkingLevel, ToolArgumentError, ToolCallId, ToolDefinition, Usage, validate_tool_arguments,
    validate_tool_definition,
};
use serde::{Deserialize, Serialize};

const PROVIDER: &str = "openai";
const MODEL: &str = OpenAiModel::Luna.as_str();
const SOL_MODEL: &str = OpenAiModel::Sol.as_str();
const ENCRYPTED_REASONING_KIND: &str = "encrypted_reasoning_v1";

#[derive(Debug, Deserialize, Serialize)]
struct EncryptedReasoningState {
    item_id: ResponseItemId,
    encrypted_content: Box<str>,
}

type NativeSession = nanocodex_oai_api::Session<DefaultResponsesService>;

/// Shared OpenAI configuration, credentials, and model catalog.
#[derive(Clone)]
pub struct OpenAiFactory {
    credentials: Arc<dyn CredentialSource>,
    models: Vec<ModelInfo>,
}

impl std::fmt::Debug for OpenAiFactory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenAiFactory")
            .field("credentials", &"[REDACTED]")
            .field("models", &self.models)
            .finish()
    }
}

impl OpenAiFactory {
    /// Creates a factory using credentials resolved when sessions are opened.
    #[must_use]
    pub fn new(credentials: Arc<dyn CredentialSource>) -> Self {
        Self {
            credentials,
            models: vec![
                ModelInfo {
                    id: ModelId::from(MODEL),
                    display_name: "GPT-5.6 Luna".to_owned(),
                    context_tokens: Some(nanocodex_oai_api::CONTEXT_WINDOW_TOKENS),
                    max_output_tokens: None,
                },
                ModelInfo {
                    id: ModelId::from(SOL_MODEL),
                    display_name: "GPT-5.6 Sol".to_owned(),
                    context_tokens: Some(nanocodex_oai_api::CONTEXT_WINDOW_TOKENS),
                    max_output_tokens: None,
                },
            ],
        }
    }
}

impl ProviderFactory for OpenAiFactory {
    fn provider_id(&self) -> ProviderId {
        ProviderId::from(PROVIDER)
    }

    fn models(&self) -> &[ModelInfo] {
        &self.models
    }

    fn open(&self, config: SessionConfig) -> OpenProvider<'_> {
        let result = (|| {
            openai_model(&config.model)?;
            let credential = self
                .credentials
                .resolve(&ProviderId::from(PROVIDER))
                .map_err(authentication_error)?;
            Ok(Box::new(OpenAiProvider {
                api_key: credential.expose_api_key().to_owned(),
                model: config.model,
                session: None,
                continuation: ContinuationState::default(),
            }) as Box<dyn Provider>)
        })();
        Box::pin(async move { result })
    }
}

#[derive(Clone, Debug, PartialEq)]
struct SessionShape {
    system: String,
    tools: Vec<ToolDefinition>,
    thinking: ThinkingLevel,
}

impl From<&Request> for SessionShape {
    fn from(request: &Request) -> Self {
        Self {
            system: request.system.clone(),
            tools: request.tools.clone(),
            thinking: request.thinking,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GenerationPlan {
    Continue { start: usize },
    Rebase,
}

impl GenerationPlan {
    fn input(self, messages: &[Message]) -> &[Message] {
        match self {
            Self::Continue { start } => &messages[start..],
            Self::Rebase => messages,
        }
    }
}

#[derive(Default)]
struct ContinuationState {
    shape: Option<SessionShape>,
    acknowledged: Vec<Message>,
    poisoned: bool,
}

impl ContinuationState {
    fn begin(
        &mut self,
        has_session: bool,
        requested_shape: &SessionShape,
        messages: &[Message],
    ) -> GenerationPlan {
        let plan = if !self.poisoned
            && has_session
            && self.shape.as_ref() == Some(requested_shape)
            && messages.starts_with(&self.acknowledged)
        {
            GenerationPlan::Continue {
                start: self.acknowledged.len(),
            }
        } else {
            GenerationPlan::Rebase
        };
        // A generation is ambiguous until its authoritative Done has been
        // assembled and acknowledged. This also covers a never-polled or
        // dropped stream.
        self.poisoned = true;
        plan
    }

    fn installed_rebase(&mut self, shape: SessionShape) {
        self.shape = Some(shape);
        self.acknowledged.clear();
    }

    fn acknowledge(&mut self, messages: &[Message], message: &AssistantMessage) {
        self.acknowledged = messages.to_vec();
        self.acknowledged.push(Message::Assistant(message.clone()));
        self.poisoned = false;
    }
}

/// One live logical OpenAI model session.
struct OpenAiProvider {
    api_key: String,
    model: ModelId,
    session: Option<NativeSession>,
    continuation: ContinuationState,
}

impl std::fmt::Debug for OpenAiProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenAiProvider")
            .field("api_key", &"[REDACTED]")
            .field("model", &self.model)
            .field("has_native_session", &self.session.is_some())
            .field("shape", &self.continuation.shape)
            .field(
                "acknowledged_messages",
                &self.continuation.acknowledged.len(),
            )
            .field("poisoned", &self.continuation.poisoned)
            .finish()
    }
}

impl Provider for OpenAiProvider {
    fn generate(
        &mut self,
        request: Request,
        cancellation: CancellationToken,
    ) -> ProviderStream<'_> {
        let shape = SessionShape::from(&request);
        let plan = self
            .continuation
            .begin(self.session.is_some(), &shape, &request.messages);
        Box::pin(stream! {
            if cancellation.is_cancelled() {
                yield StreamEvent::Error(ProviderError::cancelled());
                return;
            }
            if let Err(error) = validate_request(&request) {
                yield StreamEvent::Error(error);
                return;
            }
            let input_messages = plan.input(&request.messages);
            let items = match request_items(input_messages) {
                Ok(items) => items,
                Err(error) => {
                    yield StreamEvent::Error(error);
                    return;
                }
            };
            if plan == GenerationPlan::Rebase {
                self.session = match build_session(&self.api_key, &self.model, &request) {
                    Ok(session) => Some(session),
                    Err(error) => {
                        yield StreamEvent::Error(error);
                        return;
                    }
                };
                self.continuation.installed_rebase(shape);
            }

            let Some(session) = self.session.as_mut() else {
                yield StreamEvent::Error(ProviderError::invalid_response(
                    "OpenAI native session was not initialized",
                ));
                return;
            };
            let mut turn = session.turn();
            let mut response = turn.create(ResponseInput::items(items));
            let mut block_index = 0usize;
            loop {
                let next = tokio::select! {
                    biased;
                    () = cancellation.cancelled() => {
                        yield StreamEvent::Error(ProviderError::cancelled());
                        return;
                    }
                    next = response.next() => next,
                };
                let Some(next) = next else { break };
                let event = match next {
                    Ok(event) => event,
                    Err(error) => {
                        match incomplete_message(&error, &request.tools, &self.model) {
                            Ok(Some(message)) => yield StreamEvent::Done(message),
                            Ok(None) => yield StreamEvent::Error(map_error(&error)),
                            Err(error) => yield StreamEvent::Error(error),
                        }
                        return;
                    }
                };
                if cancellation.is_cancelled() {
                    yield StreamEvent::Error(ProviderError::cancelled());
                    return;
                }
                match event {
                    OpenAiEvent::Created => yield StreamEvent::Start,
                    OpenAiEvent::OutputTextDelta(delta) => yield StreamEvent::Delta {
                        index: block_index,
                        kind: DeltaKind::Text,
                        delta,
                    },
                    OpenAiEvent::ToolCallInputDelta { delta, .. } => {
                        yield StreamEvent::Delta {
                            index: block_index,
                            kind: DeltaKind::ToolArguments,
                            delta,
                        };
                    }
                    OpenAiEvent::ReasoningSummaryDelta { delta, .. }
                    | OpenAiEvent::ReasoningContentDelta { delta, .. } => {
                        yield StreamEvent::Delta {
                            index: block_index,
                            kind: DeltaKind::Thinking,
                            delta,
                        };
                    }
                    OpenAiEvent::OutputItemDone(item) => {
                        let blocks = match response_item_blocks(&item, &request.tools) {
                            Ok(blocks) => blocks,
                            Err(error) => {
                                yield StreamEvent::Error(error);
                                return;
                            }
                        };
                        for block in blocks {
                            if cancellation.is_cancelled() {
                                yield StreamEvent::Error(ProviderError::cancelled());
                                return;
                            }
                            yield StreamEvent::BlockDone {
                                index: block_index,
                                block,
                            };
                            block_index += 1;
                        }
                    }
                    OpenAiEvent::OutputItemAdded(_)
                    | OpenAiEvent::ReasoningSummaryDone { .. }
                    | OpenAiEvent::ReasoningSummaryPartAdded { .. }
                    | OpenAiEvent::Completed { .. } => {}
                    _ => {
                        yield StreamEvent::Error(ProviderError::invalid_response(
                            "nanocodex returned an unsupported response event",
                        ));
                        return;
                    }
                }
            }
            let completed = tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    yield StreamEvent::Error(ProviderError::cancelled());
                    return;
                }
                completed = response => match completed {
                    Ok(completed) => completed,
                    Err(error) => {
                        yield StreamEvent::Error(map_error(&error));
                        return;
                    }
                }
            };
            match completed_message(&completed, &request.tools, &self.model) {
                Ok(message) => {
                    self.continuation
                        .acknowledge(&request.messages, &message);
                    yield StreamEvent::Done(message);
                }
                Err(error) => yield StreamEvent::Error(error),
            }
        })
    }
}

fn validate_request(request: &Request) -> Result<(), ProviderError> {
    if request.system.trim().is_empty() {
        return Err(invalid_request("system instructions must not be empty"));
    }
    if request.max_output_tokens == 0 {
        return Err(invalid_request(
            "max_output_tokens must be greater than zero",
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
    Ok(())
}

fn build_session(
    api_key: &str,
    model: &ModelId,
    request: &Request,
) -> Result<NativeSession, ProviderError> {
    let tools = request.tools.iter().map(openai_tool).collect::<Vec<_>>();
    let openai = OpenAi::builder(api_key.to_owned())
        .model(openai_model(model)?)
        .max_attempts(NonZeroU32::MIN)
        .store(false)
        .history(ResponsesHistory::FullReplay)
        .thinking(openai_thinking(request.thinking))
        .build()
        .map_err(authentication_error)?;
    openai
        .instructions(request.system.clone())
        .tool_definitions(tools)
        .build()
        .map_err(|error| invalid_request(error.to_string()))
}

fn openai_model(model: &ModelId) -> Result<OpenAiModel, ProviderError> {
    match model.as_str() {
        MODEL => Ok(OpenAiModel::Luna),
        SOL_MODEL => Ok(OpenAiModel::Sol),
        model => Err(invalid_request(format!(
            "OpenAI model {model:?} is unsupported; expected {MODEL} or {SOL_MODEL}"
        ))),
    }
}

fn authentication_error(error: impl std::fmt::Display) -> ProviderError {
    ProviderError {
        retryable: false,
        kind: ErrorKind::Authentication,
        message: error.to_string(),
    }
}

fn request_items(messages: &[Message]) -> Result<Vec<ResponseItem>, ProviderError> {
    let mut items = Vec::new();
    for message in messages {
        match message {
            Message::User { content } => {
                let content = content
                    .iter()
                    .map(openai_user_content)
                    .collect::<Result<Vec<_>, _>>()?;
                items.push(ResponseItem::message(MessageRole::User, content));
            }
            Message::Assistant(message) => {
                for block in &message.blocks {
                    match block {
                        ContentBlock::Text { text } => items.push(ResponseItem::message(
                            MessageRole::Assistant,
                            [ContentItem::output_text(text.clone())],
                        )),
                        ContentBlock::Thinking { text, opaque } => {
                            let state = opaque.as_ref().filter(|opaque| {
                                opaque.provider == ProviderId::from(PROVIDER)
                                    && opaque.kind == ENCRYPTED_REASONING_KIND
                            });
                            if let Some(opaque) = state {
                                let state: EncryptedReasoningState =
                                    serde_json::from_str(&opaque.data).map_err(|error| {
                                        invalid_request(format!(
                                            "invalid OpenAI encrypted reasoning state: {error}"
                                        ))
                                    })?;
                                if !state.item_id.is_prefixed() {
                                    return Err(invalid_request(
                                        "OpenAI encrypted reasoning state has an invalid item id",
                                    ));
                                }
                                items.push(ResponseItem::Reasoning {
                                    id: Some(state.item_id),
                                    summary: if text.is_empty() {
                                        Vec::new()
                                    } else {
                                        vec![ReasoningSummary::SummaryText {
                                            text: text.clone().into_boxed_str(),
                                        }]
                                    },
                                    content: None,
                                    encrypted_content: Some(state.encrypted_content),
                                    status: None,
                                    internal_chat_message_metadata_passthrough: None,
                                });
                            } else if !text.is_empty() {
                                items.push(ResponseItem::message(
                                    MessageRole::Assistant,
                                    [ContentItem::output_text(text.clone())],
                                ));
                            }
                        }
                        ContentBlock::ToolCall { id, name, args } => {
                            items.push(function_call_item(id, name, args))
                        }
                        ContentBlock::RejectedToolCall {
                            id,
                            name,
                            args: Some(args),
                            ..
                        } => items.push(function_call_item(id, name, args)),
                        ContentBlock::RejectedToolCall {
                            id,
                            name,
                            args: None,
                            ..
                        } => items.push(function_call_item(
                            id,
                            name,
                            &serde_json::Value::Object(serde_json::Map::new()),
                        )),
                        ContentBlock::Image { .. } => {
                            return Err(invalid_request(
                                "OpenAI assistant history cannot contain image blocks",
                            ));
                        }
                        _ => {
                            return Err(invalid_request(
                                "unsupported assistant content block variant",
                            ));
                        }
                    }
                }
            }
            Message::ToolResult(result) => {
                items.push(ResponseItem::function_call_output(
                    result.call_id.as_str().to_owned(),
                    FunctionOutputBody::Text(result.content.clone().into_boxed_str()),
                ));
            }
            _ => return Err(invalid_request("unsupported transcript message variant")),
        }
    }
    Ok(items)
}

fn function_call_item(id: &ToolCallId, name: &str, args: &serde_json::Value) -> ResponseItem {
    ResponseItem::FunctionCall {
        id: None,
        name: name.to_owned().into_boxed_str(),
        namespace: None,
        arguments: args.to_string().into_boxed_str(),
        call_id: id.as_str().to_owned().into_boxed_str(),
        caller: None,
        status: None,
        created_by: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn openai_user_content(block: &ContentBlock) -> Result<ContentItem, ProviderError> {
    match block {
        ContentBlock::Text { text } => Ok(ContentItem::input_text(text.clone())),
        ContentBlock::Image { data, mime } => Ok(ContentItem::input_image(format!(
            "data:{mime};base64,{}",
            BASE64.encode(data)
        ))),
        _ => Err(invalid_request(
            "user messages may contain only text and image blocks",
        )),
    }
}

fn openai_tool(tool: &ToolDefinition) -> OpenAiToolDefinition {
    OpenAiToolDefinition::function(
        tool.name.clone(),
        tool.description.clone(),
        JsonSchema::from(tool.parameters.clone()),
    )
}

fn completed_message(
    completed: &nanocodex_oai_api::CompletedResponse,
    tools: &[ToolDefinition],
    model: &ModelId,
) -> Result<AssistantMessage, ProviderError> {
    let mut blocks = Vec::new();
    for item in completed.output() {
        blocks.extend(response_item_blocks(item, tools)?);
    }
    let tool_use = blocks.iter().any(|block| {
        matches!(
            block,
            ContentBlock::ToolCall { .. } | ContentBlock::RejectedToolCall { .. }
        )
    });
    let stop = if tool_use {
        StopReason::ToolUse
    } else if completed.end_turn() == Some(false) {
        StopReason::Paused
    } else {
        StopReason::Stop
    };
    let usage = completed_usage(completed.usage())?;
    Ok(AssistantMessage {
        blocks,
        stop,
        usage,
        provider: ProviderId::from(PROVIDER),
        model: model.clone(),
    })
}

fn completed_usage(usage: Option<&OpenAiUsage>) -> Result<Usage, ProviderError> {
    let usage = usage.ok_or_else(|| {
        ProviderError::invalid_response("OpenAI completed response omitted token usage")
    })?;
    Ok(Usage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_tokens: usage
            .input_tokens_details
            .as_ref()
            .map_or(0, |details| details.cached_tokens),
        cache_write_tokens: usage
            .input_tokens_details
            .as_ref()
            .map_or(0, |details| details.cache_write_tokens),
    })
}

#[derive(Deserialize)]
struct IncompleteEnvelope {
    #[serde(rename = "type")]
    event_type: String,
    response: IncompleteResponse,
}

#[derive(Deserialize)]
struct IncompleteResponse {
    #[serde(default)]
    model: Option<String>,
    output: Vec<ResponseItem>,
    incomplete_details: IncompleteDetails,
    #[serde(default)]
    usage: Option<IncompleteUsage>,
}

#[derive(Deserialize)]
struct IncompleteDetails {
    reason: String,
}

#[derive(Default, Deserialize)]
struct IncompleteUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    input_tokens_details: Option<IncompleteInputDetails>,
}

#[derive(Default, Deserialize)]
struct IncompleteInputDetails {
    #[serde(default)]
    cached_tokens: u64,
    #[serde(default)]
    cache_write_tokens: u64,
}

fn incomplete_message(
    error: &ResponseError,
    tools: &[ToolDefinition],
    fallback_model: &ModelId,
) -> Result<Option<AssistantMessage>, ProviderError> {
    let Some(nanocodex_oai_api::transport::ResponsesError::Api { event }) = error.responses_error()
    else {
        return Ok(None);
    };
    let value: serde_json::Value = serde_json::from_str(event).map_err(|source| {
        ProviderError::invalid_response(format!("OpenAI returned an invalid error event: {source}"))
    })?;
    if value.get("type").and_then(serde_json::Value::as_str) != Some("response.incomplete") {
        return Ok(None);
    }
    let incomplete: IncompleteEnvelope = serde_json::from_value(value).map_err(|source| {
        ProviderError::invalid_response(format!(
            "OpenAI returned an invalid incomplete response: {source}"
        ))
    })?;
    if incomplete.event_type != "response.incomplete" {
        return Err(ProviderError::invalid_response(
            "OpenAI incomplete response carried a mismatched event type",
        ));
    }
    let stop = match incomplete.response.incomplete_details.reason.as_str() {
        "max_output_tokens" | "model_context_window_exceeded" => StopReason::Length,
        "content_filter" => StopReason::Refusal,
        reason => {
            return Err(ProviderError::invalid_response(format!(
                "unsupported OpenAI incomplete reason {reason:?}"
            )));
        }
    };
    let mut blocks = Vec::new();
    for item in &incomplete.response.output {
        blocks.extend(response_item_blocks(item, tools)?);
    }
    let usage = incomplete
        .response
        .usage
        .map_or_else(Usage::default, |usage| Usage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: usage
                .input_tokens_details
                .as_ref()
                .map_or(0, |details| details.cached_tokens),
            cache_write_tokens: usage
                .input_tokens_details
                .as_ref()
                .map_or(0, |details| details.cache_write_tokens),
        });
    let model = incomplete
        .response
        .model
        .filter(|model| !model.trim().is_empty())
        .unwrap_or_else(|| fallback_model.as_str().to_owned());
    Ok(Some(AssistantMessage {
        blocks,
        stop,
        usage,
        provider: ProviderId::from(PROVIDER),
        model: ModelId::from(model),
    }))
}

fn response_item_blocks(
    item: &ResponseItem,
    tools: &[ToolDefinition],
) -> Result<Vec<ContentBlock>, ProviderError> {
    match item {
        ResponseItem::Message {
            role: MessageRole::Assistant,
            content,
            ..
        } => content
            .iter()
            .map(|content| match content {
                ContentItem::OutputText { text, .. } => Ok(ContentBlock::Text {
                    text: text.to_string(),
                }),
                ContentItem::InputText { .. }
                | ContentItem::InputImage { .. }
                | ContentItem::InputAudio { .. } => Err(ProviderError::invalid_response(
                    "OpenAI assistant output contained an input-only content item",
                )),
            })
            .collect(),
        ResponseItem::Reasoning {
            id,
            summary,
            content,
            encrypted_content,
            ..
        } => {
            let mut text = summary
                .iter()
                .map(|summary| match summary {
                    ReasoningSummary::SummaryText { text } => text.as_ref(),
                })
                .collect::<Vec<_>>()
                .join("\n");
            if let Some(content) = content {
                for content in content {
                    let value = match content {
                        OpenAiReasoningContent::ReasoningText { text }
                        | OpenAiReasoningContent::Text { text } => text,
                    };
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(value);
                }
            }
            let opaque = encrypted_content
                .as_ref()
                .map(|encrypted_content| {
                    let item_id = id
                        .as_ref()
                        .filter(|item_id| item_id.is_prefixed())
                        .ok_or_else(|| {
                            ProviderError::invalid_response(
                                "OpenAI encrypted reasoning output lacked a replayable item id",
                            )
                        })?;
                    let state = EncryptedReasoningState {
                        item_id: item_id.clone(),
                        encrypted_content: encrypted_content.clone(),
                    };
                    let data = serde_json::to_string(&state).map_err(|error| {
                        ProviderError::invalid_response(format!(
                            "could not preserve OpenAI encrypted reasoning state: {error}"
                        ))
                    })?;
                    Ok(OpaqueBlob {
                        provider: ProviderId::from(PROVIDER),
                        kind: ENCRYPTED_REASONING_KIND.to_owned(),
                        data,
                    })
                })
                .transpose()?;
            Ok(vec![ContentBlock::Thinking { text, opaque }])
        }
        ResponseItem::FunctionCall {
            name,
            arguments,
            call_id,
            ..
        } => {
            let id = ToolCallId::from(call_id.as_ref());
            let arguments: serde_json::Value = match serde_json::from_str(arguments) {
                Ok(arguments) => arguments,
                Err(error) => {
                    return Ok(vec![ContentBlock::RejectedToolCall {
                        id,
                        name: name.to_string(),
                        args: None,
                        error: ToolArgumentError {
                            kind: "json_parse".to_owned(),
                            message: format!("provider emitted malformed JSON: {error}"),
                        },
                    }]);
                }
            };
            let Some(tool) = tools.iter().find(|tool| tool.name == name.as_ref()) else {
                return Ok(vec![ContentBlock::RejectedToolCall {
                    id,
                    name: name.to_string(),
                    args: Some(arguments),
                    error: ToolArgumentError {
                        kind: "unknown_tool".to_owned(),
                        message: "provider requested a tool that was not declared".to_owned(),
                    },
                }]);
            };
            match validate_tool_arguments(tool, &arguments) {
                Ok(()) => Ok(vec![ContentBlock::ToolCall {
                    id,
                    name: name.to_string(),
                    args: arguments,
                }]),
                Err(error) => Ok(vec![ContentBlock::RejectedToolCall {
                    id,
                    name: name.to_string(),
                    args: Some(arguments),
                    error,
                }]),
            }
        }
        ResponseItem::Other(_) => Err(ProviderError::invalid_response(
            "OpenAI returned an unknown output item type",
        )),
        ResponseItem::AdditionalTools { .. }
        | ResponseItem::Message { .. }
        | ResponseItem::AgentMessage { .. }
        | ResponseItem::LocalShellCall { .. }
        | ResponseItem::FunctionCallOutput { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::CustomToolCallOutput { .. }
        | ResponseItem::ToolSearchOutput { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ImageGenerationCall { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::CompactionTrigger {}
        | ResponseItem::ContextCompaction { .. } => Err(ProviderError::invalid_response(
            "OpenAI returned an unsupported output item type",
        )),
    }
}

fn map_error(error: &ResponseError) -> ProviderError {
    let (kind, retryable) = match error.kind() {
        ResponseErrorKind::ContextWindowExceeded => (ErrorKind::ContextWindowExceeded, false),
        ResponseErrorKind::Protocol => (ErrorKind::InvalidResponse, false),
        ResponseErrorKind::Service => error
            .responses_error()
            .map_or((ErrorKind::Transport, false), classify_responses_error),
        _ => (ErrorKind::Other, false),
    };
    ProviderError {
        retryable,
        kind,
        message: error.to_string(),
    }
}

fn classify_responses_error(
    error: &nanocodex_oai_api::transport::ResponsesError,
) -> (ErrorKind, bool) {
    use nanocodex_oai_api::transport::ResponsesError;

    let retry_class = error.retry_advice().map(|advice| advice.class);
    match error {
        ResponsesError::Authorization { .. } | ResponsesError::InvalidAuthorization { .. } => {
            (ErrorKind::Authentication, false)
        }
        ResponsesError::ContextWindowExceeded { .. } => (ErrorKind::ContextWindowExceeded, false),
        ResponsesError::InvalidJson(_)
        | ResponsesError::InvalidPayload { .. }
        | ResponsesError::InvalidSseUtf8 { .. }
        | ResponsesError::UnexpectedBinary => (ErrorKind::InvalidResponse, false),
        ResponsesError::InvalidUrl { .. }
        | ResponsesError::InvalidSessionId { .. }
        | ResponsesError::EncodeRequest(_)
        | ResponsesError::InvalidImageRequest { .. } => (ErrorKind::InvalidRequest, false),
        ResponsesError::Api { event } => classify_api_event(event, retry_class.is_some()),
        ResponsesError::HandshakeRejected { status, .. }
        | ResponsesError::HttpRejected { status, .. }
            if matches!(status, 401 | 403) =>
        {
            (ErrorKind::Authentication, false)
        }
        _ if matches!(
            retry_class,
            Some("https_rate_limit" | "handshake_rate_limit")
        ) =>
        {
            (ErrorKind::RateLimited, true)
        }
        _ => (ErrorKind::Transport, retry_class.is_some()),
    }
}

fn classify_api_event(event: &str, upstream_retryable: bool) -> (ErrorKind, bool) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(event) else {
        return (ErrorKind::InvalidResponse, false);
    };
    if value.get("type").and_then(serde_json::Value::as_str) == Some("response.incomplete") {
        // Incompletes are terminal response states, not transport failures. The
        // stream path projects supported reasons to an authoritative `Done`.
        return (ErrorKind::InvalidResponse, false);
    }
    let detail = value
        .get("error")
        .or_else(|| value.pointer("/response/error"));
    let discriminator = detail
        .and_then(|detail| detail.get("code").or_else(|| detail.get("type")))
        .and_then(serde_json::Value::as_str);
    match discriminator {
        Some("authentication_error" | "invalid_api_key" | "insufficient_quota") => {
            (ErrorKind::Authentication, false)
        }
        Some("rate_limit_exceeded") => (ErrorKind::RateLimited, true),
        Some("server_is_overloaded" | "slow_down") => (ErrorKind::Overloaded, true),
        Some("context_length_exceeded") => (ErrorKind::ContextWindowExceeded, false),
        Some("invalid_prompt" | "invalid_request_error" | "invalid_image") => {
            (ErrorKind::InvalidRequest, false)
        }
        Some("server_error" | "websocket_connection_limit_reached") => (ErrorKind::Transport, true),
        _ => (ErrorKind::Other, upstream_retryable),
    }
}

fn openai_thinking(level: ThinkingLevel) -> OpenAiThinking {
    match level {
        ThinkingLevel::None => OpenAiThinking::None,
        ThinkingLevel::Low => OpenAiThinking::Low,
        ThinkingLevel::Medium => OpenAiThinking::Medium,
        ThinkingLevel::High => OpenAiThinking::High,
        ThinkingLevel::Xhigh => OpenAiThinking::Xhigh,
        ThinkingLevel::Max => OpenAiThinking::Max,
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
    use serde_json::json;

    use super::*;

    fn bash_tool() -> ToolDefinition {
        ToolDefinition::new(
            "bash",
            "Run one shell command.",
            json!({
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"],
                "additionalProperties": false
            }),
        )
    }

    fn session_request(messages: Vec<Message>) -> Request {
        Request {
            system: "test".to_owned(),
            messages,
            tools: vec![bash_tool()],
            max_output_tokens: 100,
            thinking: ThinkingLevel::High,
        }
    }

    fn recorded_done(text: &str) -> AssistantMessage {
        let item: ResponseItem = serde_json::from_value(json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text}]
        }))
        .unwrap();
        AssistantMessage {
            blocks: response_item_blocks(&item, &[bash_tool()]).unwrap(),
            stop: StopReason::Stop,
            usage: Usage {
                input_tokens: 8,
                output_tokens: 3,
                ..Usage::default()
            },
            provider: ProviderId::from(PROVIDER),
            model: ModelId::from(MODEL),
        }
    }

    fn recorded_generation(
        native_history: &mut Vec<Message>,
        input: &[Message],
        authoritative: &[Message],
    ) -> AssistantMessage {
        native_history.extend_from_slice(input);
        assert_eq!(native_history, authoritative);
        recorded_done(&format!("fixture saw {} messages", native_history.len()))
    }

    fn acknowledged_state(request: &Request, message: &AssistantMessage) -> ContinuationState {
        let shape = SessionShape::from(request);
        let mut state = ContinuationState::default();
        assert_eq!(
            state.begin(false, &shape, &request.messages),
            GenerationPlan::Rebase
        );
        state.installed_rebase(shape);
        state.acknowledge(&request.messages, message);
        state
    }

    #[test]
    fn recorded_function_call_is_parsed_and_schema_validated() {
        let item: ResponseItem = serde_json::from_value(json!({
            "type": "function_call",
            "name": "bash",
            "arguments": "{\"command\":\"pwd\"}",
            "call_id": "call-1"
        }))
        .unwrap();
        let blocks = response_item_blocks(&item, &[bash_tool()]).unwrap();
        assert!(matches!(
            &blocks[0],
            ContentBlock::ToolCall { id, name, args }
                if id.as_str() == "call-1" && name == "bash" && args["command"] == "pwd"
        ));
    }

    #[test]
    fn recorded_nonconforming_call_is_not_coerced() {
        let item: ResponseItem = serde_json::from_value(json!({
            "type": "function_call",
            "name": "bash",
            "arguments": "{\"command\":5}",
            "call_id": "call-1"
        }))
        .unwrap();
        let blocks = response_item_blocks(&item, &[bash_tool()]).unwrap();
        assert!(matches!(
            &blocks[0],
            ContentBlock::RejectedToolCall { error, .. }
                if error.kind == "schema_validation"
        ));
    }

    #[test]
    fn recorded_malformed_call_is_a_structured_rejection() {
        let item: ResponseItem = serde_json::from_value(json!({
            "type": "function_call",
            "name": "bash",
            "arguments": "{\"command\":",
            "call_id": "call-1"
        }))
        .unwrap();
        let blocks = response_item_blocks(&item, &[bash_tool()]).unwrap();
        assert!(matches!(
            &blocks[0],
            ContentBlock::RejectedToolCall { args: None, error, .. }
                if error.kind == "json_parse"
        ));
    }

    #[test]
    fn unsupported_typed_output_is_rejected_instead_of_dropped() {
        let item: ResponseItem = serde_json::from_value(json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "input_text", "text": "not output"}]
        }))
        .unwrap();
        let error = response_item_blocks(&item, &[]).unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidResponse);

        let item: ResponseItem = serde_json::from_value(json!({
            "type": "web_search_call",
            "status": "completed"
        }))
        .unwrap();
        let error = response_item_blocks(&item, &[]).unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidResponse);
    }

    #[test]
    fn fresh_session_input_contains_complete_transcript_and_drops_foreign_opaque() {
        let request = Request {
            system: "test".to_owned(),
            messages: vec![
                Message::user("first"),
                Message::Assistant(AssistantMessage {
                    blocks: vec![ContentBlock::Thinking {
                        text: "summary".to_owned(),
                        opaque: Some(OpaqueBlob {
                            provider: ProviderId::from("anthropic"),
                            kind: "signature".to_owned(),
                            data: "foreign-secret".to_owned(),
                        }),
                    }],
                    stop: StopReason::Stop,
                    usage: Usage::default(),
                    provider: ProviderId::from("anthropic"),
                    model: ModelId::from("claude-sonnet-5"),
                }),
                Message::user("second"),
            ],
            tools: Vec::new(),
            max_output_tokens: 1024,
            thinking: ThinkingLevel::High,
        };
        let encoded = serde_json::to_string(&request_items(&request.messages).unwrap()).unwrap();
        assert!(encoded.contains("first"));
        assert!(encoded.contains("second"));
        assert!(encoded.contains("summary"));
        assert!(!encoded.contains("foreign-secret"));
    }

    #[test]
    fn continue_and_rebase_are_authoritatively_equivalent_on_recorded_fixture() {
        let initial = session_request(vec![Message::user("first")]);
        let initial_done = recorded_done("first answer");
        let mut continued_state = acknowledged_state(&initial, &initial_done);
        let mut messages = initial.messages.clone();
        messages.push(Message::Assistant(initial_done));
        messages.push(Message::user("second"));
        let request = session_request(messages);
        let shape = SessionShape::from(&request);

        let continue_plan = continued_state.begin(true, &shape, &request.messages);
        let mut fresh_state = ContinuationState::default();
        let rebase_plan = fresh_state.begin(false, &shape, &request.messages);
        assert_eq!(continue_plan, GenerationPlan::Continue { start: 2 });
        assert_eq!(rebase_plan, GenerationPlan::Rebase);

        // The continued fixture starts with the acknowledged native prefix
        // and receives only the suffix; the rebased fixture starts empty and
        // receives the full transcript. Both reconstruct the same logical
        // input and therefore cross the authoritative boundary identically.
        let mut continued_native = request.messages[..2].to_vec();
        let continued_done = recorded_generation(
            &mut continued_native,
            continue_plan.input(&request.messages),
            &request.messages,
        );
        let mut rebased_native = Vec::new();
        let rebased_done = recorded_generation(
            &mut rebased_native,
            rebase_plan.input(&request.messages),
            &request.messages,
        );
        assert_eq!(continued_done, rebased_done);
    }

    #[test]
    fn failed_generation_poisons_the_next_openai_continuation() {
        let initial = session_request(vec![Message::user("first")]);
        let initial_done = recorded_done("first answer");
        let mut state = acknowledged_state(&initial, &initial_done);
        let mut messages = initial.messages.clone();
        messages.push(Message::Assistant(initial_done));
        messages.push(Message::user("second"));
        let request = session_request(messages);
        let shape = SessionShape::from(&request);

        assert!(matches!(
            state.begin(true, &shape, &request.messages),
            GenerationPlan::Continue { .. }
        ));
        // No acknowledge call models an error, cancellation, or dropped
        // stream. The retry must replay the authoritative transcript.
        assert_eq!(
            state.begin(true, &shape, &request.messages),
            GenerationPlan::Rebase
        );
    }

    #[test]
    fn branch_and_incompatible_shape_rebase_openai_session() {
        let initial = session_request(vec![Message::user("first")]);
        let initial_done = recorded_done("first answer");
        let mut branched_state = acknowledged_state(&initial, &initial_done);
        let branch = session_request(vec![Message::user("branched")]);
        assert_eq!(
            branched_state.begin(true, &SessionShape::from(&branch), &branch.messages),
            GenerationPlan::Rebase
        );

        let mut incompatible_state = acknowledged_state(&initial, &initial_done);
        let mut messages = initial.messages.clone();
        messages.push(Message::Assistant(initial_done));
        messages.push(Message::user("second"));
        let mut incompatible = session_request(messages);
        incompatible.thinking = ThinkingLevel::Low;
        assert_eq!(
            incompatible_state.begin(
                true,
                &SessionShape::from(&incompatible),
                &incompatible.messages,
            ),
            GenerationPlan::Rebase
        );
    }

    #[test]
    fn encrypted_reasoning_round_trips_with_its_provider_item_id() {
        let item: ResponseItem = serde_json::from_value(json!({
            "type": "reasoning",
            "id": "rs_original-item",
            "summary": [{"type": "summary_text", "text": "summary"}],
            "encrypted_content": "bound-ciphertext"
        }))
        .unwrap();
        let blocks = response_item_blocks(&item, &[]).unwrap();
        let ContentBlock::Thinking {
            opaque: Some(opaque),
            ..
        } = &blocks[0]
        else {
            panic!("expected encrypted reasoning state");
        };
        assert_eq!(opaque.kind, ENCRYPTED_REASONING_KIND);

        let request = Request {
            system: "test".to_owned(),
            messages: vec![Message::Assistant(AssistantMessage {
                blocks,
                stop: StopReason::Stop,
                usage: Usage::default(),
                provider: ProviderId::from(PROVIDER),
                model: ModelId::from(MODEL),
            })],
            tools: Vec::new(),
            max_output_tokens: 100,
            thinking: ThinkingLevel::High,
        };
        let replay = serde_json::to_value(request_items(&request.messages).unwrap()).unwrap();
        assert_eq!(replay[0]["id"], "rs_original-item");
        assert_eq!(replay[0]["encrypted_content"], "bound-ciphertext");
    }

    #[test]
    fn encrypted_reasoning_without_a_replayable_id_is_rejected() {
        let item: ResponseItem = serde_json::from_value(json!({
            "type": "reasoning",
            "summary": [],
            "encrypted_content": "orphaned-ciphertext"
        }))
        .unwrap();
        let error = response_item_blocks(&item, &[]).unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidResponse);
        assert!(error.message.contains("item id"));
    }

    #[test]
    fn malformed_encrypted_reasoning_state_is_rejected_before_transport() {
        let request = Request {
            system: "test".to_owned(),
            messages: vec![Message::Assistant(AssistantMessage {
                blocks: vec![ContentBlock::Thinking {
                    text: "summary".to_owned(),
                    opaque: Some(OpaqueBlob {
                        provider: ProviderId::from(PROVIDER),
                        kind: ENCRYPTED_REASONING_KIND.to_owned(),
                        data: "not-json".to_owned(),
                    }),
                }],
                stop: StopReason::Stop,
                usage: Usage::default(),
                provider: ProviderId::from(PROVIDER),
                model: ModelId::from(MODEL),
            })],
            tools: Vec::new(),
            max_output_tokens: 100,
            thinking: ThinkingLevel::High,
        };
        let error = request_items(&request.messages).unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidRequest);
    }

    #[test]
    fn provider_debug_redacts_api_key() {
        let provider = OpenAiProvider {
            api_key: "secret-key".to_owned(),
            model: ModelId::from(MODEL),
            session: None,
            continuation: ContinuationState::default(),
        };
        let debug = format!("{provider:?}");
        assert!(!debug.contains("secret-key"));
        assert!(debug.contains("REDACTED"));
    }

    #[test]
    fn model_catalog_lists_luna_first_and_retains_sol() {
        let mut credentials = rho_ai::CredentialStore::default();
        credentials.insert(
            ProviderId::from(PROVIDER),
            rho_ai::StoredCredential::ApiKey {
                api_key: "secret-key".to_owned(),
            },
        );
        let factory = OpenAiFactory::new(Arc::new(credentials));
        assert_eq!(factory.models()[0].id, ModelId::from(MODEL));
        assert_eq!(factory.models()[1].id, ModelId::from(SOL_MODEL));
    }

    #[test]
    fn malformed_rejected_call_is_replayed_with_a_valid_output_pair() {
        let request = Request {
            system: "test".to_owned(),
            messages: vec![
                Message::Assistant(AssistantMessage {
                    blocks: vec![ContentBlock::RejectedToolCall {
                        id: ToolCallId::from("call-1"),
                        name: "bash".to_owned(),
                        args: None,
                        error: ToolArgumentError {
                            kind: "json_parse".to_owned(),
                            message: "bad JSON".to_owned(),
                        },
                    }],
                    stop: StopReason::ToolUse,
                    usage: Usage::default(),
                    provider: ProviderId::from(PROVIDER),
                    model: ModelId::from(MODEL),
                }),
                Message::ToolResult(rho_ai::ToolResult {
                    call_id: ToolCallId::from("call-1"),
                    content: "tool arguments rejected".to_owned(),
                    is_error: true,
                }),
            ],
            tools: vec![bash_tool()],
            max_output_tokens: 100,
            thinking: ThinkingLevel::None,
        };

        let encoded = serde_json::to_value(request_items(&request.messages).unwrap()).unwrap();
        assert_eq!(encoded[0]["arguments"], "{}");
        assert_eq!(encoded[0]["call_id"], "call-1");
        assert_eq!(encoded[1]["call_id"], "call-1");
    }

    #[test]
    fn incomplete_response_is_authoritative_length_not_a_retryable_error() {
        let event = json!({
            "type": "response.incomplete",
            "response": {
                "model": MODEL,
                "output": [{
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "partial"}],
                    "status": "incomplete"
                }],
                "incomplete_details": {"reason": "max_output_tokens"},
                "usage": {"input_tokens": 8, "output_tokens": 3}
            }
        })
        .to_string();
        let error =
            ResponseError::from(nanocodex_oai_api::transport::ResponsesError::Api { event });
        let message = incomplete_message(&error, &[], &ModelId::from(MODEL))
            .unwrap()
            .unwrap();
        assert_eq!(message.stop, StopReason::Length);
        assert_eq!(message.usage.input_tokens, 8);
        assert!(matches!(
            &message.blocks[0],
            ContentBlock::Text { text } if text == "partial"
        ));
        assert!(!map_error(&error).retryable);
    }

    #[test]
    fn completed_response_without_usage_is_rejected() {
        let error = completed_usage(None).unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidResponse);
        assert!(error.message.contains("omitted token usage"));
    }

    #[test]
    fn adapter_owns_openai_error_classification_but_not_retries() {
        let event = json!({
            "type": "response.failed",
            "response": {
                "error": {"code": "server_is_overloaded", "message": "busy"}
            }
        })
        .to_string();
        let error =
            ResponseError::from(nanocodex_oai_api::transport::ResponsesError::Api { event });
        let mapped = map_error(&error);
        assert_eq!(mapped.kind, ErrorKind::Overloaded);
        assert!(mapped.retryable);
    }
}
