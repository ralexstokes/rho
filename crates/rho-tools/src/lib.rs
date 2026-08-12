//! Tool boundary traits, replay metadata, executor registry, and coding tools.

use std::{future::Future, pin::Pin};

use rho_ai::{CancellationToken, ContentBlock, ToolDefinition};
use rho_core::ReplaySafety;
use serde_json::Value;

mod builtins;
mod mcp;

pub use builtins::{BashTool, EditTool, ReadTool, WriteTool, coding_tools};
pub use mcp::{McpConnection, McpError, McpStdioConfig, McpTool};

/// Type-erased future returned by tool implementations.
pub type ToolFuture<'tool> = Pin<Box<dyn Future<Output = ToolOutput> + Send + 'tool>>;

/// Complete result of one tool execution.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolOutput {
    /// Model-visible result content.
    pub content: Vec<ContentBlock>,
    /// Whether the tool failed.
    pub is_error: bool,
    /// Optional structured diagnostics retained in the session.
    pub details: Option<Value>,
}

impl ToolOutput {
    /// Creates a successful text result.
    #[must_use]
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::text(content)],
            is_error: false,
            details: None,
        }
    }

    /// Creates a failed text result.
    #[must_use]
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::text(content)],
            is_error: true,
            details: None,
        }
    }
}

/// One asynchronous tool implementation.
pub trait Tool: Send + Sync {
    /// Provider-visible declaration.
    fn definition(&self) -> &ToolDefinition;

    /// Crash replay policy recorded before every invocation.
    fn replay_safety(&self) -> ReplaySafety;

    /// Executes already parsed and schema-validated arguments.
    fn execute(&self, arguments: Value, cancellation: CancellationToken) -> ToolFuture<'_>;
}

/// Name-indexed set of tool implementations.
#[derive(Default)]
pub struct ToolSet {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolSet {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a tool, rejecting duplicate provider-visible names.
    pub fn register(&mut self, tool: impl Tool + 'static) -> Result<(), DuplicateTool> {
        let name = tool.definition().name.clone();
        if self
            .tools
            .iter()
            .any(|existing| existing.definition().name == name)
        {
            return Err(DuplicateTool { name });
        }
        self.tools.push(Box::new(tool));
        Ok(())
    }

    /// Finds a tool by its provider-visible name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|tool| tool.definition().name == name)
            .map(AsRef::as_ref)
    }

    /// Returns deterministic machine metadata in registration order.
    #[must_use]
    pub fn specs(&self) -> Vec<rho_core::ToolSpec> {
        self.tools
            .iter()
            .map(|tool| rho_core::ToolSpec {
                definition: tool.definition().clone(),
                replay: tool.replay_safety(),
            })
            .collect()
    }
}

/// Duplicate provider-visible tool name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DuplicateTool {
    /// Duplicated name.
    pub name: String,
}

impl std::fmt::Display for DuplicateTool {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "tool {:?} is already registered", self.name)
    }
}

impl std::error::Error for DuplicateTool {}

#[cfg(test)]
mod tests {
    use rho_ai::ToolDefinition;
    use serde_json::json;

    use super::*;

    struct FauxTool {
        definition: ToolDefinition,
    }

    impl Tool for FauxTool {
        fn definition(&self) -> &ToolDefinition {
            &self.definition
        }

        fn replay_safety(&self) -> ReplaySafety {
            ReplaySafety::Safe
        }

        fn execute(&self, _: Value, _: CancellationToken) -> ToolFuture<'_> {
            Box::pin(async { ToolOutput::text("ok") })
        }
    }

    fn faux() -> FauxTool {
        FauxTool {
            definition: ToolDefinition::new("faux", "test", json!({"type": "object"})),
        }
    }

    #[test]
    fn registry_exports_replay_metadata_and_rejects_duplicates() {
        let mut tools = ToolSet::new();
        tools.register(faux()).unwrap();
        assert_eq!(tools.specs()[0].replay, ReplaySafety::Safe);
        assert_eq!(
            tools.register(faux()).unwrap_err(),
            DuplicateTool {
                name: "faux".to_owned()
            }
        );
    }
}
