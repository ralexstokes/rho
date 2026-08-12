use std::{io, path::PathBuf};

use rho_ai::{CancellationToken, ToolDefinition};
use rho_core::ReplaySafety;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{mutation, path};
use crate::{Tool, ToolFuture, ToolOutput};

/// Creates or completely overwrites files, including missing parent directories.
pub struct WriteTool {
    definition: ToolDefinition,
    cwd: PathBuf,
}

impl WriteTool {
    /// Creates a write tool whose relative paths resolve from `cwd`.
    pub fn new(cwd: impl Into<PathBuf>) -> io::Result<Self> {
        Ok(Self::from_absolute(path::absolute(cwd.into())?))
    }

    pub(super) fn from_absolute(cwd: PathBuf) -> Self {
        Self {
            definition: ToolDefinition::new(
                "write",
                concat!(
                    "Create or completely overwrite a file and create missing parent directories. ",
                    "Use edit for targeted changes to an existing file."
                ),
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Relative or absolute file path." },
                        "content": { "type": "string", "description": "Complete new file contents." }
                    },
                    "required": ["path", "content"],
                    "additionalProperties": false
                }),
            ),
            cwd,
        }
    }
}

#[derive(Deserialize)]
struct WriteInput {
    path: String,
    content: String,
}

impl Tool for WriteTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    fn replay_safety(&self) -> ReplaySafety {
        ReplaySafety::Never
    }

    fn execute(&self, arguments: Value, cancellation: CancellationToken) -> ToolFuture<'_> {
        let cwd = self.cwd.clone();
        Box::pin(async move {
            let input: WriteInput = match serde_json::from_value(arguments) {
                Ok(input) => input,
                Err(error) => {
                    return ToolOutput::error(format!("invalid write arguments: {error}"));
                }
            };
            let resolved = path::resolve(&cwd, &input.path);
            let _guard = mutation::lock(&resolved).await;
            if cancellation.is_cancelled() {
                return ToolOutput::error("write cancelled");
            }
            let Some(parent) = resolved.parent() else {
                return ToolOutput::error(format!(
                    "{} has no parent directory",
                    resolved.display()
                ));
            };
            if let Err(error) = tokio::fs::create_dir_all(parent).await {
                return ToolOutput::error(format!(
                    "could not create parent directory for {}: {error}",
                    resolved.display()
                ));
            }
            if cancellation.is_cancelled() {
                return ToolOutput::error("write cancelled");
            }
            if let Err(error) = tokio::fs::write(&resolved, input.content.as_bytes()).await {
                return ToolOutput::error(format!(
                    "could not write {}: {error}",
                    resolved.display()
                ));
            }
            if cancellation.is_cancelled() {
                return ToolOutput::error("write completed after cancellation was requested");
            }
            ToolOutput {
                content: vec![rho_ai::ContentBlock::text(format!(
                    "Wrote {} bytes to {}",
                    input.content.len(),
                    resolved.display()
                ))],
                is_error: false,
                details: Some(json!({ "path": resolved, "bytes": input.content.len() })),
            }
        })
    }
}
