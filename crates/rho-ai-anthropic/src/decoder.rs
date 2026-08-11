//! Incremental, I/O-free server-sent-event decoder.

use serde_json::Value;

/// One decoded SSE event with typed JSON data ready for response assembly.
#[derive(Clone, Debug, PartialEq)]
pub struct DecodedEvent {
    /// Named SSE event.
    pub event: String,
    /// Parsed JSON payload.
    pub data: Value,
}

/// Incremental SSE decoding failure.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    /// A complete SSE frame was not UTF-8.
    #[error("Anthropic SSE frame was not UTF-8: {0}")]
    Utf8(#[from] std::str::Utf8Error),
    /// Event carried no JSON data.
    #[error("Anthropic SSE event {event:?} carried no data")]
    MissingData {
        /// Named event, if supplied.
        event: Option<String>,
    },
    /// JSON payload was malformed.
    #[error("Anthropic SSE event {event:?} carried invalid JSON: {source}")]
    Json {
        /// Named event.
        event: String,
        /// JSON parsing failure.
        #[source]
        source: serde_json::Error,
    },
    /// Named SSE event disagreed with the payload's `type` field.
    #[error("Anthropic SSE event name {event:?} disagreed with payload type {payload_type:?}")]
    TypeMismatch {
        /// Named event.
        event: String,
        /// Payload-declared type.
        payload_type: String,
    },
}

/// Incremental byte-to-event decoder. It never reads a socket or a file.
#[derive(Debug, Default)]
pub struct Decoder {
    buffer: Vec<u8>,
}

impl Decoder {
    /// Creates an empty decoder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds bytes and returns every complete event now available.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<DecodedEvent>, DecodeError> {
        self.buffer.extend_from_slice(bytes);
        self.drain(false)
    }

    /// Finishes the stream, decoding a final frame even without a trailing blank line.
    pub fn finish(mut self) -> Result<Vec<DecodedEvent>, DecodeError> {
        self.drain(true)
    }

    fn drain(&mut self, finish: bool) -> Result<Vec<DecodedEvent>, DecodeError> {
        let mut events = Vec::new();
        while let Some((frame_end, separator_len)) = find_frame(&self.buffer) {
            let frame = self.buffer[..frame_end].to_vec();
            self.buffer.drain(..frame_end + separator_len);
            if let Some(event) = decode_frame(&frame)? {
                events.push(event);
            }
        }
        if finish && self.buffer.iter().any(|byte| !byte.is_ascii_whitespace()) {
            let frame = std::mem::take(&mut self.buffer);
            if let Some(event) = decode_frame(&frame)? {
                events.push(event);
            }
        }
        Ok(events)
    }
}

fn find_frame(bytes: &[u8]) -> Option<(usize, usize)> {
    let mut index = 0;
    while index < bytes.len() {
        let Some(first_len) = line_ending_len(bytes, index) else {
            index += 1;
            continue;
        };
        let second = index + first_len;
        if let Some(second_len) = line_ending_len(bytes, second) {
            return Some((index, first_len + second_len));
        }
        index = second;
    }
    None
}

fn line_ending_len(bytes: &[u8], index: usize) -> Option<usize> {
    match bytes.get(index) {
        Some(b'\r') if bytes.get(index + 1) == Some(&b'\n') => Some(2),
        Some(b'\r' | b'\n') => Some(1),
        _ => None,
    }
}

fn decode_frame(bytes: &[u8]) -> Result<Option<DecodedEvent>, DecodeError> {
    let text = std::str::from_utf8(bytes)?;
    let mut event = None;
    let mut data = Vec::new();
    for line in text.split(['\r', '\n']) {
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => event = Some(value.to_owned()),
            "data" => data.push(value),
            _ => {}
        }
    }
    if event.is_none() && data.is_empty() {
        return Ok(None);
    }
    if data.is_empty() {
        return Err(DecodeError::MissingData { event });
    }
    let joined = data.join("\n");
    let event_name = event.unwrap_or_else(|| "message".to_owned());
    let value: Value = serde_json::from_str(&joined).map_err(|source| DecodeError::Json {
        event: event_name.clone(),
        source,
    })?;
    if let Some(payload_type) = value.get("type").and_then(Value::as_str)
        && event_name != "message"
        && event_name != payload_type
    {
        return Err(DecodeError::TypeMismatch {
            event: event_name,
            payload_type: payload_type.to_owned(),
        });
    }
    let event = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or(&event_name)
        .to_owned();
    Ok(Some(DecodedEvent { event, data: value }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_mid_token_and_mid_utf8_splits() {
        let frame = concat!(
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,",
            "\"delta\":{\"type\":\"text_delta\",\"text\":\"héllo\"}}\n\n"
        )
        .as_bytes();
        let split = frame.iter().position(|byte| *byte == 0xc3).unwrap() + 1;
        let mut decoder = Decoder::new();
        assert!(decoder.feed(&frame[..split]).unwrap().is_empty());
        let events = decoder.feed(&frame[split..]).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data["delta"]["text"], "héllo");
    }

    #[test]
    fn rejects_malformed_json() {
        let mut decoder = Decoder::new();
        let error = decoder
            .feed(b"event: message_start\ndata: {not-json}\n\n")
            .unwrap_err();
        assert!(matches!(error, DecodeError::Json { .. }));
    }

    #[test]
    fn rejects_event_type_disagreement() {
        let mut decoder = Decoder::new();
        let error = decoder
            .feed(b"event: message_stop\ndata: {\"type\":\"ping\"}\n\n")
            .unwrap_err();
        assert!(matches!(error, DecodeError::TypeMismatch { .. }));
    }

    #[test]
    fn ignores_comments_and_unknown_fields() {
        let mut decoder = Decoder::new();
        let events = decoder
            .feed(b": keepalive\nid: 3\nevent: ping\ndata: {\"type\":\"ping\"}\n\n")
            .unwrap();
        assert_eq!(events[0].event, "ping");
    }

    #[test]
    fn mixed_line_endings_preserve_event_order() {
        let mut decoder = Decoder::new();
        let events = decoder
            .feed(
                b"event: ping\ndata: {\"type\":\"ping\"}\n\nevent: ping\r\ndata: {\"type\":\"ping\"}\r\n\r\n",
            )
            .unwrap();
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn decodes_all_sse_line_endings_across_chunk_boundaries() {
        let mut decoder = Decoder::new();
        assert!(
            decoder
                .feed(b"event: ping\rdata: {\"type\":\"ping\"}\r")
                .unwrap()
                .is_empty()
        );
        let events = decoder
            .feed(
                b"\revent: ping\r\ndata: {\"type\":\"ping\"}\r\n\nevent: ping\ndata: {\"type\":\"ping\"}\n\r\n",
            )
            .unwrap();
        assert_eq!(events.len(), 3);
        assert!(events.iter().all(|event| event.event == "ping"));
    }
}
