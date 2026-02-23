use serde::{Deserialize, Serialize};

use crate::{
    message::Message,
    tool::{ToolCall, ToolResult},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderEvent {
    AssistantDelta { delta: String },
    ToolCall { call: ToolCall },
    ToolResult { result: ToolResult },
    Message { message: Message },
    Finished,
}
