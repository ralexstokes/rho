use serde_json::Value;
use thiserror::Error;

use crate::{
    ClientMessage, ClientRequest, ClientResponse, ErrorObject, ResponsePayload, RpcId,
    ServerMessage,
};

/// Invalid RPC JSON Lines frame.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum CodecError {
    /// A frame was empty or contained physical line breaks.
    #[error("RPC frame must contain exactly one non-empty JSON line")]
    InvalidFrame,
    /// JSON parsing failed.
    #[error("invalid RPC JSON: {0}")]
    Json(String),
    /// The top-level shape is not a client request or response.
    #[error("invalid RPC message: {0}")]
    Shape(String),
}

/// Serializes one server message and appends its JSON Lines delimiter.
pub fn encode_server_line(message: &ServerMessage) -> Result<Vec<u8>, CodecError> {
    let mut encoded =
        serde_json::to_vec(message).map_err(|error| CodecError::Json(error.to_string()))?;
    encoded.push(b'\n');
    Ok(encoded)
}

/// Decodes one complete client JSON line, accepting either LF or CRLF.
pub fn decode_client_line(line: &[u8]) -> Result<ClientMessage, CodecError> {
    let line = strip_delimiter(line)?;
    let value: Value =
        serde_json::from_slice(line).map_err(|error| CodecError::Json(error.to_string()))?;
    let object = value
        .as_object()
        .ok_or_else(|| CodecError::Shape("top-level message must be a JSON object".to_owned()))?;
    if object.contains_key("method") {
        if object.contains_key("ok") {
            return Err(CodecError::Shape(
                "request must not contain response field ok".to_owned(),
            ));
        }
        return serde_json::from_value::<ClientRequest>(value)
            .map(ClientMessage::Request)
            .map_err(|error| CodecError::Shape(error.to_string()));
    }
    if !object.contains_key("ok") {
        return Err(CodecError::Shape(
            "message must contain either method or ok".to_owned(),
        ));
    }
    decode_response(value).map(ClientMessage::Response)
}

fn strip_delimiter(mut line: &[u8]) -> Result<&[u8], CodecError> {
    if line.last() == Some(&b'\n') {
        line = &line[..line.len() - 1];
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
    }
    if line.is_empty() || line.contains(&b'\n') || line.contains(&b'\r') {
        return Err(CodecError::InvalidFrame);
    }
    Ok(line)
}

fn decode_response(value: Value) -> Result<ClientResponse, CodecError> {
    let object = value
        .as_object()
        .expect("response dispatch requires an object");
    if object
        .keys()
        .any(|key| !matches!(key.as_str(), "v" | "id" | "ok" | "result" | "error"))
    {
        return Err(CodecError::Shape(
            "response contains an unknown field".to_owned(),
        ));
    }
    let v = object
        .get("v")
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| CodecError::Shape("response v must be a u32".to_owned()))?;
    let id: RpcId = serde_json::from_value(
        object
            .get("id")
            .cloned()
            .ok_or_else(|| CodecError::Shape("response omitted id".to_owned()))?,
    )
    .map_err(|error| CodecError::Shape(format!("invalid response id: {error}")))?;
    let ok = object
        .get("ok")
        .and_then(Value::as_bool)
        .ok_or_else(|| CodecError::Shape("response ok must be boolean".to_owned()))?;
    let payload = match (ok, object.get("result"), object.get("error")) {
        (true, Some(result), None) => ResponsePayload::Success(result.clone()),
        (false, None, Some(error)) => ResponsePayload::Failure(
            serde_json::from_value::<ErrorObject>(error.clone())
                .map_err(|error| CodecError::Shape(format!("invalid error object: {error}")))?,
        ),
        _ => {
            return Err(CodecError::Shape(
                "response must contain exactly one result or error matching ok".to_owned(),
            ));
        }
    };
    Ok(ClientResponse { v, id, payload })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{ServerEvent, ServerRequest, ServerResponse, VERSION};

    #[test]
    fn client_requests_and_responses_decode_strictly() {
        let request = decode_client_line(
            b"{\"v\":1,\"id\":\"a\",\"method\":\"session.prompt\",\"params\":{\"text\":\"a\\nb\"}}\r\n",
        )
        .unwrap();
        assert!(matches!(
            request,
            ClientMessage::Request(ClientRequest { v: VERSION, .. })
        ));

        let response =
            decode_client_line(b"{\"v\":1,\"id\":7,\"ok\":true,\"result\":null}").unwrap();
        assert_eq!(
            response,
            ClientMessage::Response(ClientResponse {
                v: VERSION,
                id: RpcId::Number(7),
                payload: ResponsePayload::Success(Value::Null),
            })
        );
        assert!(decode_client_line(b"{\"v\":1,\"id\":7,\"ok\":true,\"error\":{}}").is_err());
        assert!(decode_client_line(b"{}\n{}\n").is_err());
    }

    #[test]
    fn every_server_message_has_one_compact_line() {
        let messages = [
            ServerMessage::Response(ServerResponse::success(
                RpcId::from("response"),
                json!({"text": "a\nb"}),
            )),
            ServerMessage::Response(ServerResponse::failure(
                RpcId::from(2_u64),
                ErrorObject::new("bad", "no"),
            )),
            ServerMessage::Event(ServerEvent {
                v: VERSION,
                event: "agent.delta".to_owned(),
                data: json!({"delta": "x"}),
            }),
            ServerMessage::Request(ServerRequest {
                v: VERSION,
                id: RpcId::from("interaction"),
                method: "interaction.answer".to_owned(),
                params: json!({"prompt": "continue?"}),
            }),
        ];
        for message in messages {
            let line = encode_server_line(&message).unwrap();
            assert_eq!(line.iter().filter(|byte| **byte == b'\n').count(), 1);
            assert_eq!(line.last(), Some(&b'\n'));
            serde_json::from_slice::<Value>(&line).unwrap();
        }
    }
}
