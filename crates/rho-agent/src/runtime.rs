use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::LazyLock,
};

use futures_util::StreamExt;
use rho_core::{
    Message, MessageRole,
    message::encode_assistant_message_content,
    protocol::{
        AssistantDelta, FinalMessage, PROTOCOL_VERSION, ServerEvent, ToolCompleted, ToolStarted,
    },
    providers::{ModelKind, Provider, ProviderError, ProviderRequest},
    stream::ProviderEvent,
    tool::{ToolCall, ToolDefinition, ToolResult},
};
use serde_json::{Value, json};
use thiserror::Error;
use tracing::{Instrument, debug, info, info_span, trace, warn};

const DEFAULT_MAX_TOOL_ITERATIONS: usize = 8;

#[derive(Debug, Clone)]
pub struct AgentRuntime {
    protocol_version: u16,
    max_tool_iterations: usize,
}

impl Default for AgentRuntime {
    fn default() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            max_tool_iterations: DEFAULT_MAX_TOOL_ITERATIONS,
        }
    }
}

impl AgentRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_max_tool_iterations(max_tool_iterations: usize) -> Self {
        Self {
            max_tool_iterations,
            ..Self::default()
        }
    }

    pub fn protocol_version(&self) -> u16 {
        self.protocol_version
    }

    pub fn max_tool_iterations(&self) -> usize {
        self.max_tool_iterations
    }

    pub fn start_session(&self, session_id: impl Into<String>) -> AgentSession {
        AgentSession {
            id: session_id.into(),
            messages: Vec::new(),
        }
    }

    pub async fn run_user_message(
        &self,
        session: &mut AgentSession,
        provider: &dyn Provider,
        model: ModelKind,
        user_content: impl Into<String>,
    ) -> Result<Vec<ServerEvent>, AgentError> {
        let user_content = user_content.into();
        let mut emitted_events = Vec::new();
        self.run_user_message_streaming(session, provider, model, user_content, |event| {
            emitted_events.push(event);
        })
        .await?;
        info!(
            session_id = %session.id,
            event_count = emitted_events.len(),
            "completed user message run"
        );
        Ok(emitted_events)
    }

    pub async fn run_user_message_streaming<F>(
        &self,
        session: &mut AgentSession,
        provider: &dyn Provider,
        model: ModelKind,
        user_content: impl Into<String>,
        mut emit_event: F,
    ) -> Result<(), AgentError>
    where
        F: FnMut(ServerEvent),
    {
        let user_content = user_content.into();
        let user_chars = user_content.chars().count();
        let session_id = session.id.clone();
        let run_span = info_span!(
            "runtime.user_message",
            session_id = %session_id,
            model = %model,
            provider = ?provider.kind(),
            user_chars,
            prior_messages = session.messages.len(),
        );

        async {
            info!("starting user message handling");
            session
                .messages
                .push(Message::new(MessageRole::User, user_content));
            let run_result = self
                .run_completion_loop(session, provider, model, &mut emit_event)
                .await;
            match &run_result {
                Ok(()) => info!(
                    total_messages = session.messages.len(),
                    "user message handling completed"
                ),
                Err(error) => warn!(
                    %error,
                    total_messages = session.messages.len(),
                    "user message handling failed"
                ),
            }
            run_result
        }
        .instrument(run_span)
        .await
    }

    async fn run_completion_loop<F>(
        &self,
        session: &mut AgentSession,
        provider: &dyn Provider,
        model: ModelKind,
        emit_event: &mut F,
    ) -> Result<(), AgentError>
    where
        F: FnMut(ServerEvent),
    {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum IterationState {
            Continue,
            Finished,
        }

        let session_id = session.id.clone();
        let builtin_tools = builtin_tool_definitions();
        info!(
            session_id = %session_id,
            model = %model,
            tool_count = builtin_tools.len(),
            max_tool_iterations = self.max_tool_iterations,
            "starting completion loop"
        );

        for iteration in 0..=self.max_tool_iterations {
            let iteration_result = async {
                let request =
                    ProviderRequest::new(model, session.messages()).with_tools(builtin_tools);
                let mut stream = provider.stream(request);

                let mut assistant_message = None;
                let mut assistant_delta_text = String::new();
                let mut assistant_tool_calls = Vec::new();
                let mut tool_messages = Vec::new();

                debug!(
                    request_messages = session.messages.len(),
                    "provider stream started"
                );

                while let Some(next_event) = stream.next().await {
                    let provider_event = match next_event {
                        Ok(event) => event,
                        Err(error) => {
                            warn!(%error, "provider stream yielded error");
                            return Err(AgentError::Provider(error));
                        }
                    };

                    match provider_event {
                        ProviderEvent::AssistantDelta { delta } => {
                            trace!(
                                delta_chars = delta.chars().count(),
                                "assistant delta received"
                            );
                            assistant_delta_text.push_str(&delta);
                            emit_event(ServerEvent::AssistantDelta(AssistantDelta {
                                session_id: session_id.clone(),
                                delta,
                            }));
                        }
                        ProviderEvent::ToolCall { call } => {
                            info!(
                                call_id = %call.call_id,
                                tool_name = %call.name,
                                "provider requested tool call"
                            );
                            emit_event(ServerEvent::ToolStarted(ToolStarted {
                                session_id: session_id.clone(),
                                call: call.clone(),
                            }));
                            let result = execute_builtin_tool(&call);
                            assistant_tool_calls.push(call);

                            push_tool_result_event(
                                emit_event,
                                &session_id,
                                result,
                                &mut tool_messages,
                            );
                        }
                        ProviderEvent::ToolResult { result } => {
                            info!(
                                call_id = %result.call_id,
                                is_error = result.is_error,
                                "provider emitted tool result"
                            );
                            push_tool_result_event(
                                emit_event,
                                &session_id,
                                result,
                                &mut tool_messages,
                            );
                        }
                        ProviderEvent::Message { message } => {
                            debug!(role = ?message.role, "provider emitted full message");
                            if message.role == MessageRole::Assistant {
                                assistant_message = Some(message);
                            }
                        }
                        ProviderEvent::Finished => {
                            debug!("provider stream finished");
                            break;
                        }
                    }
                }

                let assistant_message = assistant_message
                    .unwrap_or_else(|| Message::new(MessageRole::Assistant, assistant_delta_text));
                let assistant_message = Message::new(
                    MessageRole::Assistant,
                    encode_assistant_message_content(
                        assistant_message.content,
                        &assistant_tool_calls,
                    ),
                );

                if tool_messages.is_empty() {
                    if !assistant_message.content.is_empty() {
                        session.messages.push(assistant_message.clone());
                    }
                    info!(
                        final_content_chars = assistant_message.content.chars().count(),
                        "completion loop reached final response"
                    );
                    emit_event(ServerEvent::Final(FinalMessage {
                        session_id: session_id.clone(),
                        message: assistant_message,
                    }));
                    return Ok(IterationState::Finished);
                }

                if !assistant_message.content.is_empty() {
                    session.messages.push(assistant_message);
                }

                for tool_message in tool_messages {
                    session.messages.push(tool_message);
                }

                info!(
                    session_messages = session.messages.len(),
                    "tool outputs appended; continuing completion loop"
                );

                if iteration == self.max_tool_iterations {
                    warn!("maximum tool iterations reached");
                    return Err(max_tool_iterations_error(self.max_tool_iterations));
                }

                Ok(IterationState::Continue)
            }
            .instrument(info_span!(
                "runtime.completion_iteration",
                session_id = %session_id,
                iteration,
                max_tool_iterations = self.max_tool_iterations,
            ))
            .await?;

            if matches!(iteration_result, IterationState::Finished) {
                return Ok(());
            }
        }

        warn!("completion loop exited after iteration bound");
        Err(max_tool_iterations_error(self.max_tool_iterations))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSession {
    pub id: String,
    messages: Vec<Message>,
}

impl AgentSession {
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("provider stream failed: {0}")]
    Provider(#[from] ProviderError),
    #[error("maximum tool iterations exceeded ({0})")]
    MaxToolIterationsExceeded(usize),
}

fn max_tool_iterations_error(max_tool_iterations: usize) -> AgentError {
    AgentError::MaxToolIterationsExceeded(max_tool_iterations)
}

fn push_tool_result_event<F>(
    emit_event: &mut F,
    session_id: &str,
    result: ToolResult,
    tool_messages: &mut Vec<Message>,
) where
    F: FnMut(ServerEvent),
{
    if result.is_error {
        warn!(
            session_id = %session_id,
            call_id = %result.call_id,
            output_chars = result.output.chars().count(),
            "tool call completed with error"
        );
    } else {
        info!(
            session_id = %session_id,
            call_id = %result.call_id,
            output_chars = result.output.chars().count(),
            "tool call completed"
        );
    }

    tool_messages.push(tool_result_to_message(&result));
    emit_event(ServerEvent::ToolCompleted(ToolCompleted {
        session_id: session_id.to_string(),
        result,
    }));
}

static BUILTIN_TOOL_DEFINITIONS: LazyLock<Vec<ToolDefinition>> = LazyLock::new(|| {
    vec![
        ToolDefinition {
            name: "read".to_string(),
            description: "Read a UTF-8 text file from disk.".to_string(),
            parameters: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to read.",
                    },
                },
                "required": ["path"],
            }),
        },
        ToolDefinition {
            name: "write".to_string(),
            description: "Write UTF-8 content to a file, creating parent directories as needed."
                .to_string(),
            parameters: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Destination file path.",
                    },
                    "content": {
                        "type": "string",
                        "description": "UTF-8 file contents to write.",
                    },
                },
                "required": ["path", "content"],
            }),
        },
        ToolDefinition {
            name: "bash".to_string(),
            description:
                "Execute a bash command and return JSON with exit_code, stdout, and stderr."
                    .to_string(),
            parameters: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Shell command to execute.",
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Optional working directory.",
                    },
                },
                "required": ["command"],
            }),
        },
    ]
});

fn builtin_tool_definitions() -> &'static [ToolDefinition] {
    BUILTIN_TOOL_DEFINITIONS.as_slice()
}

fn execute_builtin_tool(call: &ToolCall) -> ToolResult {
    let span = info_span!(
        "runtime.execute_builtin_tool",
        call_id = %call.call_id,
        tool_name = %call.name,
    );
    span.in_scope(|| {
        let result = match call.name.as_str() {
            "read" => read_tool(call),
            "write" => write_tool(call),
            "bash" => bash_tool(call),
            _ => tool_error(
                &call.call_id,
                format!(
                    "unknown tool `{}`; expected one of: read, write, bash",
                    call.name
                ),
            ),
        };

        if result.is_error {
            warn!(
                output_chars = result.output.chars().count(),
                "builtin tool returned error"
            );
        } else {
            info!(
                output_chars = result.output.chars().count(),
                "builtin tool succeeded"
            );
        }

        result
    })
}

fn read_tool(call: &ToolCall) -> ToolResult {
    let path = match required_string_field(&call.input, "path", false) {
        Ok(path) => path,
        Err(message) => return tool_error(&call.call_id, message),
    };

    debug!(path = %path, "reading file");
    match fs::read(Path::new(&path)) {
        Ok(bytes) => {
            info!(path = %path, byte_count = bytes.len(), "read tool completed");
            tool_ok(&call.call_id, String::from_utf8_lossy(&bytes).to_string())
        }
        Err(error) => {
            warn!(path = %path, %error, "read tool failed");
            tool_error(&call.call_id, format!("read failed for `{path}`: {error}"))
        }
    }
}

fn write_tool(call: &ToolCall) -> ToolResult {
    let path = match required_string_field(&call.input, "path", false) {
        Ok(path) => path,
        Err(message) => return tool_error(&call.call_id, message),
    };

    let content = match required_string_field(&call.input, "content", true) {
        Ok(content) => content,
        Err(message) => return tool_error(&call.call_id, message),
    };

    debug!(
        path = %path,
        content_bytes = content.len(),
        "writing file"
    );
    let path_buf = PathBuf::from(&path);
    if let Some(parent) = path_buf.parent()
        && !parent.as_os_str().is_empty()
        && let Err(error) = fs::create_dir_all(parent)
    {
        warn!(
            parent = %parent.display(),
            %error,
            "write tool failed to create parent directory"
        );
        return tool_error(
            &call.call_id,
            format!(
                "write failed creating parent directory `{}`: {error}",
                parent.display()
            ),
        );
    }

    if let Err(error) = fs::write(&path_buf, content.as_bytes()) {
        warn!(path = %path_buf.display(), %error, "write tool failed");
        return tool_error(&call.call_id, format!("write failed for `{path}`: {error}"));
    }

    info!(
        path = %path_buf.display(),
        content_bytes = content.len(),
        "write tool completed"
    );
    tool_ok(
        &call.call_id,
        format!("wrote {} bytes to {}", content.len(), path_buf.display()),
    )
}

fn bash_tool(call: &ToolCall) -> ToolResult {
    let command = match required_string_field(&call.input, "command", false) {
        Ok(command) => command,
        Err(message) => return tool_error(&call.call_id, message),
    };

    let cwd = match optional_string_field(&call.input, "cwd") {
        Ok(cwd) => cwd,
        Err(message) => return tool_error(&call.call_id, message),
    };

    debug!(
        command_chars = command.chars().count(),
        has_cwd = cwd.is_some(),
        "executing bash tool"
    );
    let mut command_builder = Command::new("bash");
    command_builder.arg("-lc").arg(&command);

    if let Some(cwd) = cwd {
        command_builder.current_dir(cwd);
    }

    let output = match command_builder.output() {
        Ok(output) => output,
        Err(error) => {
            warn!(%error, "bash tool failed to start process");
            return tool_error(&call.call_id, format!("bash execution failed: {error}"));
        }
    };

    let payload = json!({
        "exit_code": output.status.code(),
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr),
    })
    .to_string();

    info!(
        exit_code = ?output.status.code(),
        stdout_bytes = output.stdout.len(),
        stderr_bytes = output.stderr.len(),
        "bash tool process completed"
    );
    if output.status.success() {
        tool_ok(&call.call_id, payload)
    } else {
        tool_error(&call.call_id, payload)
    }
}

fn required_string_field(
    input: &Value,
    field_name: &str,
    allow_empty: bool,
) -> Result<String, String> {
    let value = input
        .get(field_name)
        .ok_or_else(|| format!("missing required field `{field_name}`"))?;

    let string_value = value
        .as_str()
        .ok_or_else(|| format!("field `{field_name}` must be a string"))?;

    if !allow_empty && string_value.trim().is_empty() {
        return Err(format!("field `{field_name}` must not be empty"));
    }

    Ok(string_value.to_string())
}

fn optional_string_field(input: &Value, field_name: &str) -> Result<Option<String>, String> {
    match input.get(field_name) {
        Some(value) => {
            let string_value = value
                .as_str()
                .ok_or_else(|| format!("field `{field_name}` must be a string"))?;

            if string_value.trim().is_empty() {
                Ok(None)
            } else {
                Ok(Some(string_value.to_string()))
            }
        }
        None => Ok(None),
    }
}

fn tool_result_to_message(result: &ToolResult) -> Message {
    let payload = json!({
        "call_id": result.call_id,
        "is_error": result.is_error,
        "output": result.output,
    });
    Message::new(MessageRole::Tool, payload.to_string())
}

fn tool_ok(call_id: &str, output: impl Into<String>) -> ToolResult {
    ToolResult {
        call_id: call_id.to_string(),
        output: output.into(),
        is_error: false,
    }
}

fn tool_error(call_id: &str, output: impl Into<String>) -> ToolResult {
    ToolResult {
        call_id: call_id.to_string(),
        output: output.into(),
        is_error: true,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::{SystemTime, UNIX_EPOCH},
    };

    use rho_core::{
        message::decode_assistant_message_content,
        providers::{ModelKind, ProviderKind, ProviderRequest, ProviderStream},
        tool::ToolCall,
    };
    use serde_json::{Value, json};

    use super::*;
    use crate::test_helpers::{FakeProvider, FakeResponse, FakeResponseQueue};

    #[test]
    fn run_user_message_without_tool_calls_emits_final() {
        futures::executor::block_on(async {
            let runtime = AgentRuntime::new();
            let mut session = runtime.start_session("session-1");
            let provider = FakeProvider::new(vec![vec![
                Ok(ProviderEvent::AssistantDelta {
                    delta: "hello".to_string(),
                }),
                Ok(ProviderEvent::Message {
                    message: Message::new(MessageRole::Assistant, "hello"),
                }),
                Ok(ProviderEvent::Finished),
            ]]);

            let events = runtime
                .run_user_message(&mut session, &provider, ModelKind::Gpt52, "hi")
                .await
                .expect("run should succeed");

            assert_eq!(events.len(), 2);
            assert!(matches!(
                &events[0],
                ServerEvent::AssistantDelta(AssistantDelta { session_id, delta })
                if session_id == "session-1" && delta == "hello"
            ));
            assert!(matches!(
                &events[1],
                ServerEvent::Final(FinalMessage { session_id, message })
                if session_id == "session-1"
                    && message.role == MessageRole::Assistant
                    && message.content == "hello"
            ));
            assert_eq!(
                session.messages(),
                [
                    Message::new(MessageRole::User, "hi"),
                    Message::new(MessageRole::Assistant, "hello")
                ]
            );
            assert_eq!(provider.requests().len(), 1);
            let tool_names: Vec<String> = provider.requests()[0]
                .tools
                .iter()
                .map(|tool| tool.name.clone())
                .collect();
            assert_eq!(tool_names, ["read", "write", "bash"]);
        });
    }

    #[test]
    fn run_user_message_executes_read_tool_and_continues() {
        futures::executor::block_on(async {
            let runtime = AgentRuntime::new();
            let mut session = runtime.start_session("session-2");
            let file_path = temp_file_path("read");
            fs::write(&file_path, "file-content").expect("write fixture file");

            let provider = FakeProvider::new(vec![
                vec![
                    Ok(ProviderEvent::ToolCall {
                        call: ToolCall {
                            id: None,
                            call_id: "call-1".to_string(),
                            name: "read".to_string(),
                            input: json!({ "path": file_path.display().to_string() }),
                        },
                    }),
                    Ok(ProviderEvent::Message {
                        message: Message::new(MessageRole::Assistant, ""),
                    }),
                    Ok(ProviderEvent::Finished),
                ],
                vec![
                    Ok(ProviderEvent::AssistantDelta {
                        delta: "done".to_string(),
                    }),
                    Ok(ProviderEvent::Message {
                        message: Message::new(MessageRole::Assistant, "done"),
                    }),
                    Ok(ProviderEvent::Finished),
                ],
            ]);

            let events = runtime
                .run_user_message(&mut session, &provider, ModelKind::Gpt52, "read the file")
                .await
                .expect("run should succeed");

            assert!(events
                .iter()
                .any(|event| matches!(event, ServerEvent::ToolStarted(tool_started) if tool_started.call.name == "read")));
            let tool_completed = events
                .iter()
                .find_map(|event| match event {
                    ServerEvent::ToolCompleted(tool_completed) => Some(tool_completed),
                    _ => None,
                })
                .expect("expected a tool completion event");
            assert!(!tool_completed.result.is_error);
            assert_eq!(tool_completed.result.output, "file-content");

            assert!(events
                .iter()
                .any(|event| matches!(event, ServerEvent::Final(final_message) if final_message.message.content == "done")));

            let requests = provider.requests();
            assert_eq!(requests.len(), 2);
            let second_request_tool_message = requests[1]
                .messages
                .iter()
                .find(|message| message.role == MessageRole::Tool)
                .expect("second request should contain a tool message");
            let payload: Value = serde_json::from_str(&second_request_tool_message.content)
                .expect("tool message should be JSON");
            assert_eq!(payload["call_id"], json!("call-1"));
            assert_eq!(payload["is_error"], json!(false));
            assert_eq!(payload["output"], json!("file-content"));
            let second_request_assistant_message = requests[1]
                .messages
                .iter()
                .find(|message| message.role == MessageRole::Assistant)
                .expect("second request should contain assistant tool calls");
            let parsed_assistant =
                decode_assistant_message_content(&second_request_assistant_message.content);
            assert_eq!(parsed_assistant.text, "");
            assert_eq!(parsed_assistant.tool_calls.len(), 1);
            assert_eq!(parsed_assistant.tool_calls[0].call_id, "call-1");
            assert_eq!(parsed_assistant.tool_calls[0].name, "read");
            assert_eq!(
                parsed_assistant.tool_calls[0].input,
                json!({ "path": file_path.display().to_string() })
            );

            let _ = fs::remove_file(file_path);
        });
    }

    #[test]
    fn tool_failures_surface_as_structured_tool_results() {
        futures::executor::block_on(async {
            let runtime = AgentRuntime::new();
            let mut session = runtime.start_session("session-3");
            let provider = FakeProvider::new(vec![
                vec![
                    Ok(ProviderEvent::ToolCall {
                        call: ToolCall {
                            id: None,
                            call_id: "call-2".to_string(),
                            name: "missing_tool".to_string(),
                            input: json!({}),
                        },
                    }),
                    Ok(ProviderEvent::Message {
                        message: Message::new(MessageRole::Assistant, ""),
                    }),
                    Ok(ProviderEvent::Finished),
                ],
                vec![
                    Ok(ProviderEvent::Message {
                        message: Message::new(MessageRole::Assistant, "tool failed"),
                    }),
                    Ok(ProviderEvent::Finished),
                ],
            ]);

            let events = runtime
                .run_user_message(&mut session, &provider, ModelKind::Gpt52, "do something")
                .await
                .expect("run should continue after tool failure");

            let tool_completed = events
                .iter()
                .find_map(|event| match event {
                    ServerEvent::ToolCompleted(tool_completed) => Some(tool_completed),
                    _ => None,
                })
                .expect("expected tool completion");
            assert!(tool_completed.result.is_error);
            assert!(tool_completed.result.output.contains("unknown tool"));
            assert!(events
                .iter()
                .any(|event| matches!(event, ServerEvent::Final(final_message) if final_message.message.content == "tool failed")));
        });
    }

    #[test]
    fn run_user_message_enforces_tool_iteration_limit() {
        futures::executor::block_on(async {
            let runtime = AgentRuntime::with_max_tool_iterations(1);
            let mut session = runtime.start_session("session-4");
            let provider = FakeProvider::new(vec![
                vec![
                    Ok(ProviderEvent::ToolCall {
                        call: ToolCall {
                            id: None,
                            call_id: "call-3".to_string(),
                            name: "bash".to_string(),
                            input: json!({ "command": "echo first" }),
                        },
                    }),
                    Ok(ProviderEvent::Finished),
                ],
                vec![
                    Ok(ProviderEvent::ToolCall {
                        call: ToolCall {
                            id: None,
                            call_id: "call-4".to_string(),
                            name: "bash".to_string(),
                            input: json!({ "command": "echo second" }),
                        },
                    }),
                    Ok(ProviderEvent::Finished),
                ],
            ]);

            let error = runtime
                .run_user_message(&mut session, &provider, ModelKind::Gpt52, "loop")
                .await
                .expect_err("run should fail once the limit is exceeded");
            assert!(matches!(error, AgentError::MaxToolIterationsExceeded(1)));
        });
    }

    #[test]
    fn run_user_message_borrows_history_and_reuses_builtin_tools() {
        futures::executor::block_on(async {
            let runtime = AgentRuntime::new();
            let mut session = runtime.start_session("session-5");
            let provider = PointerRecordingProvider::new(vec![vec![Ok(ProviderEvent::Finished)]]);

            runtime
                .run_user_message(&mut session, &provider, ModelKind::Gpt52, "hello")
                .await
                .expect("run should succeed");

            let request_pointers = provider.request_pointers();
            assert_eq!(request_pointers.len(), 1);
            assert_eq!(
                request_pointers[0].message_ptr,
                session.messages().as_ptr() as usize
            );
            assert_eq!(
                request_pointers[0].tool_ptr,
                builtin_tool_definitions().as_ptr() as usize
            );
        });
    }

    #[derive(Debug, Clone, Copy)]
    struct RequestPointers {
        message_ptr: usize,
        tool_ptr: usize,
    }

    #[derive(Debug, Clone)]
    struct PointerRecordingProvider {
        responses: Arc<Mutex<FakeResponseQueue>>,
        request_pointers: Arc<Mutex<Vec<RequestPointers>>>,
    }

    impl PointerRecordingProvider {
        fn new(responses: Vec<FakeResponse>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses.into_iter().collect())),
                request_pointers: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn request_pointers(&self) -> Vec<RequestPointers> {
            self.request_pointers
                .lock()
                .expect("request pointers mutex should be available")
                .clone()
        }
    }

    impl Provider for PointerRecordingProvider {
        fn kind(&self) -> ProviderKind {
            ProviderKind::OpenAi
        }

        fn stream(&self, request: ProviderRequest<'_>) -> ProviderStream {
            self.request_pointers
                .lock()
                .expect("request pointers mutex should be available")
                .push(RequestPointers {
                    message_ptr: request.messages.as_ptr() as usize,
                    tool_ptr: request.tools.as_ptr() as usize,
                });

            let events = self
                .responses
                .lock()
                .expect("responses mutex should be available")
                .pop_front()
                .unwrap_or_default();

            Box::pin(futures_util::stream::iter(events))
        }
    }

    fn temp_file_path(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "rho-agent-{label}-{nanos}-{}.txt",
            std::process::id()
        ))
    }
}
