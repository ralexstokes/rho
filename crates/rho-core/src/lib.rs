//! Deterministic session state, reducers, context assembly, and commands.
//!
//! This crate is the pure decision-making core. Hosts execute the effects and
//! actions it emits and feed their outcomes back into the state machine.

mod context;
mod machine;
mod model;
mod recovery;

pub use context::{
    AssembledContext, CompactionPlan, ContextError, SessionSettings, assemble_context,
    plan_compaction, unresolved_tool_calls,
};
pub use machine::{
    Action, ActionOutcome, AgentEvent, Effect, EntryStamp, HookInvocation, Input,
    InteractionAnswer, InteractionRequest, MachineConfig, MachineError, PreparedToolCall,
    SessionMachine, Step, ToolSpec,
};
pub use model::{
    CorruptionReason, Entry, EntryBody, EntryId, FORMAT_VERSION, Fact, ForkParent, HostInfo, Item,
    LaneName, LaneStatus, ModelRef, NewEntry, NewFact, NewRecord, OpId, OpIntent, OpOutcome,
    OpenTool, Origin, QueueChange, QueueId, QueueKind, Record, RecordBody, ReplaySafety,
    SessionHeader, SessionId, SessionMessage, SuspendedOp, Timestamp,
};
pub use recovery::reduce_lane_status;
