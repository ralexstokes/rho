use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The only RPC protocol version accepted and emitted by this build.
pub const VERSION: u32 = 1;

/// Request identity chosen by the side initiating the request.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(untagged)]
pub enum RpcId {
    /// Non-negative numeric identity.
    Number(u64),
    /// String identity.
    String(String),
}

impl From<u64> for RpcId {
    fn from(value: u64) -> Self {
        Self::Number(value)
    }
}

impl From<String> for RpcId {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<&str> for RpcId {
    fn from(value: &str) -> Self {
        Self::String(value.to_owned())
    }
}

impl fmt::Display for RpcId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(value) => value.fmt(formatter),
            Self::String(value) => formatter.write_str(value),
        }
    }
}

/// Stable machine-readable RPC failure.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorObject {
    /// Stable error class, such as `invalid_params` or `busy`.
    pub code: String,
    /// Human-readable diagnostic.
    pub message: String,
    /// Optional structured context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ErrorObject {
    /// Creates an error without structured context.
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            data: None,
        }
    }

    /// Adds structured diagnostic context.
    #[must_use]
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

/// Client-to-server method request.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientRequest {
    /// Protocol version.
    pub v: u32,
    /// Client-selected correlation identity.
    pub id: RpcId,
    /// Dotted method name.
    pub method: String,
    /// Method-specific object, or null when omitted.
    #[serde(default)]
    pub params: Value,
}

/// Validated payload of a client response to a server-initiated request.
#[derive(Clone, Debug, PartialEq)]
pub enum ResponsePayload {
    /// The client answered successfully.
    Success(Value),
    /// The client rejected or failed the request.
    Failure(ErrorObject),
}

/// Client response to a server-initiated request, primarily an interaction.
#[derive(Clone, Debug, PartialEq)]
pub struct ClientResponse {
    /// Protocol version.
    pub v: u32,
    /// Identity copied from [`ServerRequest::id`].
    pub id: RpcId,
    /// Exactly one success or failure payload.
    pub payload: ResponsePayload,
}

/// One validated inbound client message.
#[derive(Clone, Debug, PartialEq)]
pub enum ClientMessage {
    /// Method request.
    Request(ClientRequest),
    /// Answer to a server request.
    Response(ClientResponse),
}

/// Server response to one client method request.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerResponse {
    /// Protocol version.
    pub v: u32,
    /// Identity copied from the request.
    pub id: RpcId,
    /// Success discriminator.
    pub ok: bool,
    /// Successful result, present exactly when `ok` is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Failure, present exactly when `ok` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorObject>,
}

impl ServerResponse {
    /// Creates a successful response, including an explicit JSON null result.
    #[must_use]
    pub fn success(id: RpcId, result: Value) -> Self {
        Self {
            v: VERSION,
            id,
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    /// Creates a failed response.
    #[must_use]
    pub fn failure(id: RpcId, error: ErrorObject) -> Self {
        Self {
            v: VERSION,
            id,
            ok: false,
            result: None,
            error: Some(error),
        }
    }
}

/// Unsolicited advisory server event. Snapshot events carry complete state.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerEvent {
    /// Protocol version.
    pub v: u32,
    /// Dotted event name.
    pub event: String,
    /// Event-specific data.
    pub data: Value,
}

/// Server-to-client request that requires a [`ClientResponse`].
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerRequest {
    /// Protocol version.
    pub v: u32,
    /// Server-selected correlation identity.
    pub id: RpcId,
    /// Dotted method name.
    pub method: String,
    /// Request-specific data.
    pub params: Value,
}

/// Any outbound server frame.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ServerMessage {
    /// Response to a client request.
    Response(ServerResponse),
    /// Advisory event.
    Event(ServerEvent),
    /// Request requiring a client answer.
    Request(ServerRequest),
}
