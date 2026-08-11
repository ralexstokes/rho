//! Phase-1 walking skeleton: one user turn with a provider and a bash tool.

#![allow(clippy::disallowed_methods)]

mod credentials;

use std::{
    env,
    io::{self, Write as _},
    process::ExitStatus,
};

use anyhow::{Context as _, Result, anyhow, bail};
use futures_util::StreamExt as _;
use rho_ai::{
    AssistantMessage, CancellationToken, ContentBlock, Message, ModelId, Provider, ProviderId,
    Request, StopReason, StreamEvent, ThinkingLevel, ToolCallId, ToolDefinition, ToolResult,
};
use rho_ai_anthropic::AnthropicProvider;
use rho_ai_openai::OpenAiProvider;
use serde_json::{Value, json};

use crate::credentials::resolve_credential;

const MAX_MODEL_STEPS: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProviderChoice {
    OpenAi,
    Anthropic,
}

impl ProviderChoice {
    fn id(self) -> ProviderId {
        match self {
            Self::OpenAi => ProviderId::from("openai"),
            Self::Anthropic => ProviderId::from("anthropic"),
        }
    }

    fn default_model(self) -> ModelId {
        match self {
            Self::OpenAi => ModelId::from("gpt-5.6-sol"),
            Self::Anthropic => ModelId::from("claude-sonnet-5"),
        }
    }
}

#[derive(Debug)]
struct Cli {
    provider: ProviderChoice,
    model: ModelId,
    max_output_tokens: u64,
    thinking: ThinkingLevel,
    prompt: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = parse_args(env::args().skip(1))?;
    let credential = resolve_credential(&cli.provider.id())?;
    let provider: Box<dyn Provider> = match cli.provider {
        ProviderChoice::OpenAi => Box::new(OpenAiProvider::new(credential.expose_api_key())?),
        ProviderChoice::Anthropic => Box::new(AnthropicProvider::new(credential.expose_api_key())?),
    };
    let cancellation = CancellationToken::new();
    let signal = cancellation.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal.cancel();
        }
    });

    run_turn(provider.as_ref(), &cli, cancellation).await
}

async fn run_turn(
    provider: &dyn Provider,
    cli: &Cli,
    cancellation: CancellationToken,
) -> Result<()> {
    let mut request = Request {
        system: concat!(
            "You are a concise coding assistant. Use the bash tool when you need to inspect or ",
            "change the current workspace. Report what you did and whether it succeeded."
        )
        .to_owned(),
        messages: vec![Message::user(cli.prompt.clone())],
        tools: vec![bash_definition()],
        model: cli.model.clone(),
        max_output_tokens: cli.max_output_tokens,
        thinking: cli.thinking,
    };

    for _ in 0..MAX_MODEL_STEPS {
        let (message, had_text) =
            collect_message(provider, request.clone(), cancellation.clone()).await?;
        let stop = message.stop;
        if had_text {
            println!();
        }
        request.messages.push(Message::Assistant(message.clone()));

        match stop {
            StopReason::ToolUse => {
                let results = execute_tool_calls(&message, false, &cancellation).await?;
                if results.is_empty() {
                    bail!("provider stopped for tool use without returning a tool call");
                }
                request
                    .messages
                    .extend(results.into_iter().map(Message::ToolResult));
            }
            StopReason::Length => {
                let results = execute_tool_calls(&message, true, &cancellation).await?;
                if results.is_empty() {
                    bail!("provider output was truncated before completing the turn");
                }
                request
                    .messages
                    .extend(results.into_iter().map(Message::ToolResult));
            }
            StopReason::Stop => {
                return Ok(());
            }
            StopReason::Paused => {}
            StopReason::Refusal => bail!("provider refused the request"),
            StopReason::Error => bail!("provider ended the generation with an error"),
            StopReason::Aborted => bail!("provider request was aborted"),
            _ => bail!("provider returned an unsupported stop reason"),
        }
    }
    bail!("turn exceeded the limit of {MAX_MODEL_STEPS} model steps")
}

async fn collect_message(
    provider: &dyn Provider,
    request: Request,
    cancellation: CancellationToken,
) -> Result<(AssistantMessage, bool)> {
    let mut stream = provider.stream(request, cancellation);
    let mut streamed_text = false;
    while let Some(event) = stream.next().await {
        match event {
            StreamEvent::Delta {
                kind: rho_ai::DeltaKind::Text,
                delta,
                ..
            } => {
                streamed_text = true;
                print!("{delta}");
                io::stdout().flush()?;
            }
            StreamEvent::Done(message) => {
                if !streamed_text {
                    for block in &message.blocks {
                        if let ContentBlock::Text { text } = block {
                            print!("{text}");
                            streamed_text = true;
                        }
                    }
                    io::stdout().flush()?;
                }
                return Ok((message, streamed_text));
            }
            StreamEvent::Error(error) => return Err(error.into()),
            StreamEvent::Start | StreamEvent::Delta { .. } | StreamEvent::BlockDone { .. } => {}
            _ => bail!("provider stream contained an unsupported event"),
        }
    }
    Err(anyhow!("provider stream ended without Done or Error"))
}

async fn execute_tool_calls(
    message: &AssistantMessage,
    truncated: bool,
    cancellation: &CancellationToken,
) -> Result<Vec<ToolResult>> {
    let mut results = Vec::new();
    for block in &message.blocks {
        if cancellation.is_cancelled() {
            bail!("tool execution cancelled");
        }
        match block {
            ContentBlock::ToolCall { id, .. } if truncated => {
                results.push(ToolResult {
                    call_id: id.clone(),
                    content: "tool call was not executed because the model output was truncated"
                        .to_owned(),
                    is_error: true,
                });
            }
            ContentBlock::ToolCall { id, name, args } if name == "bash" => {
                results.push(execute_bash(id, args, cancellation).await?);
            }
            ContentBlock::ToolCall { id, name, .. } => results.push(ToolResult {
                call_id: id.clone(),
                content: format!("unknown tool {name:?}"),
                is_error: true,
            }),
            ContentBlock::RejectedToolCall { id, error, .. } => results.push(ToolResult {
                call_id: id.clone(),
                content: format!("tool arguments rejected: {}", error.message),
                is_error: true,
            }),
            _ => {}
        }
    }
    Ok(results)
}

async fn execute_bash(
    call_id: &ToolCallId,
    arguments: &Value,
    cancellation: &CancellationToken,
) -> Result<ToolResult> {
    let Some(command) = arguments.get("command").and_then(Value::as_str) else {
        return Ok(ToolResult {
            call_id: call_id.clone(),
            content: "bash arguments were missing a string command".to_owned(),
            is_error: true,
        });
    };
    if cancellation.is_cancelled() {
        bail!("bash tool execution cancelled");
    }
    let mut process = tokio::process::Command::new("bash");
    process.arg("-lc").arg(command).kill_on_drop(true);
    let output = process.output();
    tokio::pin!(output);
    let output = tokio::select! {
        biased;
        () = cancellation.cancelled() => bail!("bash tool execution cancelled"),
        output = &mut output => output,
    };
    Ok(match output {
        Ok(output) => ToolResult {
            call_id: call_id.clone(),
            content: format_process_output(output.status, &output.stdout, &output.stderr),
            is_error: !output.status.success(),
        },
        Err(error) => ToolResult {
            call_id: call_id.clone(),
            content: format!("failed to start shell: {error}"),
            is_error: true,
        },
    })
}

fn format_process_output(status: ExitStatus, stdout: &[u8], stderr: &[u8]) -> String {
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
    format!("status: {status}\nstdout:\n{stdout}\nstderr:\n{stderr}")
}

fn bash_definition() -> ToolDefinition {
    ToolDefinition::new(
        "bash",
        concat!(
            "Run one command using `bash -lc` in rho's current working directory. ",
            "The command is not sandboxed by rho."
        ),
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Complete shell command to run."
                }
            },
            "required": ["command"],
            "additionalProperties": false
        }),
    )
}

fn parse_args(arguments: impl IntoIterator<Item = String>) -> Result<Cli> {
    let mut provider = ProviderChoice::OpenAi;
    let mut model = None;
    let mut max_output_tokens = 16_384;
    let mut thinking = ThinkingLevel::High;
    let mut prompt = Vec::new();
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--provider" => {
                provider = match arguments.next().as_deref() {
                    Some("openai") => ProviderChoice::OpenAi,
                    Some("anthropic") => ProviderChoice::Anthropic,
                    Some(value) => {
                        bail!("unknown provider {value:?}; expected openai or anthropic")
                    }
                    None => bail!("--provider requires a value"),
                };
            }
            "--model" => {
                model = Some(ModelId::from(
                    arguments.next().context("--model requires a value")?,
                ));
            }
            "--max-output-tokens" => {
                max_output_tokens = arguments
                    .next()
                    .context("--max-output-tokens requires a value")?
                    .parse()
                    .context("--max-output-tokens must be an integer")?;
            }
            "--thinking" => {
                thinking = match arguments.next().as_deref() {
                    Some("none") => ThinkingLevel::None,
                    Some("low") => ThinkingLevel::Low,
                    Some("medium") => ThinkingLevel::Medium,
                    Some("high") => ThinkingLevel::High,
                    Some("xhigh") => ThinkingLevel::Xhigh,
                    Some("max") => ThinkingLevel::Max,
                    Some(value) => bail!(
                        "unknown thinking level {value:?}; expected none, low, medium, high, xhigh, or max"
                    ),
                    None => bail!("--thinking requires a value"),
                };
            }
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            value if value.starts_with('-') => bail!("unknown option {value:?}"),
            _ => {
                prompt.push(argument);
                prompt.extend(arguments);
                break;
            }
        }
    }
    if prompt.is_empty() {
        bail!("a prompt is required; run rho-cli --help for usage");
    }
    Ok(Cli {
        provider,
        model: model.unwrap_or_else(|| provider.default_model()),
        max_output_tokens,
        thinking,
        prompt: prompt.join(" "),
    })
}

fn print_help() {
    println!(
        "rho-cli [--provider openai|anthropic] [--model ID] \\\n         [--max-output-tokens N] [--thinking LEVEL] PROMPT\n\n\\
         Runs one agent turn with an unsandboxed bash tool. Credentials are read from \\\n         OPENAI_API_KEY or ANTHROPIC_API_KEY, then from ~/.rho/credentials.json."
    );
}

#[cfg(test)]
mod tests {
    use rho_ai::{
        ModelInfo, Usage,
        faux::{FauxProvider, Script},
    };

    use super::*;

    #[test]
    fn arguments_select_provider_model_and_prompt() {
        let cli = parse_args([
            "--provider".to_owned(),
            "anthropic".to_owned(),
            "--thinking".to_owned(),
            "medium".to_owned(),
            "inspect".to_owned(),
            "the repo".to_owned(),
        ])
        .unwrap();
        assert_eq!(cli.provider, ProviderChoice::Anthropic);
        assert_eq!(cli.model, ModelId::from("claude-sonnet-5"));
        assert_eq!(cli.thinking, ThinkingLevel::Medium);
        assert_eq!(cli.prompt, "inspect the repo");
    }

    #[tokio::test]
    async fn bash_walking_skeleton_executes_a_command() {
        let result = execute_bash(
            &ToolCallId::from("call-1"),
            &json!({"command": "printf phase1"}),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("phase1"));
    }

    #[tokio::test]
    async fn cancellation_prevents_bash_execution() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let error = execute_bash(
            &ToolCallId::from("call-1"),
            &json!({"command": "printf should-not-run"}),
            &cancellation,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
    }

    #[tokio::test]
    async fn cancellation_stops_in_flight_bash_execution() {
        let cancellation = CancellationToken::new();
        let signal = cancellation.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            signal.cancel();
        });
        let started = std::time::Instant::now();
        let error = execute_bash(
            &ToolCallId::from("call-1"),
            &json!({"command": "sleep 5"}),
            &cancellation,
        )
        .await
        .unwrap_err();
        canceller.join().unwrap();

        assert!(error.to_string().contains("cancelled"));
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[tokio::test]
    async fn length_terminated_tool_calls_are_failed_without_execution() {
        let message = AssistantMessage {
            blocks: vec![ContentBlock::ToolCall {
                id: ToolCallId::from("call-1"),
                name: "bash".to_owned(),
                args: json!({"command": "exit 99"}),
            }],
            stop: StopReason::Length,
            usage: rho_ai::Usage::default(),
            provider: ProviderId::from("faux"),
            model: ModelId::from("faux"),
        };
        let results = execute_tool_calls(&message, true, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].is_error);
        assert!(results[0].content.contains("not executed"));
    }

    #[tokio::test]
    async fn paused_provider_response_continues_the_same_logical_turn() {
        let cli = Cli {
            provider: ProviderChoice::OpenAi,
            model: ModelId::from("faux"),
            max_output_tokens: 100,
            thinking: ThinkingLevel::None,
            prompt: "hello".to_owned(),
        };
        let initial = Request {
            system: concat!(
                "You are a concise coding assistant. Use the bash tool when you need to inspect or ",
                "change the current workspace. Report what you did and whether it succeeded."
            )
            .to_owned(),
            messages: vec![Message::user("hello")],
            tools: vec![bash_definition()],
            model: ModelId::from("faux"),
            max_output_tokens: 100,
            thinking: ThinkingLevel::None,
        };
        let paused = AssistantMessage {
            blocks: Vec::new(),
            stop: StopReason::Paused,
            usage: Usage::default(),
            provider: ProviderId::from("faux"),
            model: ModelId::from("faux"),
        };
        let done = AssistantMessage {
            blocks: Vec::new(),
            stop: StopReason::Stop,
            usage: Usage::default(),
            provider: ProviderId::from("faux"),
            model: ModelId::from("faux"),
        };
        let mut continued = initial.clone();
        continued.messages.push(Message::Assistant(paused.clone()));
        let provider = FauxProvider::new(
            vec![ModelInfo {
                id: ModelId::from("faux"),
                display_name: "Faux".to_owned(),
                context_tokens: None,
                max_output_tokens: None,
            }],
            [
                Script {
                    request: initial,
                    events: vec![StreamEvent::Start, StreamEvent::Done(paused)],
                },
                Script {
                    request: continued,
                    events: vec![StreamEvent::Start, StreamEvent::Done(done)],
                },
            ],
        );

        run_turn(&provider, &cli, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(provider.remaining(), 0);
    }
}
