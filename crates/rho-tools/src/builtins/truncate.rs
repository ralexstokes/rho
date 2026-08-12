use serde_json::{Value, json};

pub(super) const MAX_BYTES: usize = 50 * 1024;
pub(super) const MAX_LINES: usize = 2_000;

#[derive(Clone, Copy)]
pub(super) enum Keep {
    Head,
    Tail,
}

pub(super) struct Truncated {
    pub(super) text: String,
    pub(super) details: Option<Value>,
    pub(super) shown_lines: usize,
    pub(super) partial_line: bool,
}

pub(super) fn output(text: &str, keep: Keep) -> Truncated {
    output_with_limits(text, keep, MAX_LINES, MAX_BYTES)
}

fn output_with_limits(text: &str, keep: Keep, max_lines: usize, max_bytes: usize) -> Truncated {
    let lines = text.split_inclusive('\n').collect::<Vec<_>>();
    let total_lines = lines.len();
    let total_bytes = text.len();
    if total_lines <= max_lines && total_bytes <= max_bytes {
        return Truncated {
            text: text.to_owned(),
            details: None,
            shown_lines: total_lines,
            partial_line: false,
        };
    }

    let mut selected = Vec::new();
    let mut selected_bytes = 0_usize;
    let candidates: Box<dyn Iterator<Item = &str>> = match keep {
        Keep::Head => Box::new(lines.iter().copied()),
        Keep::Tail => Box::new(lines.iter().rev().copied()),
    };
    for line in candidates {
        if selected.len() == max_lines || selected_bytes.saturating_add(line.len()) > max_bytes {
            break;
        }
        selected.push(line);
        selected_bytes += line.len();
    }
    let partial_line = selected.is_empty() && !text.is_empty();
    if partial_line {
        let source = match keep {
            Keep::Head => text,
            Keep::Tail => {
                let start = ceil_char_boundary(text, text.len().saturating_sub(max_bytes));
                &text[start..]
            }
        };
        let end = match keep {
            Keep::Head => floor_char_boundary(source, max_bytes.min(source.len())),
            Keep::Tail => source.len(),
        };
        selected.push(&source[..end]);
        selected_bytes = end;
    }
    if matches!(keep, Keep::Tail) {
        selected.reverse();
    }
    let selected_text = selected.concat();
    let selected_lines = selected.len();
    let omitted_lines = total_lines.saturating_sub(selected_lines);
    let marker = match (keep, partial_line) {
        (Keep::Head, true) => format!(
            "\n[output truncated inside an overlong line: showing the first {selected_bytes} of {total_bytes} bytes]"
        ),
        (Keep::Tail, true) => format!(
            "[output truncated inside an overlong line: showing the last {selected_bytes} of {total_bytes} bytes]\n"
        ),
        (Keep::Head, false) => format!(
            "\n[output truncated: omitted {omitted_lines} trailing lines; {total_bytes} bytes total]"
        ),
        (Keep::Tail, false) => format!(
            "[output truncated: omitted {omitted_lines} leading lines; {total_bytes} bytes total]\n"
        ),
    };
    let text = match keep {
        Keep::Head => format!("{selected_text}{marker}"),
        Keep::Tail => format!("{marker}{selected_text}"),
    };
    Truncated {
        text,
        details: Some(json!({
            "truncated": true,
            "kept": match keep { Keep::Head => "head", Keep::Tail => "tail" },
            "total_lines": total_lines,
            "shown_lines": selected_lines,
            "total_bytes": total_bytes,
            "shown_bytes": selected_bytes,
            "partial_line": partial_line,
        })),
        shown_lines: selected_lines,
        partial_line,
    }
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(text: &str, mut index: usize) -> usize {
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_keeps_the_requested_end_and_marks_omissions() {
        let text = "one\ntwo\nthree\nfour\n";
        let head = output_with_limits(text, Keep::Head, 2, 100);
        assert!(head.text.starts_with("one\ntwo\n"));
        assert!(head.text.contains("omitted 2 trailing lines"));
        let tail = output_with_limits(text, Keep::Tail, 2, 100);
        assert!(tail.text.ends_with("three\nfour\n"));
        assert!(tail.text.contains("omitted 2 leading lines"));
    }

    #[test]
    fn byte_truncation_never_splits_utf8() {
        let truncated = output_with_limits("🦀🦀🦀", Keep::Head, 10, 5);
        assert!(truncated.text.starts_with('🦀'));
    }
}
