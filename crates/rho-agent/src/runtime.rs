use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use futures_util::StreamExt;
use rho_core::{
    Message, MessageRole,
    protocol::{
        AssistantDelta, FinalMessage, PROTOCOL_VERSION, ServerEvent, ToolCompleted, ToolStarted,
    },
    providers::{Provider, ProviderError, ProviderRequest},
    stream::ProviderEvent,
    tool::{ToolCall, ToolDefinition, ToolResult},
};
use serde_json::{Value, json};
use thiserror::Error;

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
        model: impl Into<String>,
        user_content: impl Into<String>,
    ) -> Result<Vec<ServerEvent>, AgentError> {
        let model = model.into();
        let user_content = user_content.into();
        let mut emitted_events = Vec::new();
        self.run_user_message_streaming(session, provider, model, user_content, |event| {
            emitted_events.push(event);
        })
        .await?;
        Ok(emitted_events)
    }

    pub async fn run_user_message_streaming<F>(
        &self,
        session: &mut AgentSession,
        provider: &dyn Provider,
        model: impl Into<String>,
        user_content: impl Into<String>,
        mut emit_event: F,
    ) -> Result<(), AgentError>
    where
        F: FnMut(ServerEvent),
    {
        let model = model.into();
        let user_content = user_content.into();
        session
            .messages
            .push(Message::new(MessageRole::User, user_content));
        self.run_completion_loop(session, provider, &model, &mut emit_event)
            .await
    }

    async fn run_completion_loop<F>(
        &self,
        session: &mut AgentSession,
        provider: &dyn Provider,
        model: &str,
        emit_event: &mut F,
    ) -> Result<(), AgentError>
    where
        F: FnMut(ServerEvent),
    {
        let session_id = session.id.clone();
        for iteration in 0..=self.max_tool_iterations {
            let request = ProviderRequest::new(model.to_string(), session.messages.clone())
                .with_tools(builtin_tool_definitions());
            let mut stream = provider.stream(request);

            let mut assistant_message = None;
            let mut assistant_delta_text = String::new();
            let mut tool_results = Vec::new();

            while let Some(next_event) = stream.next().await {
                match next_event? {
                    ProviderEvent::AssistantDelta { delta } => {
                        assistant_delta_text.push_str(&delta);
                        emit_event(ServerEvent::AssistantDelta(AssistantDelta {
                            session_id: session_id.clone(),
                            delta,
                        }));
                    }
                    ProviderEvent::ToolCall { call } => {
                        emit_event(ServerEvent::ToolStarted(ToolStarted {
                            session_id: session_id.clone(),
                            call: call.clone(),
                        }));

                        let result = execute_builtin_tool(&call);
                        push_tool_result_event(emit_event, &session_id, result, &mut tool_results);
                    }
                    ProviderEvent::ToolResult { result } => {
                        push_tool_result_event(emit_event, &session_id, result, &mut tool_results);
                    }
                    ProviderEvent::Message { message } => {
                        if message.role == MessageRole::Assistant {
                            assistant_message = Some(message);
                        }
                    }
                    ProviderEvent::Finished => break,
                }
            }

            let assistant_message = assistant_message.unwrap_or_else(|| {
                Message::new(MessageRole::Assistant, assistant_delta_text.clone())
            });

            if !assistant_message.content.is_empty() {
                session.messages.push(assistant_message.clone());
            }

            if tool_results.is_empty() {
                emit_event(ServerEvent::Final(FinalMessage {
                    session_id: session_id.clone(),
                    message: assistant_message,
                }));
                return Ok(());
            }

            for tool_result in tool_results {
                session.messages.push(tool_result_to_message(&tool_result));
            }

            if iteration == self.max_tool_iterations {
                return Err(max_tool_iterations_error(self.max_tool_iterations));
            }
        }

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
    tool_results: &mut Vec<ToolResult>,
) where
    F: FnMut(ServerEvent),
{
    emit_event(ServerEvent::ToolCompleted(ToolCompleted {
        session_id: session_id.to_string(),
        result: result.clone(),
    }));
    tool_results.push(result);
}

fn builtin_tool_definitions() -> Vec<ToolDefinition> {
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
}

fn execute_builtin_tool(call: &ToolCall) -> ToolResult {
    match call.name.as_str() {
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
    }
}

fn read_tool(call: &ToolCall) -> ToolResult {
    let path = match required_string_field(&call.input, "path", false) {
        Ok(path) => path,
        Err(message) => return tool_error(&call.call_id, message),
    };

    match fs::read(Path::new(&path)) {
        Ok(bytes) => tool_ok(&call.call_id, String::from_utf8_lossy(&bytes).to_string()),
        Err(error) => tool_error(&call.call_id, format!("read failed for `{path}`: {error}")),
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

    let path_buf = PathBuf::from(&path);
    if let Some(parent) = path_buf.parent()
        && !parent.as_os_str().is_empty()
        && let Err(error) = fs::create_dir_all(parent)
    {
        return tool_error(
            &call.call_id,
            format!(
                "write failed creating parent directory `{}`: {error}",
                parent.display()
            ),
        );
    }

    if let Err(error) = fs::write(&path_buf, content.as_bytes()) {
        return tool_error(&call.call_id, format!("write failed for `{path}`: {error}"));
    }

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

    let mut command_builder = Command::new("bash");
    command_builder.arg("-lc").arg(&command);

    if let Some(cwd) = cwd {
        command_builder.current_dir(cwd);
    }

    let output = match command_builder.output() {
        Ok(output) => output,
        Err(error) => {
            return tool_error(&call.call_id, format!("bash execution failed: {error}"));
        }
    };

    let payload = json!({
        "exit_code": output.status.code(),
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr),
    })
    .to_string();

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
        collections::VecDeque,
        sync::{Arc, Mutex},
        time::{SystemTime, UNIX_EPOCH},
    };

    use rho_core::{
        providers::{ProviderKind, ProviderRequest, ProviderStream},
        tool::ToolCall,
    };
    use serde_json::{Value, json};

    use super::*;

    type FakeResponse = Vec<Result<ProviderEvent, ProviderError>>;
    type FakeResponseQueue = VecDeque<FakeResponse>;

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
                .run_user_message(&mut session, &provider, "test-model", "hi")
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
                .run_user_message(&mut session, &provider, "test-model", "read the file")
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
                .run_user_message(&mut session, &provider, "test-model", "do something")
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
                            call_id: "call-4".to_string(),
                            name: "bash".to_string(),
                            input: json!({ "command": "echo second" }),
                        },
                    }),
                    Ok(ProviderEvent::Finished),
                ],
            ]);

            let error = runtime
                .run_user_message(&mut session, &provider, "test-model", "loop")
                .await
                .expect_err("run should fail once the limit is exceeded");
            assert!(matches!(error, AgentError::MaxToolIterationsExceeded(1)));
        });
    }

    #[derive(Debug, Clone)]
    struct FakeProvider {
        responses: Arc<Mutex<FakeResponseQueue>>,
        requests: Arc<Mutex<Vec<ProviderRequest>>>,
    }

    impl FakeProvider {
        fn new(responses: Vec<FakeResponse>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses.into_iter().collect())),
                requests: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn requests(&self) -> Vec<ProviderRequest> {
            self.requests
                .lock()
                .expect("requests mutex should be available")
                .clone()
        }
    }

    impl Provider for FakeProvider {
        fn kind(&self) -> ProviderKind {
            ProviderKind::OpenAi
        }

        fn stream(&self, request: ProviderRequest) -> ProviderStream {
            self.requests
                .lock()
                .expect("requests mutex should be available")
                .push(request);

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
