pub mod message;
pub mod protocol;
pub mod providers;
pub mod stream;
pub mod tool;

pub use message::{Message, MessageRole};
pub use protocol::{ClientEnvelope, ClientEvent, PROTOCOL_VERSION, ServerEnvelope, ServerEvent};
pub use stream::ProviderEvent;
pub use tool::{ToolCall, ToolDefinition, ToolResult};
