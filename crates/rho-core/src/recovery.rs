use std::collections::{BTreeMap, BTreeSet};

use rho_ai::ToolCallId;

use crate::{
    CompactionWork, CompletedCompaction, CorruptionReason, Item, LaneName, LaneStatus, OpId,
    OpIntent, OpenTool, RecordBody, SessionMessage, SuspendedCompaction, SuspendedOp,
};

#[derive(Debug)]
struct OperationState {
    operation_started: bool,
    intent: OpIntent,
    finished: bool,
    abort_requested: bool,
    last_step: Option<u32>,
    stream_in_flight: bool,
    started_tools: BTreeSet<ToolCallId>,
    open_tools: Vec<OpenTool>,
    last_assistant: Option<rho_ai::AssistantMessage>,
    last_assistant_usage_recorded: bool,
    resolved_tool_calls: Vec<ToolCallId>,
    compaction_work: Option<CompactionWork>,
    completed_compaction: Option<CompletedCompaction>,
    compaction_usage_recorded: bool,
}

/// Reduces an interleaved session stream to one lane's recoverable status.
///
/// The reducer never repairs or guesses. Every impossible sequence maps to a
/// stable [`CorruptionReason`].
#[must_use]
pub fn reduce_lane_status(items: &[Item], lane: &LaneName) -> LaneStatus {
    if let Some(reason) = validate_sequences(items) {
        return LaneStatus::Corrupt(reason);
    }

    let known_ops = items
        .iter()
        .filter_map(|item| match item {
            Item::Record(record) if &record.lane == lane => match &record.body {
                RecordBody::OpStarted { op, .. } => Some(op.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let mut operations = BTreeMap::<OpId, OperationState>::new();
    let mut open = None::<OpId>;

    for item in items {
        let result = match item {
            Item::Record(record) if &record.lane == lane => {
                reduce_record(&record.body, &known_ops, &mut operations, &mut open)
            }
            Item::Entry(entry) if &entry.lane == lane => reduce_entry(
                entry.op.as_ref(),
                &entry.body,
                &known_ops,
                &mut operations,
                &mut open,
            ),
            Item::Entry(_) | Item::Record(_) | Item::Fact(_) => Ok(()),
        };
        if let Err(reason) = result {
            return LaneStatus::Corrupt(reason);
        }
    }

    let Some(op) = open else {
        return LaneStatus::Idle;
    };
    let state = &operations[&op];
    LaneStatus::Suspended(SuspendedOp {
        op,
        operation_started: state.operation_started,
        intent: state.intent,
        abort_requested: state.abort_requested,
        last_step: state.last_step,
        open_tools: state.open_tools.clone(),
        stream_in_flight: state.stream_in_flight,
        last_assistant: state.last_assistant.clone(),
        last_assistant_usage_recorded: state.last_assistant_usage_recorded,
        resolved_tool_calls: state.resolved_tool_calls.clone(),
        compaction: state.compaction_work.clone().map(|work| {
            Box::new(SuspendedCompaction {
                work,
                completed: state.completed_compaction.clone(),
                usage_recorded: state.compaction_usage_recorded,
            })
        }),
    })
}

fn validate_sequences(items: &[Item]) -> Option<CorruptionReason> {
    for (expected, item) in (1_u64..).zip(items) {
        let actual = item.seq();
        if actual != expected {
            return Some(CorruptionReason::NonConsecutiveSequence { expected, actual });
        }
    }
    None
}

fn reduce_record(
    body: &RecordBody,
    known_ops: &BTreeSet<OpId>,
    operations: &mut BTreeMap<OpId, OperationState>,
    open: &mut Option<OpId>,
) -> Result<(), CorruptionReason> {
    if let RecordBody::OpStarted { op, intent, .. } = body {
        if operations.contains_key(op) {
            return Err(CorruptionReason::DuplicateOperation { op: op.clone() });
        }
        if let Some(first) = open {
            return Err(CorruptionReason::MultipleOpenOperations {
                first: first.clone(),
                second: op.clone(),
            });
        }
        operations.insert(
            op.clone(),
            OperationState {
                operation_started: true,
                intent: *intent,
                finished: false,
                abort_requested: false,
                last_step: None,
                stream_in_flight: false,
                started_tools: BTreeSet::new(),
                open_tools: Vec::new(),
                last_assistant: None,
                last_assistant_usage_recorded: false,
                resolved_tool_calls: Vec::new(),
                compaction_work: None,
                completed_compaction: None,
                compaction_usage_recorded: false,
            },
        );
        *open = Some(op.clone());
        return Ok(());
    }

    let Some(op) = body.op() else {
        return Ok(());
    };
    require_known(op, known_ops)?;
    let state = operations
        .get_mut(op)
        .ok_or_else(|| CorruptionReason::UnknownOperation { op: op.clone() })?;
    if state.finished {
        return Err(CorruptionReason::ItemAfterFinish { op: op.clone() });
    }

    match body {
        RecordBody::OpFinished { outcome, .. } => {
            if !state.open_tools.is_empty() {
                return Err(CorruptionReason::FinishedWithOpenTools {
                    op: op.clone(),
                    call_ids: state
                        .open_tools
                        .iter()
                        .map(|tool| tool.call_id.clone())
                        .collect(),
                });
            }
            if matches!(outcome, crate::OpOutcome::Completed) && state.stream_in_flight {
                return Err(CorruptionReason::CompletedWithStreamInFlight { op: op.clone() });
            }
            if matches!(outcome, crate::OpOutcome::Completed)
                && state.intent == OpIntent::Compaction
                && state.completed_compaction.is_none()
            {
                return Err(CorruptionReason::CompletedCompactionWithoutCheckpoint {
                    op: op.clone(),
                });
            }
            state.finished = true;
            if open.as_ref() == Some(op) {
                *open = None;
            }
        }
        RecordBody::AbortRequested { .. } => state.abort_requested = true,
        RecordBody::Step { n, .. } => {
            let expected = state.last_step.map_or(1, |last| last + 1);
            if *n != expected {
                return Err(CorruptionReason::NonConsecutiveStep {
                    op: op.clone(),
                    expected,
                    actual: *n,
                });
            }
            state.last_step = Some(*n);
            state.stream_in_flight = true;
        }
        RecordBody::ToolStarted {
            call_id,
            name,
            effective_args,
            replay,
            ..
        } => {
            if state.intent != OpIntent::Run {
                return Err(CorruptionReason::IntentMismatch {
                    op: op.clone(),
                    intent: state.intent,
                });
            }
            if state.stream_in_flight {
                return Err(CorruptionReason::ToolStartedBeforeAssistant {
                    op: op.clone(),
                    call_id: call_id.clone(),
                });
            }
            if !state.started_tools.insert(call_id.clone()) {
                return Err(CorruptionReason::DuplicateToolStart {
                    op: op.clone(),
                    call_id: call_id.clone(),
                });
            }
            state.open_tools.push(OpenTool {
                call_id: call_id.clone(),
                name: name.clone(),
                effective_args: effective_args.clone(),
                replay: *replay,
            });
        }
        RecordBody::CompactionStarted { work, .. } => {
            if state.intent != OpIntent::Compaction {
                return Err(CorruptionReason::IntentMismatch {
                    op: op.clone(),
                    intent: state.intent,
                });
            }
            if state.compaction_work.replace(work.clone()).is_some() {
                return Err(CorruptionReason::DuplicateCompactionStart { op: op.clone() });
            }
        }
        RecordBody::Usage { .. } => {
            if state.completed_compaction.is_some() {
                state.compaction_usage_recorded = true;
            } else if state.last_assistant.is_some() {
                state.last_assistant_usage_recorded = true;
            }
        }
        RecordBody::QueueChanged { .. } => {}
        RecordBody::OpStarted { .. } | RecordBody::LaneMoved { .. } => {}
    }
    Ok(())
}

fn reduce_entry(
    op: Option<&OpId>,
    body: &crate::EntryBody,
    known_ops: &BTreeSet<OpId>,
    operations: &mut BTreeMap<OpId, OperationState>,
    open: &mut Option<OpId>,
) -> Result<(), CorruptionReason> {
    let Some(op) = op else {
        return Ok(());
    };
    if !known_ops.contains(op) {
        return match body {
            crate::EntryBody::Message {
                message: SessionMessage::User { .. },
            } => {
                if let Some(first) = open.as_ref() {
                    return Err(CorruptionReason::MultipleOpenOperations {
                        first: first.clone(),
                        second: op.clone(),
                    });
                }
                operations.insert(
                    op.clone(),
                    OperationState {
                        operation_started: false,
                        intent: OpIntent::Run,
                        finished: false,
                        abort_requested: false,
                        last_step: None,
                        stream_in_flight: false,
                        started_tools: BTreeSet::new(),
                        open_tools: Vec::new(),
                        last_assistant: None,
                        last_assistant_usage_recorded: false,
                        resolved_tool_calls: Vec::new(),
                        compaction_work: None,
                        completed_compaction: None,
                        compaction_usage_recorded: false,
                    },
                );
                *open = Some(op.clone());
                Ok(())
            }
            _ => Err(CorruptionReason::UnknownOperation { op: op.clone() }),
        };
    }

    // The initial user entry is deliberately written immediately before
    // OpStarted, so an operation may be known but not yet encountered.
    let Some(state) = operations.get_mut(op) else {
        return match body {
            crate::EntryBody::Message {
                message: SessionMessage::User { .. },
            } => Ok(()),
            _ => Err(CorruptionReason::UnknownOperation { op: op.clone() }),
        };
    };
    if state.finished {
        return Err(CorruptionReason::ItemAfterFinish { op: op.clone() });
    }
    match body {
        crate::EntryBody::Message { message } => match message {
            SessionMessage::Assistant(message) => {
                if state.intent != OpIntent::Run {
                    return Err(CorruptionReason::IntentMismatch {
                        op: op.clone(),
                        intent: state.intent,
                    });
                }
                if !state.stream_in_flight {
                    return Err(CorruptionReason::AssistantWithoutStep { op: op.clone() });
                }
                state.stream_in_flight = false;
                state.last_assistant = Some(message.clone());
                state.last_assistant_usage_recorded = false;
                state.resolved_tool_calls.clear();
            }
            SessionMessage::ToolResult { call_id, .. } => {
                let Some(position) = state
                    .open_tools
                    .iter()
                    .position(|tool| tool.call_id == *call_id)
                else {
                    return Err(CorruptionReason::ToolResultWithoutStart {
                        op: op.clone(),
                        call_id: call_id.clone(),
                    });
                };
                state.open_tools.remove(position);
                state.resolved_tool_calls.push(call_id.clone());
            }
            SessionMessage::User { .. } | SessionMessage::Custom { .. } => {}
        },
        crate::EntryBody::Compaction { summary, usage, .. } => {
            if state.intent != OpIntent::Compaction {
                return Err(CorruptionReason::IntentMismatch {
                    op: op.clone(),
                    intent: state.intent,
                });
            }
            if state.compaction_work.is_none() {
                return Err(CorruptionReason::CompactionWithoutStart { op: op.clone() });
            }
            if !state.stream_in_flight {
                return Err(CorruptionReason::AssistantWithoutStep { op: op.clone() });
            }
            state.stream_in_flight = false;
            state.completed_compaction = Some(CompletedCompaction {
                summary: summary.clone(),
                usage: usage.clone(),
            });
            state.compaction_usage_recorded = false;
        }
        crate::EntryBody::SettingsChange { .. } | crate::EntryBody::Custom { .. } => {}
    }
    Ok(())
}

fn require_known(op: &OpId, known_ops: &BTreeSet<OpId>) -> Result<(), CorruptionReason> {
    if known_ops.contains(op) {
        Ok(())
    } else {
        Err(CorruptionReason::UnknownOperation { op: op.clone() })
    }
}

#[cfg(test)]
mod tests {
    use rho_ai::{AssistantMessage, ModelId, ProviderId, StopReason, ToolCallId, Usage};
    use serde_json::json;

    use crate::{
        Entry, EntryBody, Item, LaneStatus, NewRecord, OpOutcome, Origin, Record, ReplaySafety,
        SessionMessage, Timestamp,
    };

    use super::*;

    fn record(seq: u64, body: RecordBody) -> Item {
        Item::Record(Record {
            seq,
            lane: LaneName::main(),
            at: Timestamp::from("t"),
            body,
        })
    }

    fn entry(seq: u64, op: Option<&str>, message: SessionMessage) -> Item {
        Item::Entry(Entry {
            seq,
            id: format!("e{seq}").into(),
            parent: None,
            lane: LaneName::main(),
            op: op.map(OpId::from),
            at: Timestamp::from("t"),
            body: EntryBody::Message { message },
        })
    }

    fn started(op: &str) -> RecordBody {
        RecordBody::OpStarted {
            op: op.into(),
            intent: OpIntent::Run,
            origin: Origin::External,
            host: None,
        }
    }

    fn compaction_started(op: &str) -> RecordBody {
        RecordBody::OpStarted {
            op: op.into(),
            intent: OpIntent::Compaction,
            origin: Origin::External,
            host: None,
        }
    }

    fn compaction_work(op: &str) -> RecordBody {
        RecordBody::CompactionStarted {
            op: op.into(),
            work: CompactionWork {
                compacted: vec![SessionMessage::user("old")],
                retained_tail: vec![SessionMessage::user("tail")],
                first_kept: None,
                tokens_before: 100,
            },
        }
    }

    fn compaction_entry(seq: u64, op: &str) -> Item {
        Item::Entry(Entry {
            seq,
            id: format!("e{seq}").into(),
            parent: None,
            lane: LaneName::main(),
            op: Some(OpId::from(op)),
            at: Timestamp::from("t"),
            body: EntryBody::Compaction {
                summary: "summary".to_owned(),
                first_kept: None,
                retained_tail: Vec::new(),
                tokens_before: 100,
                usage: Usage::default(),
            },
        })
    }

    fn assistant() -> SessionMessage {
        SessionMessage::Assistant(AssistantMessage {
            blocks: Vec::new(),
            stop: StopReason::Stop,
            usage: Usage::default(),
            provider: ProviderId::from("p"),
            model: ModelId::from("m"),
        })
    }

    #[test]
    fn truth_table_covers_idle_and_suspended_shapes() {
        assert_eq!(reduce_lane_status(&[], &LaneName::main()), LaneStatus::Idle);

        let items = vec![
            record(1, started("op")),
            record(
                2,
                RecordBody::Step {
                    op: OpId::from("op"),
                    n: 1,
                },
            ),
        ];
        assert!(matches!(
            reduce_lane_status(&items, &LaneName::main()),
            LaneStatus::Suspended(SuspendedOp {
                stream_in_flight: true,
                last_step: Some(1),
                ..
            })
        ));
    }

    #[test]
    fn user_entry_before_missing_op_start_is_recoverable() {
        let items = vec![entry(1, Some("op"), SessionMessage::user("durable prompt"))];
        let LaneStatus::Suspended(status) = reduce_lane_status(&items, &LaneName::main()) else {
            panic!("expected recoverable pre-start operation");
        };
        assert_eq!(status.op, OpId::from("op"));
        assert!(!status.operation_started);
        assert_eq!(status.last_step, None);
    }

    #[test]
    fn open_tools_close_only_with_matching_result() {
        let call = ToolCallId::from("call");
        let items = vec![
            record(1, started("op")),
            record(
                2,
                RecordBody::ToolStarted {
                    op: OpId::from("op"),
                    call_id: call.clone(),
                    name: "write".to_owned(),
                    effective_args: json!({"path": "x"}),
                    replay: ReplaySafety::Never,
                },
            ),
        ];
        let LaneStatus::Suspended(status) = reduce_lane_status(&items, &LaneName::main()) else {
            panic!("expected suspended status");
        };
        assert_eq!(status.open_tools.len(), 1);

        let mut closed = items;
        closed.push(entry(
            3,
            Some("op"),
            SessionMessage::ToolResult {
                call_id: call,
                content: Vec::new(),
                is_error: false,
                details: None,
            },
        ));
        closed.push(record(
            4,
            RecordBody::OpFinished {
                op: OpId::from("op"),
                outcome: OpOutcome::Completed,
            },
        ));
        assert_eq!(
            reduce_lane_status(&closed, &LaneName::main()),
            LaneStatus::Idle
        );
    }

    #[test]
    fn every_named_impossible_sequence_is_typed() {
        let cases = [
            (
                vec![record(2, started("op"))],
                CorruptionReason::NonConsecutiveSequence {
                    expected: 1,
                    actual: 2,
                },
            ),
            (
                vec![record(1, started("a")), record(2, started("b"))],
                CorruptionReason::MultipleOpenOperations {
                    first: OpId::from("a"),
                    second: OpId::from("b"),
                },
            ),
            (
                vec![record(1, started("a")), record(2, started("a"))],
                CorruptionReason::DuplicateOperation {
                    op: OpId::from("a"),
                },
            ),
            (
                vec![record(
                    1,
                    RecordBody::Step {
                        op: OpId::from("missing"),
                        n: 1,
                    },
                )],
                CorruptionReason::UnknownOperation {
                    op: OpId::from("missing"),
                },
            ),
            (
                vec![
                    record(1, started("a")),
                    record(
                        2,
                        RecordBody::OpFinished {
                            op: OpId::from("a"),
                            outcome: OpOutcome::Completed,
                        },
                    ),
                    record(
                        3,
                        RecordBody::Usage {
                            op: OpId::from("a"),
                            usage: Usage::default(),
                        },
                    ),
                ],
                CorruptionReason::ItemAfterFinish {
                    op: OpId::from("a"),
                },
            ),
            (
                vec![
                    record(1, started("a")),
                    record(
                        2,
                        RecordBody::Step {
                            op: OpId::from("a"),
                            n: 2,
                        },
                    ),
                ],
                CorruptionReason::NonConsecutiveStep {
                    op: OpId::from("a"),
                    expected: 1,
                    actual: 2,
                },
            ),
            (
                vec![record(1, started("a")), entry(2, Some("a"), assistant())],
                CorruptionReason::AssistantWithoutStep {
                    op: OpId::from("a"),
                },
            ),
            (
                vec![
                    record(1, started("a")),
                    record(
                        2,
                        RecordBody::ToolStarted {
                            op: OpId::from("a"),
                            call_id: ToolCallId::from("c"),
                            name: "x".to_owned(),
                            effective_args: json!({}),
                            replay: ReplaySafety::Safe,
                        },
                    ),
                    record(
                        3,
                        RecordBody::ToolStarted {
                            op: OpId::from("a"),
                            call_id: ToolCallId::from("c"),
                            name: "x".to_owned(),
                            effective_args: json!({}),
                            replay: ReplaySafety::Safe,
                        },
                    ),
                ],
                CorruptionReason::DuplicateToolStart {
                    op: OpId::from("a"),
                    call_id: ToolCallId::from("c"),
                },
            ),
            (
                vec![
                    record(1, started("a")),
                    entry(
                        2,
                        Some("a"),
                        SessionMessage::ToolResult {
                            call_id: ToolCallId::from("c"),
                            content: Vec::new(),
                            is_error: false,
                            details: None,
                        },
                    ),
                ],
                CorruptionReason::ToolResultWithoutStart {
                    op: OpId::from("a"),
                    call_id: ToolCallId::from("c"),
                },
            ),
            (
                vec![
                    record(1, started("a")),
                    record(
                        2,
                        RecordBody::Step {
                            op: OpId::from("a"),
                            n: 1,
                        },
                    ),
                    record(
                        3,
                        RecordBody::ToolStarted {
                            op: OpId::from("a"),
                            call_id: ToolCallId::from("c"),
                            name: "x".to_owned(),
                            effective_args: json!({}),
                            replay: ReplaySafety::Safe,
                        },
                    ),
                ],
                CorruptionReason::ToolStartedBeforeAssistant {
                    op: OpId::from("a"),
                    call_id: ToolCallId::from("c"),
                },
            ),
            (
                vec![
                    record(1, started("a")),
                    record(
                        2,
                        RecordBody::ToolStarted {
                            op: OpId::from("a"),
                            call_id: ToolCallId::from("c"),
                            name: "x".to_owned(),
                            effective_args: json!({}),
                            replay: ReplaySafety::Safe,
                        },
                    ),
                    record(
                        3,
                        RecordBody::OpFinished {
                            op: OpId::from("a"),
                            outcome: OpOutcome::Aborted,
                        },
                    ),
                ],
                CorruptionReason::FinishedWithOpenTools {
                    op: OpId::from("a"),
                    call_ids: vec![ToolCallId::from("c")],
                },
            ),
            (
                vec![
                    record(1, started("a")),
                    record(
                        2,
                        RecordBody::Step {
                            op: OpId::from("a"),
                            n: 1,
                        },
                    ),
                    record(
                        3,
                        RecordBody::OpFinished {
                            op: OpId::from("a"),
                            outcome: OpOutcome::Completed,
                        },
                    ),
                ],
                CorruptionReason::CompletedWithStreamInFlight {
                    op: OpId::from("a"),
                },
            ),
            (
                vec![record(1, started("a")), record(2, compaction_work("a"))],
                CorruptionReason::IntentMismatch {
                    op: OpId::from("a"),
                    intent: OpIntent::Run,
                },
            ),
            (
                vec![
                    record(1, compaction_started("a")),
                    record(2, compaction_work("a")),
                    record(3, compaction_work("a")),
                ],
                CorruptionReason::DuplicateCompactionStart {
                    op: OpId::from("a"),
                },
            ),
            (
                vec![
                    record(1, compaction_started("a")),
                    record(
                        2,
                        RecordBody::Step {
                            op: OpId::from("a"),
                            n: 1,
                        },
                    ),
                    compaction_entry(3, "a"),
                ],
                CorruptionReason::CompactionWithoutStart {
                    op: OpId::from("a"),
                },
            ),
            (
                vec![
                    record(1, compaction_started("a")),
                    record(
                        2,
                        RecordBody::OpFinished {
                            op: OpId::from("a"),
                            outcome: OpOutcome::Completed,
                        },
                    ),
                ],
                CorruptionReason::CompletedCompactionWithoutCheckpoint {
                    op: OpId::from("a"),
                },
            ),
        ];

        for (items, expected) in cases {
            assert_eq!(
                reduce_lane_status(&items, &LaneName::main()),
                LaneStatus::Corrupt(expected)
            );
        }
    }

    #[test]
    fn new_record_type_is_constructible_without_io() {
        let record = NewRecord {
            lane: LaneName::main(),
            at: Timestamp::from("t"),
            body: started("op"),
        };
        assert!(matches!(record.body, RecordBody::OpStarted { .. }));
    }
}
