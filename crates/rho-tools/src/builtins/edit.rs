use std::{io, path::PathBuf};

use rho_ai::{CancellationToken, ToolDefinition};
use rho_core::ReplaySafety;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{mutation, path};
use crate::{Tool, ToolFuture, ToolOutput};

/// Applies unique, non-overlapping text replacements to one existing file.
pub struct EditTool {
    definition: ToolDefinition,
    cwd: PathBuf,
}

impl EditTool {
    /// Creates an edit tool whose relative paths resolve from `cwd`.
    pub fn new(cwd: impl Into<PathBuf>) -> io::Result<Self> {
        Ok(Self::from_absolute(path::absolute(cwd.into())?))
    }

    pub(super) fn from_absolute(cwd: PathBuf) -> Self {
        Self {
            definition: ToolDefinition::new(
                "edit",
                concat!(
                    "Apply one or more unique, non-overlapping replacements to an existing file. ",
                    "Every old_text is matched against the original file. Exact matching is tried ",
                    "first, followed by conservative quote, dash, space, and trailing-whitespace normalization."
                ),
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Relative or absolute file path." },
                        "edits": {
                            "type": "array",
                            "minItems": 1,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "old_text": { "type": "string", "description": "Unique text from the original file." },
                                    "new_text": { "type": "string", "description": "Replacement text." }
                                },
                                "required": ["old_text", "new_text"],
                                "additionalProperties": false
                            }
                        }
                    },
                    "required": ["path", "edits"],
                    "additionalProperties": false
                }),
            ),
            cwd,
        }
    }
}

#[derive(Deserialize)]
struct EditInput {
    path: String,
    edits: Vec<Replacement>,
}

#[derive(Deserialize)]
struct Replacement {
    old_text: String,
    new_text: String,
}

struct LocatedReplacement {
    input_index: usize,
    start: usize,
    end: usize,
    new_text: String,
    fuzzy: bool,
}

impl Tool for EditTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    fn replay_safety(&self) -> ReplaySafety {
        ReplaySafety::Never
    }

    fn execute(&self, arguments: Value, cancellation: CancellationToken) -> ToolFuture<'_> {
        let cwd = self.cwd.clone();
        Box::pin(async move {
            let input: EditInput = match serde_json::from_value(arguments) {
                Ok(input) => input,
                Err(error) => return ToolOutput::error(format!("invalid edit arguments: {error}")),
            };
            if input.edits.is_empty() {
                return ToolOutput::error("edit requires at least one replacement");
            }
            let resolved = path::resolve(&cwd, &input.path);
            let _guard = mutation::lock(&resolved).await;
            if cancellation.is_cancelled() {
                return ToolOutput::error("edit cancelled");
            }
            let bytes = match tokio::fs::read(&resolved).await {
                Ok(bytes) => bytes,
                Err(error) => {
                    return ToolOutput::error(format!(
                        "could not read {} for editing: {error}",
                        resolved.display()
                    ));
                }
            };
            let raw = match String::from_utf8(bytes) {
                Ok(raw) => raw,
                Err(_) => {
                    return ToolOutput::error(format!(
                        "cannot edit non-UTF-8 file {}",
                        resolved.display()
                    ));
                }
            };
            if cancellation.is_cancelled() {
                return ToolOutput::error("edit cancelled");
            }
            let (bom, raw) = raw
                .strip_prefix('\u{feff}')
                .map_or(("", raw.as_str()), |text| ("\u{feff}", text));
            let ending = detect_line_ending(raw);
            let original = normalize_line_endings(raw);
            let fuzzy_original = FuzzyText::new(&original);
            let mut located = Vec::with_capacity(input.edits.len());
            for (index, replacement) in input.edits.into_iter().enumerate() {
                if replacement.old_text.is_empty() {
                    return ToolOutput::error(format!("edits[{index}].old_text must not be empty"));
                }
                match locate_replacement(&original, &fuzzy_original, index, replacement) {
                    Ok(replacement) => located.push(replacement),
                    Err(error) => return ToolOutput::error(error),
                }
            }
            located.sort_by_key(|replacement| replacement.start);
            for pair in located.windows(2) {
                if pair[1].start < pair[0].end {
                    return ToolOutput::error(format!(
                        "edits[{}] overlaps edits[{}]; merge them into one replacement",
                        pair[1].input_index, pair[0].input_index
                    ));
                }
            }
            let fuzzy_count = located
                .iter()
                .filter(|replacement| replacement.fuzzy)
                .count();
            let first_changed_line = located.first().map_or(1, |replacement| {
                original[..replacement.start].lines().count() + 1
            });
            let mut edited = original.clone();
            for replacement in located.iter().rev() {
                edited.replace_range(replacement.start..replacement.end, &replacement.new_text);
            }
            if edited == original {
                return ToolOutput::error("the replacements did not change the file");
            }
            let restored = restore_line_endings(&edited, ending);
            let output = format!("{bom}{restored}");
            if cancellation.is_cancelled() {
                return ToolOutput::error("edit cancelled");
            }
            if let Err(error) = tokio::fs::write(&resolved, output.as_bytes()).await {
                return ToolOutput::error(format!(
                    "could not write edited file {}: {error}",
                    resolved.display()
                ));
            }
            if cancellation.is_cancelled() {
                return ToolOutput::error("edit completed after cancellation was requested");
            }
            ToolOutput {
                content: vec![rho_ai::ContentBlock::text(format!(
                    "Applied {} replacement(s) to {}",
                    located.len(),
                    resolved.display()
                ))],
                is_error: false,
                details: Some(json!({
                    "path": resolved,
                    "replacements": located.len(),
                    "fuzzy_replacements": fuzzy_count,
                    "first_changed_line": first_changed_line,
                    "preserved_bom": !bom.is_empty(),
                    "line_ending": ending,
                })),
            }
        })
    }
}

fn locate_replacement(
    original: &str,
    fuzzy_original: &FuzzyText,
    input_index: usize,
    replacement: Replacement,
) -> Result<LocatedReplacement, String> {
    let old_text = normalize_line_endings(&replacement.old_text);
    let new_text = normalize_line_endings(&replacement.new_text);
    let fuzzy_old = FuzzyText::new(&old_text).text;
    if fuzzy_old.is_empty() {
        return Err(format!(
            "edits[{input_index}].old_text is empty after whitespace normalization"
        ));
    }
    let normalized_matches = occurrences(&fuzzy_original.text, &fuzzy_old);
    if normalized_matches.len() > 1 {
        return Err(format!(
            "edits[{input_index}].old_text has {} normalized matches; include more context",
            normalized_matches.len()
        ));
    }
    let exact = occurrences(original, &old_text);
    if exact.len() > 1 {
        return Err(format!(
            "edits[{input_index}].old_text occurs {} times; include more context",
            exact.len()
        ));
    }
    if let Some(start) = exact.first().copied() {
        return Ok(LocatedReplacement {
            input_index,
            start,
            end: start + old_text.len(),
            new_text,
            fuzzy: false,
        });
    }

    if normalized_matches.is_empty() {
        return Err(format!(
            "could not find edits[{input_index}].old_text; whitespace and newlines must identify existing text"
        ));
    }
    let fuzzy_start = normalized_matches[0];
    let fuzzy_end = fuzzy_start + fuzzy_old.len();
    let start = fuzzy_original.boundary(fuzzy_start).ok_or_else(|| {
        format!("edits[{input_index}] did not align to a source character boundary")
    })?;
    let end = fuzzy_original.boundary(fuzzy_end).ok_or_else(|| {
        format!("edits[{input_index}] did not align to a source character boundary")
    })?;
    Ok(LocatedReplacement {
        input_index,
        start,
        end,
        new_text,
        fuzzy: true,
    })
}

fn occurrences(haystack: &str, needle: &str) -> Vec<usize> {
    if needle.is_empty() {
        return Vec::new();
    }
    let mut found = Vec::new();
    let mut offset = 0;
    while let Some(relative) = haystack[offset..].find(needle) {
        let index = offset + relative;
        found.push(index);
        let advance = haystack[index..].chars().next().map_or(1, char::len_utf8);
        offset = index + advance;
        if offset >= haystack.len() {
            break;
        }
    }
    found
}

struct FuzzyText {
    text: String,
    boundaries: Vec<Option<usize>>,
}

impl FuzzyText {
    fn new(source: &str) -> Self {
        let mut normalized = Self {
            text: String::new(),
            boundaries: vec![Some(0)],
        };
        let mut source_offset = 0;
        for segment in source.split_inclusive('\n') {
            let has_newline = segment.ends_with('\n');
            let content = segment.strip_suffix('\n').unwrap_or(segment);
            let trimmed = content.trim_end_matches(char::is_whitespace);
            for (relative, character) in trimmed.char_indices() {
                let start = source_offset + relative;
                let end = start + character.len_utf8();
                if let Some(replacement) = normalized_character(character) {
                    normalized.push(replacement, start, end);
                } else {
                    let mut encoded = [0; 4];
                    normalized.push(character.encode_utf8(&mut encoded), start, end);
                }
            }
            let line_end = source_offset + content.len();
            normalized.set_boundary(line_end);
            if has_newline {
                normalized.push("\n", line_end, line_end + 1);
            }
            source_offset += segment.len();
        }
        normalized.set_boundary(source.len());
        normalized
    }

    fn push(&mut self, value: &str, source_start: usize, source_end: usize) {
        let start = self.text.len();
        self.text.push_str(value);
        self.boundaries.resize(self.text.len() + 1, None);
        self.boundaries[start] = Some(source_start);
        self.boundaries[self.text.len()] = Some(source_end);
    }

    fn set_boundary(&mut self, source: usize) {
        let index = self.text.len();
        self.boundaries.resize(index + 1, None);
        self.boundaries[index] = Some(source);
    }

    fn boundary(&self, index: usize) -> Option<usize> {
        self.boundaries.get(index).copied().flatten()
    }
}

fn normalized_character(character: char) -> Option<&'static str> {
    match character {
        '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}' | '\u{2032}' => Some("'"),
        '\u{201c}' | '\u{201d}' | '\u{201e}' | '\u{2033}' => Some("\""),
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
        | '\u{2212}' | '\u{fe58}' | '\u{fe63}' | '\u{ff0d}' => Some("-"),
        '\u{00a0}' | '\u{2000}' | '\u{2001}' | '\u{2002}' | '\u{2003}' | '\u{2004}'
        | '\u{2005}' | '\u{2006}' | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200a}'
        | '\u{202f}' | '\u{205f}' | '\u{3000}' => Some(" "),
        _ => None,
    }
}

fn detect_line_ending(text: &str) -> &'static str {
    text.find('\n').map_or("\n", |index| {
        if index > 0 && text.as_bytes()[index - 1] == b'\r' {
            "\r\n"
        } else {
            "\n"
        }
    })
}

fn normalize_line_endings(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn restore_line_endings(text: &str, ending: &str) -> String {
    if ending == "\r\n" {
        text.replace('\n', "\r\n")
    } else {
        text.to_owned()
    }
}
