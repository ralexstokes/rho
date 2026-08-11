//! Pure JSONL session encoding, decoding, and torn-tail recovery rules.
//!
//! Filesystem access belongs in `rho-store`; this crate only transforms bytes
//! and typed session data.

use rho_core::{FORMAT_VERSION, Fact, Item, Record, SessionHeader};
use serde_json::{Map, Value};
use thiserror::Error;

/// A fully parsed session plus the byte boundary safe to retain on disk.
#[derive(Clone, Debug, PartialEq)]
pub struct DecodedSession {
    /// Required first-line header.
    pub header: SessionHeader,
    /// Interleaved session items.
    pub items: Vec<Item>,
    /// Length through the final complete LF-terminated line.
    pub valid_up_to: usize,
    /// Whether an incomplete tail was ignored.
    pub had_torn_tail: bool,
}

/// Strict session-format rejection.
#[derive(Debug, Error)]
pub enum CodecError {
    /// The file had no complete header line.
    #[error("session file is missing a complete header line")]
    MissingHeader,
    /// A complete line was not a JSON object.
    #[error("line {line} is not a JSON object")]
    ExpectedObject {
        /// One-based line number.
        line: usize,
    },
    /// A complete line lacked a known discriminator.
    #[error("line {line} has unknown or missing type {kind:?}")]
    UnknownLineType {
        /// One-based line number.
        line: usize,
        /// Observed discriminator.
        kind: Option<String>,
    },
    /// A line had invalid JSON or did not match its typed schema.
    #[error("line {line} is invalid: {source}")]
    InvalidLine {
        /// One-based line number.
        line: usize,
        /// Serde rejection.
        #[source]
        source: serde_json::Error,
    },
    /// The first complete line was not a session header.
    #[error("line 1 must be a session header, found {found:?}")]
    HeaderNotFirst {
        /// Observed discriminator.
        found: Option<String>,
    },
    /// A later line attempted to introduce another header.
    #[error("session header may only appear on line 1 (found another on line {line})")]
    DuplicateHeader {
        /// One-based line number.
        line: usize,
    },
    /// The format version is not understood by this build.
    #[error("unsupported session format version {actual}; expected {expected}")]
    UnsupportedVersion {
        /// Supported version.
        expected: u32,
        /// Observed version.
        actual: u32,
    },
    /// Encoding a typed value failed.
    #[error("could not encode session value: {0}")]
    Encode(#[source] serde_json::Error),
    /// The encoder was called with a value that did not serialize as an object.
    #[error("session wire values must serialize as JSON objects")]
    EncodedValueNotObject,
}

/// Encodes the required first line, including its trailing LF.
pub fn encode_header(header: &SessionHeader) -> Result<Vec<u8>, CodecError> {
    encode_typed("session", header)
}

/// Encodes one interleaved item, including its trailing LF.
pub fn encode_item(item: &Item) -> Result<Vec<u8>, CodecError> {
    match item {
        Item::Entry(entry) => encode_typed("entry", entry),
        Item::Record(record) => encode_typed("record", record),
        Item::Fact(fact) => encode_typed("fact", fact),
    }
}

/// Parses all complete lines and ignores only an unterminated final line.
pub fn decode_session(bytes: &[u8]) -> Result<DecodedSession, CodecError> {
    let valid_up_to = complete_prefix_len(bytes);
    if valid_up_to == 0 {
        return Err(CodecError::MissingHeader);
    }
    let had_torn_tail = valid_up_to != bytes.len();
    let complete_without_final_lf = &bytes[..valid_up_to - 1];
    let mut lines = complete_without_final_lf
        .split(|byte| *byte == b'\n')
        .enumerate();
    let Some((_, first)) = lines.next() else {
        return Err(CodecError::MissingHeader);
    };
    let header = decode_header(first)?;
    if header.v != FORMAT_VERSION {
        return Err(CodecError::UnsupportedVersion {
            expected: FORMAT_VERSION,
            actual: header.v,
        });
    }

    let mut items = Vec::new();
    for (index, line) in lines {
        items.push(decode_item_line(line, index + 1)?);
    }
    Ok(DecodedSession {
        header,
        items,
        valid_up_to,
        had_torn_tail,
    })
}

/// Returns the byte length through the last complete LF-terminated line.
#[must_use]
pub fn complete_prefix_len(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1)
}

fn encode_typed<T: serde::Serialize>(kind: &str, value: &T) -> Result<Vec<u8>, CodecError> {
    let Value::Object(fields) = serde_json::to_value(value).map_err(CodecError::Encode)? else {
        return Err(CodecError::EncodedValueNotObject);
    };
    let mut object = Map::with_capacity(fields.len() + 1);
    object.insert("t".to_owned(), Value::String(kind.to_owned()));
    object.extend(fields);
    let mut encoded = serde_json::to_vec(&object).map_err(CodecError::Encode)?;
    encoded.push(b'\n');
    Ok(encoded)
}

fn decode_header(line: &[u8]) -> Result<SessionHeader, CodecError> {
    let mut object = parse_object(line, 1)?;
    let kind = take_kind(&mut object);
    if kind.as_deref() != Some("session") {
        return Err(CodecError::HeaderNotFirst { found: kind });
    }
    serde_json::from_value(Value::Object(object))
        .map_err(|source| CodecError::InvalidLine { line: 1, source })
}

fn decode_item_line(line: &[u8], line_number: usize) -> Result<Item, CodecError> {
    let mut object = parse_object(line, line_number)?;
    let kind = take_kind(&mut object);
    let value = Value::Object(object);
    match kind.as_deref() {
        Some("entry") => serde_json::from_value(value)
            .map(Item::Entry)
            .map_err(|source| CodecError::InvalidLine {
                line: line_number,
                source,
            }),
        Some("record") => serde_json::from_value::<Record>(value)
            .map(Item::Record)
            .map_err(|source| CodecError::InvalidLine {
                line: line_number,
                source,
            }),
        Some("fact") => serde_json::from_value::<Fact>(value)
            .map(Item::Fact)
            .map_err(|source| CodecError::InvalidLine {
                line: line_number,
                source,
            }),
        Some("session") => Err(CodecError::DuplicateHeader { line: line_number }),
        _ => Err(CodecError::UnknownLineType {
            line: line_number,
            kind,
        }),
    }
}

fn parse_object(line: &[u8], line_number: usize) -> Result<Map<String, Value>, CodecError> {
    let value = serde_json::from_slice(line).map_err(|source| CodecError::InvalidLine {
        line: line_number,
        source,
    })?;
    match value {
        Value::Object(object) => Ok(object),
        _ => Err(CodecError::ExpectedObject { line: line_number }),
    }
}

fn take_kind(object: &mut Map<String, Value>) -> Option<String> {
    object
        .remove("t")
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
}

#[cfg(test)]
mod tests {
    use rho_core::{
        Entry, EntryBody, EntryId, Item, LaneName, SessionId, SessionMessage, Timestamp,
    };

    use super::*;

    fn header(version: u32) -> SessionHeader {
        SessionHeader {
            v: version,
            id: SessionId::from("session"),
            created_at: Timestamp::from("2026-08-11T00:00:00Z"),
            cwd: "/tmp/project".to_owned(),
            parent: None,
        }
    }

    fn entry(text: &str) -> Item {
        Item::Entry(Entry {
            seq: 1,
            id: EntryId::from("entry"),
            parent: None,
            lane: LaneName::main(),
            op: None,
            at: Timestamp::from("2026-08-11T00:00:01Z"),
            body: EntryBody::Message {
                message: SessionMessage::user(text),
            },
        })
    }

    #[test]
    fn typed_lines_round_trip_for_arbitrary_text_samples() {
        let samples = [
            "",
            "plain",
            "embedded \"quotes\" and \\ slashes",
            "unicode λ🦀",
            "newlines\nremain inside JSON strings",
        ];
        for sample in samples {
            let expected_header = header(FORMAT_VERSION);
            let expected_item = entry(sample);
            let mut bytes = encode_header(&expected_header).unwrap();
            bytes.extend(encode_item(&expected_item).unwrap());
            let decoded = decode_session(&bytes).unwrap();
            assert_eq!(decoded.header, expected_header);
            assert_eq!(decoded.items, [expected_item]);
            assert!(!decoded.had_torn_tail);
            assert_eq!(decoded.valid_up_to, bytes.len());
        }
    }

    #[test]
    fn torn_tail_is_the_only_ignored_parse_failure() {
        let complete = encode_header(&header(FORMAT_VERSION)).unwrap();
        let mut torn = complete.clone();
        torn.extend(br#"{"t":"entry","seq":1"#);
        let decoded = decode_session(&torn).unwrap();
        assert!(decoded.had_torn_tail);
        assert_eq!(decoded.valid_up_to, complete.len());
        assert!(decoded.items.is_empty());

        let mut malformed = complete;
        malformed.extend(b"not-json\n");
        assert!(matches!(
            decode_session(&malformed),
            Err(CodecError::InvalidLine { line: 2, .. })
        ));
    }

    #[test]
    fn future_versions_and_misplaced_headers_are_rejected() {
        let future = encode_header(&header(FORMAT_VERSION + 1)).unwrap();
        assert!(matches!(
            decode_session(&future),
            Err(CodecError::UnsupportedVersion { .. })
        ));

        let mut duplicate = encode_header(&header(FORMAT_VERSION)).unwrap();
        duplicate.extend(encode_header(&header(FORMAT_VERSION)).unwrap());
        assert!(matches!(
            decode_session(&duplicate),
            Err(CodecError::DuplicateHeader { line: 2 })
        ));
    }

    #[test]
    fn blank_complete_lines_are_rejected() {
        let mut bytes = encode_header(&header(FORMAT_VERSION)).unwrap();
        bytes.push(b'\n');
        assert!(matches!(
            decode_session(&bytes),
            Err(CodecError::InvalidLine { line: 2, .. })
        ));
    }

    #[test]
    fn wire_shape_is_flat_and_discriminated() {
        let encoded = String::from_utf8(encode_item(&entry("hello")).unwrap()).unwrap();
        let value: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(value["t"], "entry");
        assert_eq!(value["seq"], 1);
        assert_eq!(value["id"], "entry");
        assert!(!encoded.contains(r#""item""#));
    }
}
