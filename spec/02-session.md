# rho session model — entries, lanes, and the operation journal

Status: draft (2026-08-11). Depends on decisions in 00-overview §4 and
05-embedding §8: message-level actions, deterministic replay, journal writes at
action boundaries, `replay: safe | never` tool markers, retained-tail-ready
compaction, first-class open-operation inspection.

## 1. Goals / non-goals

Goals:
- **Durability as the recovery API**: a crashed process, restarted actor, or
  new machine reconstructs everything from the session file alone. "Suspended
  mid-run" is an inspectable state, not an error.
- **Branching**: the transcript is an append-only tree; going back and trying
  again is appending, never rewriting.
- **Deterministic replay**: the agent state machine is a pure function of
  (entries, journaled action results) — golden-session tests replay files.
- **One schema, multiple backends**: memory, JSONL (v1), SQLite (later), all
  passing one conformance suite.

Non-goals (v1): multi-writer sessions, cross-session references, remote
backends, search, navigation UI semantics (the *format* supports moving a leaf;
UX comes later).

## 2. The two-plane model

A session contains two kinds of durable data with different roles:

- **Entries** (the *transcript plane*): the append-only tree of content —
  messages, compaction checkpoints, settings changes. Entries are what LLM
  context is built from. Entries are never mutated or deleted.
- **Records** (the *journal plane*): the per-lane operation log — run
  started/finished, step attempts, tool starts, queue changes, usage. Records
  are how in-flight work is recovered, audited, and accounted. Records are
  append-only too, but they are *about* execution, not content.

A **lane** is a named line of execution: a leaf pointer into the entry tree
plus its record log. v1 has exactly one lane per session (`"main"`); the
format carries lane names so sub-lanes (concurrent operations on branches of
the same session) can arrive without a format break.

**Facts** are a small third plane: session-scoped key/value state outside the
tree (display name, labels). Last-writer-wins, not part of LLM context.

## 3. Identity

- `SessionId`, `EntryId`: UUIDv7 (time-ordered, no coordination, safe across
  hosts). Storage additionally stamps every appended item with a per-session
  monotonic `seq: u64` — the total order used for recovery and log streaming.
- `OpId`: UUIDv7 minted at `op_started`. **Every entry appended during an
  operation carries its `op`** (absent on entries appended while idle, e.g. an
  imported transcript). This scopes recovery: "which tool calls in *this*
  operation lack results" is a pure scan.
- `ToolCallId`: provider-issued, unique within an operation.

## 4. Wire format (JSONL backend)

One file per session: `<dir>/<uuidv7>.jsonl`. One JSON object per line, LF
terminated, appended only. The single interleaved stream is deliberate: it
gives entries and records one total order (`seq`), which recovery depends on,
for free.

```jsonl
{"t":"session","v":1,"id":"01912ab…","created_at":"2026-08-11T20:04:11Z","cwd":"/home/x/proj","parent":null}
{"t":"entry","seq":1,"id":"01912ab…","parent":null,"lane":"main","op":"01912ac…","at":"…","body":{"kind":"message","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}}
{"t":"record","seq":2,"lane":"main","at":"…","body":{"kind":"op_started","op":"01912ac…","intent":"run","origin":"external","host":{"incarnation":"3"}}}
{"t":"record","seq":3,"lane":"main","at":"…","body":{"kind":"step","op":"01912ac…","n":1}}
{"t":"entry","seq":4,"id":"…","parent":"…","lane":"main","op":"01912ac…","at":"…","body":{"kind":"message","message":{"role":"assistant","…":"…"}}}
{"t":"record","seq":5,"lane":"main","at":"…","body":{"kind":"op_finished","op":"01912ac…","outcome":"completed"}}
{"t":"fact","seq":6,"at":"…","key":"name","value":"fix flaky test"}
```

Rules:
- Header line first; `v` is the format version; **unknown `v` ⇒ refuse to
  open** (langsec: no best-effort reading of future formats).
- Torn tail line (crash mid-append) ⇒ truncate to last complete line on open;
  everything before it is valid by construction.
- Flush on every append; fsync at operation boundaries (`op_started`,
  `op_finished`) by default, per-append fsync configurable.
- Single writer: `open()` takes an exclusive advisory lock (lock file beside
  the session file); readers may snapshot without the lock.

## 5. Entry schema

```rust
struct Entry {
    id: EntryId,
    parent: Option<EntryId>,   // None = root; tree structure
    lane: LaneName,
    op: Option<OpId>,
    at: Timestamp,
    body: EntryBody,
}

enum EntryBody {
    Message { message: Message },
    Compaction {
        summary: String,
        first_kept: Option<EntryId>,   // back-pointer form
        retained_tail: Vec<Message>,   // self-contained checkpoint form
        tokens_before: u64,
        usage: Usage,
    },
    SettingsChange { model: Option<ModelRef>, thinking: Option<ThinkingLevel> },
    Custom { tag: String, data: JsonValue },   // extension state, NOT in LLM context
}

enum Message {
    User { content: Vec<UserContent> },
    Assistant(AssistantMessage),               // rho-ai type, verbatim (incl. opaque blobs)
    ToolResult { call_id: ToolCallId, content: Vec<ToolContent>,
                 is_error: bool, details: Option<JsonValue> },
    Custom { tag: String, content: Vec<UserContent> },  // extension-injected, IS in context
}
```

Notes:
- The assistant `Message` entry **is** the journaled result of a
  `StreamAssistant` action; there is no separate result record (message-level
  actions decision). Same for tool results.
- `Custom` appears twice on purpose (pi's lesson): opaque extension *state*
  (`EntryBody::Custom`) vs extension *context* (`Message::Custom`).
- Compaction carries `retained_tail` from day one (v1 writes it; the
  `first_kept` back-pointer is allowed for space-conscious backends). A
  compaction entry is a **self-contained checkpoint**: context building never
  needs to walk past the newest compaction on the path.

### Context assembly (pure function, lives in rho-agent, not storage)

Walk root→leaf along parent links for the lane's leaf. If the path contains
compaction entries, start from the newest one: context = summary (rendered as
a user message) + `retained_tail` (or entries from `first_kept`) + everything
after it. `SettingsChange` entries determine current model/thinking;
`EntryBody::Custom` is skipped.

## 6. Record schema

```rust
struct Record { seq: u64, lane: LaneName, at: Timestamp, body: RecordBody }

enum RecordBody {
    OpStarted   { op: OpId, intent: OpIntent, origin: Origin,
                  host: Option<HostInfo> },      // incarnation etc., opaque to core
    OpFinished  { op: OpId, outcome: OpOutcome },// completed|aborted|failed{error}
    AbortRequested { op: OpId },
    Step        { op: OpId, n: u32 },            // one per StreamAssistant attempt
    ToolStarted { op: OpId, call_id: ToolCallId, name: String,
                  effective_args: JsonValue,     // post-hook args actually executed
                  replay: ReplaySafety },        // Safe | Never (required)
    HookStarted { op: OpId, n: u32, invocation: HookInvocation },
    HookFinished{ op: OpId, n: u32, result: Result<HookOutput, String> },
    InteractionRequested { op: OpId, hook: u32, request: InteractionRequest },
    InteractionAnswered  { op: OpId, hook: u32, request_id: String,
                           answer: InteractionAnswer },
    QueueChanged{ op: Option<OpId>, change: Enqueued { id, kind: Steer|FollowUp,
                  message: Message } | Cancelled { id } },
    Usage       { op: OpId, usage: Usage },
    LaneMoved   { to: EntryId },                 // leaf navigation (fork/undo later)
}

enum OpIntent  { Run, Compaction }               // v1; extensible
enum Origin    { External, Replay }              // provenance (05-embedding hazard)
```

Notes:
- `ToolStarted.effective_args` records what actually ran (after any hook
  mutation) — the audit record, and what a `Safe` replay re-executes.
- `HookStarted` is synced before extension code runs. `HookFinished` stores the
  exact owned result. Hook action numbers are consecutive within an operation;
  impossible hook/interaction sequences are typed corruption.
- A hook may return `Interact { request }` instead of completing. The request is
  recorded before it reaches a client, the answer is recorded before the hook
  resumes, and only the hook's eventual terminal result produces
  `HookFinished`. A hook may ask more than once; recovery folds its ordered,
  durable request/answer chain back into the original invocation.
- A tool call is **closed** by a `ToolResult` entry with the same `call_id`
  and `op`. No separate tool-finished record; the entry is the closure.
- `Step` exists for retry accounting and so recovery can distinguish "stream
  in flight" from "stream never started".
- Queued steer/follow-up messages are journaled (`QueueChanged`) so a
  supervised restart preserves user input verbatim; draining is visible as the
  injected `Message` entries. Privacy consequence: a *cancelled* steer persists
  in the records plane despite never entering the transcript — **any export or
  sharing path MUST strip records** (entries-only export), not just for size.

## 7. The run choreography (what gets written when)

Action-boundary journaling (05-embedding §8.2), for one prompt:

```
recv Prompt            → entry  Message::User                (op stamped)
                       → record OpStarted { intent: Run, origin }
loop:
  action StreamAssistant:
                       → record Step { n }
    …stream runs (deltas advisory, never durable)…
    done               → entry  Message::Assistant           ← the action result
  if tool calls:
    for each call      → record ToolStarted { call_id, effective_args, replay }
    …execute (parallel)…
    each result        → entry  Message::ToolResult          ← closes the call
  if queues drained    → entry  Message::User (steer)        (op stamped)
finish                 → record OpFinished { outcome }
```

Invariant: **at most one open operation per lane** at any time. All appends
for a lane go through the lane's single writer (the session actor / driver),
so no interleaving hazards within a lane.

## 8. Recovery (first-class, cheap)

`open()` returns lane status without loading full content:

```rust
enum LaneStatus {
    Idle,                                  // 0 open ops
    Suspended(SuspendedOp),                // 1 open op
    Corrupt(CorruptionReason),             // anything else / impossible sequence
}
struct SuspendedOp {
    op: OpId, intent: OpIntent,
    abort_requested: bool,
    last_step: Option<u32>,
    open_tools: Vec<OpenTool>,             // ToolStarted without ToolResult entry
    stream_in_flight: bool,                // Step without following Assistant entry
    last_hook: u32,
    hook: Option<SuspendedHook>,            // result and nested interaction, if any
}
```

Resume semantics (executed by rho-agent, not storage):
1. `abort_requested == true` → finalize as aborted: synthesize error
   `ToolResult` entries for open tools, append `OpFinished { aborted }`.
2. Open tools: `replay: Safe` → re-execute from `effective_args`;
   `replay: Never` → synthesize a failed `ToolResult` ("interrupted; not safe
   to re-run") and let the model react.
3. `stream_in_flight` → re-issue `StreamAssistant` (record a new `Step`).
   Always replay-safe: the new incarnation's provider session rebases from
   the transcript (spec/01 §1.1), so nothing provider-side is resumed or
   double-appended.
4. Resume-injected work runs with `origin: Replay` so journaling middleware
   can distinguish it (the double-execution hazard from shelterwood's spec).
5. An unanswered interaction is durably resolved as `TimedOut` and its hook is
   resumed with that answer. It is never silently re-presented after restart.
   A hook interrupted outside an interaction is re-invoked with
   `origin: Replay`; a durable `HookFinished` result is applied without calling
   the hook again.
6. Recovery derives a missing post-request, post-tool, pre-compaction, or
   run-finished hook when the preceding entry/record is durable but the
   corresponding `HookStarted` is not. An abort closes an open hook (and any
   unanswered interaction) before the terminal record, without replaying the
   interrupted extension.

Hook points are typed and enabled explicitly in `MachineConfig`: user-run
start/end, context transform, before/after request, before/after tool, and
before compaction. Compaction is a separate maintenance operation and does not
fire the user-run lifecycle pair. One `rho-agent::HookHost` receives owned serde
invocations and may multiplex any number of native or serialized extensions.
Transform results are parsed at the core boundary. In particular, `before_tool`
cannot change tool identity or replay policy; its argument mutation is
schema-revalidated before the post-hook `ToolStarted.effective_args` record is
synced.

`CorruptionReason` is a closed enum of impossible sequences (two open ops,
record after finish, non-consecutive step, tool result without start, unknown
op reference, …) — modeled on pi's reducer, which proved the value of naming
each case. Corruption is a typed error, never a panic, and never "best-effort
repaired" (langsec).

## 9. Traits

```rust
trait SessionRepo {
    async fn create(&self, opts: CreateOptions) -> Result<Session, SessionError>;
    async fn open(&self, id: SessionId) -> Result<Session, SessionError>;   // writer lock
    async fn list(&self) -> Result<Vec<SessionMeta>, SessionError>;
    async fn delete(&self, id: SessionId) -> Result<(), SessionError>;
    async fn fork(&self, src: SessionId, at: ForkPoint) -> Result<Session, SessionError>;
}

trait Session {  // one open handle = the lane writer
    fn append_entry(&mut self, body: EntryBody, op: Option<OpId>) -> Result<EntryId>;
    fn append_record(&mut self, body: RecordBody) -> Result<u64 /*seq*/>;
    fn leaf(&self) -> Option<EntryId>;
    fn move_leaf(&mut self, to: EntryId) -> Result<()>;         // writes LaneMoved
    fn branch(&self, from: Option<EntryId>) -> Result<Vec<Entry>>;  // root→leaf path
    fn lane_status(&self) -> Result<LaneStatus>;                // §8, cheap
    fn get_fact(&self, key: &str) -> Option<JsonValue>;
    fn set_fact(&mut self, key: &str, value: JsonValue) -> Result<()>;
    fn log(&self, after_seq: u64, limit: usize) -> Result<Vec<Item>>; // raw stream
}
```

- `fork` **copies** the root→fork-point path into a new, self-contained file
  (header records `parent: {session, entry}` for lineage). No cross-file
  references — every session file stands alone. Tree-scope fork = full copy.
  **Copied entries preserve their `EntryId`s**: fork mints a new `SessionId`
  but never re-mints entry ids, so lineage is traceable entry-by-entry and a
  future content-addressed / reference-fork optimization (if fan-out over
  large shared prefixes becomes a real workload) needs no migration.
- Backends: `memory`, `jsonl` in `rho-agent`; `sqlite` later (pi's table
  shape — entries/records/facts/branch-cache/writer-leases — ports cleanly
  onto this schema).
- **The conformance suite ships in `rho-agent`** from day one and is the
  contract: append/read-back identity, tree navigation, torn-tail recovery,
  lane-status truth table (idle/suspended/each corruption reason), fork
  lineage + id preservation, fact LWW, lock exclusivity, record-stripping
  export. **Golden `.jsonl` fixtures are added only when format `v: 1` is
  stamped stable** — while the schema is still moving, goldens degrade into
  regenerate-on-every-tweak noise; until the stamp, serialization is covered
  by property-based round-trip tests instead.

## 10. Open questions

1. Fsync policy default: op-boundary (proposed) vs every-append — measure cost
   on real sessions before hardening.
2. Should `QueueChanged.Enqueued` carry full `Message` payloads (proposed, so
   restarts preserve queued steering verbatim) or ids into a side store?
3. Compaction summarization *requests*: do they run as their own `OpIntent::
   Compaction` operation (proposed — they're interruptible work too) with the
   summary-model call journaled as a `Step`?
4. Export format for sharing — later, but one property is already fixed:
   exports are **entries-only** (records MUST be stripped; see §6 privacy
   note).
