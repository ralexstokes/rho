use std::collections::BTreeSet;

use rho_ai::{ContentBlock, Message, ThinkingLevel, ToolCallId};
use thiserror::Error;

use crate::{Entry, EntryBody, EntryId, ModelRef, SessionMessage};

/// Settings derived from the full root-to-leaf path.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionSettings {
    /// Most recently selected model.
    pub model: Option<ModelRef>,
    /// Most recently selected reasoning level.
    pub thinking: Option<ThinkingLevel>,
}

/// Provider context and settings derived from a branch.
#[derive(Clone, Debug, PartialEq)]
pub struct AssembledContext {
    /// Authoritative provider transcript.
    pub messages: Vec<Message>,
    /// Effective settings.
    pub settings: SessionSettings,
}

/// Deterministic compaction cut point.
#[derive(Clone, Debug, PartialEq)]
pub struct CompactionPlan {
    /// Messages to summarize.
    pub compacted: Vec<SessionMessage>,
    /// Self-contained tail copied into the checkpoint.
    pub retained_tail: Vec<SessionMessage>,
    /// First retained entry when the tail maps directly to stored entries.
    pub first_kept: Option<EntryId>,
}

/// Invalid branch or compaction input.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ContextError {
    /// Entries did not form one root-to-leaf path in the supplied order.
    #[error("entry {entry} expected parent {expected:?}, found {actual:?}")]
    BrokenPath {
        /// Entry with the bad parent.
        entry: EntryId,
        /// Required parent.
        expected: Option<EntryId>,
        /// Actual parent.
        actual: Option<EntryId>,
    },
    /// A compaction back-pointer does not identify an earlier entry.
    #[error("compaction entry {compaction} refers to missing retained entry {first_kept}")]
    MissingFirstKept {
        /// Compaction entry.
        compaction: EntryId,
        /// Missing back-pointer.
        first_kept: EntryId,
    },
}

/// Builds provider context from an ordered root-to-leaf branch.
pub fn assemble_context(entries: &[Entry]) -> Result<AssembledContext, ContextError> {
    validate_path(entries)?;
    let settings = derive_settings(entries);
    let newest_compaction = entries
        .iter()
        .enumerate()
        .rev()
        .find(|(_, entry)| matches!(entry.body, EntryBody::Compaction { .. }));
    let mut messages = Vec::new();
    let start = if let Some((index, entry)) = newest_compaction {
        let EntryBody::Compaction {
            summary,
            first_kept,
            retained_tail,
            ..
        } = &entry.body
        else {
            unreachable!()
        };
        messages.push(Message::user(summary.clone()));
        if retained_tail.is_empty() {
            if let Some(first_kept) = first_kept {
                let kept_index = entries[..index]
                    .iter()
                    .position(|candidate| &candidate.id == first_kept)
                    .ok_or_else(|| ContextError::MissingFirstKept {
                        compaction: entry.id.clone(),
                        first_kept: first_kept.clone(),
                    })?;
                append_messages(&entries[kept_index..index], &mut messages);
            }
        } else {
            messages.extend(retained_tail.iter().map(SessionMessage::to_provider));
        }
        index + 1
    } else {
        0
    };
    append_messages(&entries[start..], &mut messages);
    Ok(AssembledContext { messages, settings })
}

/// Chooses a retained tail without separating a tool result from its call.
#[must_use]
pub fn plan_compaction(entries: &[Entry], retain_messages: usize) -> CompactionPlan {
    let messages = entries
        .iter()
        .filter_map(|entry| match &entry.body {
            EntryBody::Message { message } => Some((entry.id.clone(), message.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut cut = messages.len().saturating_sub(retain_messages);

    loop {
        let result_calls = messages[cut..]
            .iter()
            .filter_map(|(_, message)| match message {
                SessionMessage::ToolResult { call_id, .. } => Some(call_id.clone()),
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        let mut earlier_call = None;
        for (index, (_, message)) in messages[..cut].iter().enumerate().rev() {
            if assistant_calls(message)
                .iter()
                .any(|call_id| result_calls.contains(call_id))
            {
                earlier_call = Some(index);
            }
        }
        let Some(index) = earlier_call else {
            break;
        };
        cut = index;
    }

    CompactionPlan {
        compacted: messages[..cut]
            .iter()
            .map(|(_, message)| message.clone())
            .collect(),
        retained_tail: messages[cut..]
            .iter()
            .map(|(_, message)| message.clone())
            .collect(),
        first_kept: messages.get(cut).map(|(id, _)| id.clone()),
    }
}

/// Returns provider tool calls that lack a later result on this branch.
pub fn unresolved_tool_calls(entries: &[Entry]) -> Result<Vec<ToolCallId>, ContextError> {
    let context = assemble_context(entries)?;
    let mut unresolved = Vec::<ToolCallId>::new();
    for message in context.messages {
        match message {
            Message::Assistant(message) => {
                unresolved.extend(message.blocks.into_iter().filter_map(|block| match block {
                    ContentBlock::ToolCall { id, .. }
                    | ContentBlock::RejectedToolCall { id, .. } => Some(id),
                    _ => None,
                }));
            }
            Message::ToolResult(result) => {
                if let Some(index) = unresolved
                    .iter()
                    .position(|call_id| call_id == &result.call_id)
                {
                    unresolved.remove(index);
                }
            }
            Message::User { .. } => {}
            _ => {}
        }
    }
    Ok(unresolved)
}

fn validate_path(entries: &[Entry]) -> Result<(), ContextError> {
    let mut expected = None;
    for entry in entries {
        if entry.parent != expected {
            return Err(ContextError::BrokenPath {
                entry: entry.id.clone(),
                expected,
                actual: entry.parent.clone(),
            });
        }
        expected = Some(entry.id.clone());
    }
    Ok(())
}

fn derive_settings(entries: &[Entry]) -> SessionSettings {
    let mut settings = SessionSettings::default();
    for entry in entries {
        if let EntryBody::SettingsChange { model, thinking } = &entry.body {
            if let Some(model) = model {
                settings.model = Some(model.clone());
            }
            if let Some(thinking) = thinking {
                settings.thinking = Some(*thinking);
            }
        }
    }
    settings
}

fn append_messages(entries: &[Entry], output: &mut Vec<Message>) {
    output.extend(entries.iter().filter_map(|entry| match &entry.body {
        EntryBody::Message { message } => Some(message.to_provider()),
        EntryBody::Compaction { .. }
        | EntryBody::SettingsChange { .. }
        | EntryBody::Custom { .. } => None,
    }));
}

fn assistant_calls(message: &SessionMessage) -> Vec<ToolCallId> {
    let SessionMessage::Assistant(message) = message else {
        return Vec::new();
    };
    message
        .blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolCall { id, .. } | ContentBlock::RejectedToolCall { id, .. } => {
                Some(id.clone())
            }
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use rho_ai::{
        AssistantMessage, ContentBlock, ModelId, ProviderId, StopReason, ToolArgumentError,
        ToolCallId, Usage,
    };

    use crate::{LaneName, Timestamp};

    use super::*;

    fn entry(index: u64, body: EntryBody) -> Entry {
        Entry {
            seq: index,
            id: format!("e{index}").into(),
            parent: (index > 1).then(|| format!("e{}", index - 1).into()),
            lane: LaneName::main(),
            op: None,
            source_queue: None,
            at: Timestamp::from("t"),
            body,
        }
    }

    #[test]
    fn newest_compaction_is_a_self_contained_checkpoint() {
        let entries = vec![
            entry(
                1,
                EntryBody::Message {
                    message: SessionMessage::user("old"),
                },
            ),
            entry(
                2,
                EntryBody::Compaction {
                    summary: "summary".to_owned(),
                    first_kept: None,
                    retained_tail: vec![SessionMessage::user("tail")],
                    tokens_before: 100,
                    usage: Usage::default(),
                },
            ),
            entry(
                3,
                EntryBody::Message {
                    message: SessionMessage::user("new"),
                },
            ),
        ];
        let context = assemble_context(&entries).unwrap();
        assert_eq!(
            context.messages,
            [
                Message::user("summary"),
                Message::user("tail"),
                Message::user("new")
            ]
        );
    }

    fn assert_compaction_keeps_call_and_result_together(call: ContentBlock) {
        let call_id = match &call {
            ContentBlock::ToolCall { id, .. } | ContentBlock::RejectedToolCall { id, .. } => {
                id.clone()
            }
            _ => panic!("test requires a tool call block"),
        };
        let assistant = AssistantMessage {
            blocks: vec![call],
            stop: StopReason::ToolUse,
            usage: Usage::default(),
            provider: ProviderId::from("p"),
            model: ModelId::from("m"),
        };
        let entries = vec![
            entry(
                1,
                EntryBody::Message {
                    message: SessionMessage::user("old"),
                },
            ),
            entry(
                2,
                EntryBody::Message {
                    message: SessionMessage::Assistant(assistant),
                },
            ),
            entry(
                3,
                EntryBody::Message {
                    message: SessionMessage::ToolResult {
                        call_id,
                        content: vec![ContentBlock::text("ok")],
                        is_error: false,
                        details: None,
                    },
                },
            ),
        ];
        let plan = plan_compaction(&entries, 1);
        assert_eq!(plan.compacted, [SessionMessage::user("old")]);
        assert_eq!(plan.retained_tail.len(), 2);
        assert_eq!(plan.first_kept, Some(EntryId::from("e2")));
        assert_eq!(
            unresolved_tool_calls(&entries[..2]).unwrap(),
            [ToolCallId::from("call")]
        );
        assert!(unresolved_tool_calls(&entries).unwrap().is_empty());
    }

    #[test]
    fn compaction_keeps_tool_call_and_result_together() {
        assert_compaction_keeps_call_and_result_together(ContentBlock::ToolCall {
            id: ToolCallId::from("call"),
            name: "read".to_owned(),
            args: serde_json::json!({}),
        });
    }

    #[test]
    fn compaction_keeps_rejected_tool_call_and_result_together() {
        assert_compaction_keeps_call_and_result_together(ContentBlock::RejectedToolCall {
            id: ToolCallId::from("call"),
            name: "read".to_owned(),
            args: None,
            error: ToolArgumentError {
                kind: "json_parse".to_owned(),
                message: "bad JSON".to_owned(),
            },
        });
    }

    #[test]
    fn settings_before_checkpoint_remain_effective() {
        let entries = vec![
            entry(
                1,
                EntryBody::SettingsChange {
                    model: Some(ModelRef {
                        provider: ProviderId::from("p"),
                        model: ModelId::from("m"),
                    }),
                    thinking: Some(ThinkingLevel::Medium),
                },
            ),
            entry(
                2,
                EntryBody::Compaction {
                    summary: "summary".to_owned(),
                    first_kept: None,
                    retained_tail: Vec::new(),
                    tokens_before: 0,
                    usage: Usage::default(),
                },
            ),
        ];
        let context = assemble_context(&entries).unwrap();
        assert_eq!(context.settings.model.unwrap().model, ModelId::from("m"));
        assert_eq!(context.settings.thinking, Some(ThinkingLevel::Medium));
    }
}
