use std::{io, path::PathBuf};

use rho_ai::{CancellationToken, ContentBlock, ToolDefinition};
use rho_core::ReplaySafety;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{path, truncate};
use crate::{Tool, ToolFuture, ToolOutput};

const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

/// Reads text files by line range and passes supported images through.
pub struct ReadTool {
    definition: ToolDefinition,
    cwd: PathBuf,
}

impl ReadTool {
    /// Creates a read tool whose relative paths resolve from `cwd`.
    pub fn new(cwd: impl Into<PathBuf>) -> io::Result<Self> {
        Ok(Self::from_absolute(path::absolute(cwd.into())?))
    }

    pub(super) fn from_absolute(cwd: PathBuf) -> Self {
        Self {
            definition: ToolDefinition::new(
                "read",
                concat!(
                    "Read a text file or supported image. Text output keeps the first 2000 lines ",
                    "or 50KB and includes a continuation offset when more content remains."
                ),
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Relative or absolute file path." },
                        "offset": { "type": "integer", "minimum": 1, "description": "One-based first line." },
                        "limit": { "type": "integer", "minimum": 1, "description": "Maximum lines before output truncation." }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
            ),
            cwd,
        }
    }
}

#[derive(Deserialize)]
struct ReadInput {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

impl Tool for ReadTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    fn replay_safety(&self) -> ReplaySafety {
        ReplaySafety::Safe
    }

    fn execute(&self, arguments: Value, cancellation: CancellationToken) -> ToolFuture<'_> {
        let cwd = self.cwd.clone();
        Box::pin(async move {
            let input: ReadInput = match serde_json::from_value(arguments) {
                Ok(input) => input,
                Err(error) => return ToolOutput::error(format!("invalid read arguments: {error}")),
            };
            if cancellation.is_cancelled() {
                return ToolOutput::error("read cancelled");
            }
            let resolved = path::resolve(&cwd, &input.path);
            let metadata = match tokio::fs::metadata(&resolved).await {
                Ok(metadata) => metadata,
                Err(error) => {
                    return ToolOutput::error(format!(
                        "could not inspect {}: {error}",
                        resolved.display()
                    ));
                }
            };
            if !metadata.is_file() {
                return ToolOutput::error(format!("{} is not a regular file", resolved.display()));
            }
            let bytes = match tokio::fs::read(&resolved).await {
                Ok(bytes) => bytes,
                Err(error) => {
                    return ToolOutput::error(format!(
                        "could not read {}: {error}",
                        resolved.display()
                    ));
                }
            };
            if cancellation.is_cancelled() {
                return ToolOutput::error("read cancelled");
            }
            if let Some(mime) = image_mime(&bytes) {
                if bytes.len() > MAX_IMAGE_BYTES {
                    return ToolOutput::error(format!(
                        "image {} is {} bytes; the read limit is {} bytes",
                        resolved.display(),
                        bytes.len(),
                        MAX_IMAGE_BYTES
                    ));
                }
                return ToolOutput {
                    content: vec![
                        ContentBlock::text(format!("Read image file [{}]", resolved.display())),
                        ContentBlock::Image {
                            data: bytes,
                            mime: mime.to_owned(),
                        },
                    ],
                    is_error: false,
                    details: Some(json!({ "path": resolved, "mime": mime })),
                };
            }
            let text = match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(_) => {
                    return ToolOutput::error(format!(
                        "{} is not UTF-8 text or a supported image",
                        resolved.display()
                    ));
                }
            };
            let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
            let lines = text.split_inclusive('\n').collect::<Vec<_>>();
            let total_lines = lines.len();
            let start = input.offset.unwrap_or(1).saturating_sub(1);
            if start >= total_lines && !(start == 0 && text.is_empty()) {
                return ToolOutput::error(format!(
                    "offset {} is beyond end of file ({total_lines} lines)",
                    input.offset.unwrap_or(1)
                ));
            }
            let end = input.limit.map_or(total_lines, |limit| {
                start.saturating_add(limit).min(total_lines)
            });
            let selected = if text.is_empty() {
                String::new()
            } else {
                lines[start..end].concat()
            };
            let mut truncated = truncate::output(&selected, truncate::Keep::Head);
            let automatic_more = truncated.details.is_some();
            if !automatic_more && end < total_lines {
                truncated.text.push_str(&format!(
                    "\n[{} more lines; use offset={} to continue]",
                    total_lines - end,
                    end + 1
                ));
            } else if truncated.partial_line {
                truncated.text.push_str(&format!(
                    "\n[line {} exceeds the byte limit; use bash to inspect a byte range]",
                    start + 1
                ));
            } else if automatic_more {
                let next = start
                    .saturating_add(truncated.shown_lines)
                    .saturating_add(1);
                truncated
                    .text
                    .push_str(&format!("\n[use offset={next} to continue]"));
            }
            ToolOutput {
                content: vec![ContentBlock::text(truncated.text)],
                is_error: false,
                details: Some(json!({
                    "path": resolved,
                    "start_line": start + 1,
                    "selected_end_line": end,
                    "total_lines": total_lines,
                    "truncation": truncated.details,
                })),
            }
        })
    }
}

fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(b"BM") {
        Some("image/bmp")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}
