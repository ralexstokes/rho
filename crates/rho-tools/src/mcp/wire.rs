use base64::Engine as _;
use rho_ai::ContentBlock;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::ToolOutput;

use super::McpError;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ListToolsResult {
    #[serde(default)]
    pub(super) result_type: Option<String>,
    pub(super) tools: Vec<RemoteTool>,
    #[serde(default)]
    pub(super) next_cursor: Option<String>,
}

impl ListToolsResult {
    pub(super) fn require_complete(&self) -> Result<(), McpError> {
        match self.result_type.as_deref() {
            None | Some("complete") => Ok(()),
            Some(other) => Err(McpError::Protocol {
                message: format!("tools/list returned unsupported resultType {other:?}"),
            }),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RemoteTool {
    pub(super) name: String,
    #[serde(default)]
    pub(super) title: Option<String>,
    #[serde(default)]
    pub(super) description: Option<String>,
    pub(super) input_schema: Value,
    #[serde(default)]
    execution: Option<Execution>,
}

impl RemoteTool {
    pub(super) fn requires_task(&self) -> bool {
        self.execution
            .as_ref()
            .and_then(|execution| execution.task_support.as_deref())
            == Some("required")
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Execution {
    #[serde(default)]
    task_support: Option<String>,
}

pub(super) fn tool_output(result: Value) -> ToolOutput {
    match parse_tool_output(&result) {
        Ok(mut output) => {
            output.details = Some(json!({
                "mcp": {
                    "structured_content": result.get("structuredContent"),
                    "meta": result.get("_meta"),
                }
            }));
            output
        }
        Err(error) => ToolOutput::error(format!("invalid MCP tool result: {error}")),
    }
}

fn parse_tool_output(result: &Value) -> Result<ToolOutput, McpError> {
    match result.get("resultType").and_then(Value::as_str) {
        None | Some("complete") => {}
        Some("input_required") => {
            return Ok(ToolOutput::error(
                "MCP tool requires additional client input, but rho's simple MCP client does not advertise elicitation",
            ));
        }
        Some(other) => {
            return Err(McpError::Protocol {
                message: format!("unsupported resultType {other:?}"),
            });
        }
    }
    let content = result
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| McpError::Protocol {
            message: "tools/call result omitted content array".to_owned(),
        })?;
    let mut blocks = Vec::with_capacity(content.len().max(1));
    for item in content {
        blocks.push(content_block(item)?);
    }
    if blocks.is_empty()
        && let Some(structured) = result.get("structuredContent")
    {
        blocks.push(ContentBlock::text(
            serde_json::to_string(structured).map_err(|error| McpError::Protocol {
                message: format!("could not render structuredContent: {error}"),
            })?,
        ));
    }
    Ok(ToolOutput {
        content: blocks,
        is_error: result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        details: None,
    })
}

fn content_block(item: &Value) -> Result<ContentBlock, McpError> {
    let kind = item
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| McpError::Protocol {
            message: "tool content block omitted string type".to_owned(),
        })?;
    match kind {
        "text" => Ok(ContentBlock::text(required_string(item, "text")?)),
        "image" => {
            let data = base64::engine::general_purpose::STANDARD
                .decode(required_string(item, "data")?)
                .map_err(|error| McpError::Protocol {
                    message: format!("image content contains invalid base64: {error}"),
                })?;
            Ok(ContentBlock::Image {
                data,
                mime: required_string(item, "mimeType")?,
            })
        }
        "resource_link" => Ok(ContentBlock::text(format_resource_link(item))),
        "resource" => Ok(ContentBlock::text(format_embedded_resource(item)?)),
        "audio" => Ok(ContentBlock::text(format!(
            "[MCP audio content omitted; MIME type: {}]",
            item.get("mimeType")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        ))),
        other => Ok(ContentBlock::text(format!(
            "[Unsupported MCP content type {other:?}: {}]",
            serde_json::to_string(item).unwrap_or_else(|_| "<unrenderable>".to_owned())
        ))),
    }
}

fn required_string(item: &Value, key: &str) -> Result<String, McpError> {
    item.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| McpError::Protocol {
            message: format!("content block omitted string field {key:?}"),
        })
}

fn format_resource_link(item: &Value) -> String {
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("resource");
    let uri = item
        .get("uri")
        .and_then(Value::as_str)
        .unwrap_or("unknown URI");
    let description = item
        .get("description")
        .and_then(Value::as_str)
        .map(|description| format!(" — {description}"))
        .unwrap_or_default();
    format!("[MCP resource link: {name} ({uri}){description}]")
}

fn format_embedded_resource(item: &Value) -> Result<String, McpError> {
    let resource = item.get("resource").ok_or_else(|| McpError::Protocol {
        message: "embedded resource block omitted resource".to_owned(),
    })?;
    let uri = resource
        .get("uri")
        .and_then(Value::as_str)
        .unwrap_or("unknown URI");
    if let Some(text) = resource.get("text").and_then(Value::as_str) {
        return Ok(format!("MCP resource {uri}:\n{text}"));
    }
    if resource.get("blob").is_some() {
        return Ok(format!(
            "[MCP binary resource omitted: {uri}; MIME type: {}]",
            resource
                .get("mimeType")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        ));
    }
    Err(McpError::Protocol {
        message: "embedded resource has neither text nor blob".to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use rho_ai::ContentBlock;
    use serde_json::json;

    use super::*;

    #[test]
    fn tool_results_preserve_text_images_resources_and_structured_details() {
        let output = tool_output(json!({
            "resultType": "complete",
            "content": [
                {"type": "text", "text": "hello"},
                {"type": "image", "data": "iVBORw==", "mimeType": "image/png"},
                {"type": "resource", "resource": {"uri": "file:///a", "text": "body"}},
                {"type": "audio", "data": "AA==", "mimeType": "audio/wav"}
            ],
            "structuredContent": {"answer": 42},
            "isError": false
        }));
        assert!(!output.is_error);
        assert_eq!(output.content[0], ContentBlock::text("hello"));
        assert!(matches!(output.content[1], ContentBlock::Image { .. }));
        assert_eq!(
            output.content[2],
            ContentBlock::text("MCP resource file:///a:\nbody")
        );
        assert_eq!(
            output
                .details
                .unwrap()
                .pointer("/mcp/structured_content/answer"),
            Some(&json!(42))
        );
    }

    #[test]
    fn input_required_is_a_visible_unsupported_result() {
        let output = tool_output(json!({
            "resultType": "input_required",
            "content": []
        }));
        assert!(output.is_error);
    }
}
