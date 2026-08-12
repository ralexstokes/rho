use std::{
    collections::VecDeque,
    io,
    path::PathBuf,
    process::{ExitStatus, Stdio},
    time::Duration,
};

use rho_ai::{CancellationToken, ToolDefinition};
use rho_core::ReplaySafety;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt as _};

use super::{path, truncate};
use crate::{Tool, ToolFuture, ToolOutput};

/// Executes unsandboxed shell commands from one working directory.
pub struct BashTool {
    definition: ToolDefinition,
    cwd: PathBuf,
}

impl BashTool {
    /// Creates a bash tool rooted at `cwd`.
    pub fn new(cwd: impl Into<PathBuf>) -> io::Result<Self> {
        Ok(Self::from_absolute(path::absolute(cwd.into())?))
    }

    pub(super) fn from_absolute(cwd: PathBuf) -> Self {
        Self {
            definition: ToolDefinition::new(
                "bash",
                concat!(
                    "Execute a command with `bash -lc` in the current working directory. ",
                    "Returns stdout and stderr, keeping the last 2000 lines or 50KB. ",
                    "The command is not sandboxed by rho."
                ),
                json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "Complete shell command." },
                        "timeout_seconds": { "type": "integer", "minimum": 1, "maximum": 3600 }
                    },
                    "required": ["command"],
                    "additionalProperties": false
                }),
            ),
            cwd,
        }
    }
}

#[derive(Deserialize)]
struct BashInput {
    command: String,
    timeout_seconds: Option<u64>,
}

impl Tool for BashTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    fn replay_safety(&self) -> ReplaySafety {
        ReplaySafety::Never
    }

    fn execute(&self, arguments: Value, cancellation: CancellationToken) -> ToolFuture<'_> {
        let cwd = self.cwd.clone();
        Box::pin(async move {
            let input: BashInput = match serde_json::from_value(arguments) {
                Ok(input) => input,
                Err(error) => return ToolOutput::error(format!("invalid bash arguments: {error}")),
            };
            if cancellation.is_cancelled() {
                return ToolOutput::error("bash cancelled");
            }
            let mut command = tokio::process::Command::new("bash");
            command
                .arg("-lc")
                .arg(&input.command)
                .current_dir(&cwd)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            #[cfg(unix)]
            command.process_group(0);
            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(error) => return ToolOutput::error(format!("could not start bash: {error}")),
            };
            let stdout = child.stdout.take().expect("piped stdout");
            let stderr = child.stderr.take().expect("piped stderr");
            let stdout = tokio::spawn(capture(stdout));
            let stderr = tokio::spawn(capture(stderr));
            enum Completion {
                Status(io::Result<ExitStatus>),
                Cancelled,
                TimedOut(u64),
            }
            let completion = if let Some(seconds) = input.timeout_seconds {
                let timeout = tokio::time::sleep(Duration::from_secs(seconds));
                tokio::pin!(timeout);
                tokio::select! {
                    biased;
                    () = cancellation.cancelled() => Completion::Cancelled,
                    () = &mut timeout => Completion::TimedOut(seconds),
                    status = child.wait() => Completion::Status(status),
                }
            } else {
                tokio::select! {
                    biased;
                    () = cancellation.cancelled() => Completion::Cancelled,
                    status = child.wait() => Completion::Status(status),
                }
            };
            if !matches!(&completion, Completion::Status(_)) {
                terminate(&mut child).await;
            }
            let stdout = match captured(stdout.await, "stdout") {
                Ok(stdout) => stdout,
                Err(error) => return ToolOutput::error(error),
            };
            let stderr = match captured(stderr.await, "stderr") {
                Ok(stderr) => stderr,
                Err(error) => return ToolOutput::error(error),
            };
            let status = match completion {
                Completion::Status(Ok(status)) => status,
                Completion::Status(Err(error)) => {
                    return ToolOutput::error(format!("could not wait for bash: {error}"));
                }
                Completion::Cancelled => return ToolOutput::error("bash cancelled"),
                Completion::TimedOut(seconds) => {
                    return ToolOutput::error(format!("bash timed out after {seconds} seconds"));
                }
            };
            let full = format_process_output(&stdout, &stderr);
            let truncated = truncate::output(&full, truncate::Keep::Tail);
            ToolOutput {
                content: vec![rho_ai::ContentBlock::text(format!(
                    "exit code: {}\n{}",
                    status
                        .code()
                        .map_or_else(|| status.to_string(), |code| code.to_string()),
                    truncated.text
                ))],
                is_error: !status.success(),
                details: Some(json!({
                    "cwd": cwd,
                    "exit_code": status.code(),
                    "success": status.success(),
                    "stdout_bytes": stdout.total_bytes,
                    "stdout_lines": stdout.total_lines,
                    "stdout_truncated": stdout.total_bytes > stdout.tail.len(),
                    "stderr_bytes": stderr.total_bytes,
                    "stderr_lines": stderr.total_lines,
                    "stderr_truncated": stderr.total_bytes > stderr.tail.len(),
                    "truncation": truncated.details,
                })),
            }
        })
    }
}

const STREAM_BYTES: usize = truncate::MAX_BYTES / 2;

struct Captured {
    tail: VecDeque<u8>,
    total_bytes: usize,
    total_lines: usize,
}

async fn capture(mut reader: impl AsyncRead + Unpin) -> io::Result<Captured> {
    let mut tail = VecDeque::with_capacity(STREAM_BYTES);
    let mut total_bytes = 0_usize;
    let mut newlines = 0_usize;
    let mut last_was_newline = false;
    let mut buffer = [0; 8 * 1024];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        total_bytes = total_bytes.saturating_add(read);
        newlines =
            newlines.saturating_add(buffer[..read].iter().filter(|byte| **byte == b'\n').count());
        last_was_newline = buffer[read - 1] == b'\n';
        tail.extend(&buffer[..read]);
        while tail.len() > STREAM_BYTES {
            tail.pop_front();
        }
    }
    Ok(Captured {
        tail,
        total_bytes,
        total_lines: newlines + usize::from(total_bytes > 0 && !last_was_newline),
    })
}

fn captured(
    result: Result<io::Result<Captured>, tokio::task::JoinError>,
    stream: &str,
) -> Result<Captured, String> {
    match result {
        Ok(Ok(captured)) => Ok(captured),
        Ok(Err(error)) => Err(format!("could not read {stream}: {error}")),
        Err(error) => Err(format!("{stream} capture task failed: {error}")),
    }
}

impl Captured {
    fn render(&self, stream: &str) -> String {
        let bytes = self.tail.iter().copied().collect::<Vec<_>>();
        let text = String::from_utf8_lossy(&bytes);
        if self.total_bytes > self.tail.len() {
            format!(
                "[{stream} truncated: showing the last {} of {} bytes]\n{text}",
                self.tail.len(),
                self.total_bytes
            )
        } else {
            text.into_owned()
        }
    }
}

fn format_process_output(stdout: &Captured, stderr: &Captured) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        stdout.render("stdout"),
        stderr.render("stderr")
    )
}

async fn terminate(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(id) = child.id() {
        let _ = tokio::process::Command::new("kill")
            .args(["-KILL", "--", &format!("-{id}")])
            .status()
            .await;
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}
