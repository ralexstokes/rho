use serde::{Deserialize, Serialize};

use crate::{
    message::Message,
    tool::{ToolCall, ToolResult},
};

pub const PROTOCOL_VERSION: u16 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClientEnvelope {
    pub version: u16,
    pub event: ClientEvent,
}

impl ClientEnvelope {
    pub fn new(event: ClientEvent) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            event,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum ClientEvent {
    StartSession(StartSession),
    UserMessage(UserMessage),
    Cancel(CancelRequest),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StartSession {
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserMessage {
    pub session_id: String,
    pub message: Message,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CancelRequest {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServerEnvelope {
    pub version: u16,
    pub event: ServerEvent,
}

impl ServerEnvelope {
    pub fn new(event: ServerEvent) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            event,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum ServerEvent {
    SessionAck(SessionAck),
    AssistantDelta(AssistantDelta),
    ToolStarted(ToolStarted),
    ToolCompleted(ToolCompleted),
    Final(FinalMessage),
    Error(ErrorEvent),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionAck {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AssistantDelta {
    pub session_id: String,
    pub delta: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolStarted {
    pub session_id: String,
    pub call: ToolCall,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolCompleted {
    pub session_id: String,
    pub result: ToolResult,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FinalMessage {
    pub session_id: String,
    pub message: Message,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorEvent {
    pub session_id: Option<String>,
    pub code: String,
    pub message: String,
}
