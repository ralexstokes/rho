# rho — overview & design direction

Status: draft v2 (2026-08-11).

rho is a minimal, extensible coding agent in Rust. It is *inspired by* pi
(`earendil-works/pi`) — we take its good ideas (minimality, a small correctness
core, a clearly-tiered extension surface, durable branching sessions) and none of
its compatibility obligations. No wire/format/API conformance with pi is a goal.
Where pi has both a legacy abstraction and a newer design direction, rho implements
only the newer direction.

## 1. Ideas we keep from pi (and where they came from)

These are design-level takeaways from the pi source survey (see appendix in git
history of this file for the full survey):

1. **Harness, not framework** (pi's stubbed `AgentHarness` target design):
   - all fallible operations return `Result<Outcome, TypedError>` — never throw
     through the loop; hooks are typed and cannot corrupt the event sequence;
   - **manual drive mode**: the caller can single-step the agent through discrete
     actions (`append_entry`, `stream_assistant`, `execute_tool`, `hook`, …) —
     invaluable for tests, debuggers, and deterministic replay;
   - outcomes are explicit: `completed | aborted | failed | suspended`.
2. **Sessions are an append-only entry tree, not a log** — branching/forking is
   appending a child to an older entry; the "current position" is a leaf; plus a
   per-branch **lane record journal** (operation started/finished, step attempts,
   tool started with replay-safety marker) so crash recovery is a pure function of
   the journal: 0 open operations = idle, 1 = suspended/resumable, 2+ = corruption.
3. **Compaction as first-class session entries**, including the split-turn idea
   (separate summaries for old history vs the prefix of a turn that got cut), and
   compaction summary requests isolated from the main prompt cache.
4. **Steering and follow-up queues** feeding the turn loop (steer = inject while
   running, follow-up = next run), with pre-poll before the first turn.
5. **Tool-execution semantics that encode hard-won lessons**:
   - `stopReason == length` ⇒ fail every tool call in the batch without executing
     (streamed args may be silently truncated yet parse);
   - parallel execution with *serialized preparation* (permission prompts must not
     interleave); one sequential tool downgrades the whole batch;
   - a batch terminates early only if *every* result requests termination;
   - cooperative cancellation threaded to every tool.
6. **The two-tier extension split**: hooks/registrations whose payloads are plain
   data can cross a serialized boundary; anything touching live UI objects cannot.
   rho only ever offers tier 1 across the ABI; tier 2 is native Rust.
7. **Protocol rule for frontends**: snapshots are authoritative; streaming deltas
   are transient UI hints and must never be reduced into authoritative state.
8. **Skills and prompt templates as pure data** (agentskills.io SKILL.md standard;
   markdown templates with `$1/$@` substitution) — extensibility without code.
9. **Opaque provider state stays opaque**: thinking signatures / reasoning replay
   blobs are provider-owned strings attached to messages; only the provider adapter
   that produced them may interpret them. Cross-provider handoff = drop them.

Explicitly dropped: pi conformance/differential testing, session-file compatibility,
pi's RPC protocol, the legacy `Agent`/`agentLoop` semantics, jiti/npm extension
loading, the TypeBox/JSON-coercion machinery, hand-rolled provider adapters and the
regex error-classification lore (backends own that now), the Node sidecar host idea
(only existed for pi ecosystem compat), themes/keybindings/TUI details (deferred).

## 2. Architecture

**Division of labor with shelterwood** (`ralexstokes/shelterwood`, the structured
supervision / actor runtime):

> rho owns *one agent's* lifecycle — the turn loop, context, provider I/O, tools,
> and durable session state. shelterwood owns *many agents'* lifecycles —
> supervision, restart, startup ordering, mailboxes, and teardown. rho never
> grows its own orchestration (no sub-agent spawner, no process supervision, no
> multi-agent coordination); multi-agent systems are shelterwood trees whose
> actors embed rho sessions.

That makes rho **library-first**: the primary artifact is `rho-agent` as an
embeddable, host-agnostic core. Hosts wrap it:

```
   hosts   ┌────────────────────┐  ┌───────────────────────────┐
           │ rho-cli / rho-rpc  │  │ shelterwood actor host    │
           │ (stdio/socket,     │  │ (rho-shelterwood: session │
           │  one-shot modes)   │  │  actor per rho session)   │
           └─────────┬──────────┘  └────────────┬──────────────┘
                     └──────────┬───────────────┘
                    ┌───────────▼───────────────────┐
                    │           rho-agent           │  harness: session tree,
                    │  loop · lanes · compaction ·  │  lanes, hooks, queues,
                    │  hooks · steering/follow-up   │  manual drive
                    └───┬──────────────┬────────────┘
                        │              │
            ┌───────────▼───┐   ┌──────▼──────────────────┐
            │    rho-ai     │   │  rho-tools              │
            │ Provider +    │   │ fs/shell built-ins,     │
            │ Factory traits│   │ MCP client              │
            │ + bound. types│   └─────────────────────────┘
            └───┬───────┬───┘
                │       │
        nanocodex     anthropic adapter      ← backends: existing crate for
        (openai)      (small, hand-rolled)     openai; thin stub for anthropic
                    ┌───────────────────────────────┐
                    │           rho-ext             │  WASM component host
                    │  WIT-defined tier-1 ABI       │  (wasmtime)
                    └───────────────────────────────┘
```

pi's remote-session stack (CBOR protocol/client/server/leases) stays out of
scope — it layers on the RPC host later without redesign.

### 2.1 `rho-ai` — thin, extensible provider boundary

Very small on purpose. It defines the boundary types and one trait; it does **not**
implement providers. Backends are existing crates wrapped in adapters:

- **OpenAI**: `nanocodex` (gakonst) — targets the Responses protocol with
  WebSocket transport, auth, MCP, compaction, reconnect replay.
- **Anthropic**: a small hand-rolled adapter — the Messages API is plain HTTP +
  SSE and a minimal streaming adapter (text/thinking/tool_use blocks, stop
  reasons, usage) is a few hundred lines. Enough to get up and running; a
  framework crate (rig or similar) can be swapped in behind the same trait later
  if we ever want its long tail of providers.

Boundary sketch:

```rust
// stable rho types — the only vocabulary rho-agent speaks
enum ContentBlock { Text(..), Thinking { text, opaque: Option<OpaqueBlob> },
                    Image(..), ToolCall { id, name, args: serde_json::Value } }
struct AssistantMessage { blocks: Vec<ContentBlock>, stop: StopReason,
                          usage: Usage, provider: ProviderId, model: ModelId }
enum StreamEvent { Start, Delta { index, kind, delta }, BlockDone(..),
                   Done(AssistantMessage), Error(ProviderError) }

trait ProviderFactory {   // shared: credentials, model catalog, opening sessions
    fn models(&self) -> &[ModelInfo];
    async fn open(&self, config: SessionConfig) -> Result<Box<dyn Provider>, ProviderError>;
}

trait Provider {          // one live logical model session, owned by a rho session
    fn generate(&mut self, req: Request, cancel: CancellationToken)
        -> impl Stream<Item = StreamEvent> + Send + '_;
}
```

Design rules:
- `OpaqueBlob` (provider id + bytes/string) carries reasoning signatures etc.;
  adapters serialize/restore it; rho-agent never inspects it; handoff to a
  different provider drops it.
- Retry/backoff and error classification live behind the trait — backends already
  do this; rho-ai only defines `ProviderError { retryable, kind, message }`.
- Custom providers = implement the trait (native) — and later, possibly over the
  extension ABI if a real need appears. Don't build a model-catalog subsystem;
  a static table + config file entry per model is enough.

**Decided** (details in spec/01-rho-ai.md): the boundary is
**transcript-authoritative provider sessions** (revised 2026-08-11,
superseding the earlier stateless-first shape after the phase-1 spike). A
`Provider` is one live logical model session opened by a shared
`ProviderFactory`; rho-agent still passes the full authoritative transcript on
every generation, and the adapter either continues its native session (prefix
match) or **rebases** from the transcript (restart, branch, error, doubt).
Provider checkpoints / external session ids are acceleration only — the rho
transcript and journal remain the sole durable truth, so durability,
branching, and deterministic replay are unchanged, and "always rebase" is the
degenerate stateless implementation. A failed or cancelled generation poisons
the native continuation (the next generation rebases); tool execution stays in
rho; model or incompatible config change = reopen via the factory. Validation
is **langsec-style**: parse-or-reject at the adapter boundary, no coercion in
the harness — provider quirks are handled by per-provider tool conformance
suites, not harness code. Auth v1 = env vars + credentials file behind one
trait, consumed by the factory. Retry has exactly one owner (loop-level
policy; adapters only classify). The nanocodex-layer question is settled by
the phase-1 spike: the standalone `OpenAi -> Session -> Response` layer,
quarantined in `rho-ai-openai`.

### 2.2 `rho-agent` — the harness (the real correctness core)

Implements the pi ideas 1–5 above directly, skipping pi's legacy loop:

- **Session store**: `SessionRepo`/`SessionTree` traits; entries = append-only
  tree with parent links; lanes carry operation journals. Backends: in-memory and
  JSONL first; SQLite later (WAL + writer leases + FTS is a good pattern to reuse).
  Ship a **conformance test suite as part of the crate** (pi does this and it's
  excellent — any backend passes one shared suite).
- **Turn loop**: steering pre-poll → stream assistant → execute tool batch →
  emit turn end → prepare next turn (model/context swap point) → poll queues.
  Events mirror the loop 1:1 (`agent_/turn_/message_/tool_execution_*`), sinks
  are awaited (backpressure is the listener's choice).
- **Hooks** (typed trait, all `Result`): transform-context, before/after request,
  before/after tool, before compaction/navigation, run lifecycle. This one hook
  surface serves native embedders *and* the extension ABI.
- **Compaction** (v1 scope DECIDED): simple summarize-and-truncate at a token
  threshold, cut-point selection that never orphans a tool call. The entry
  format carries retained-tail checkpoints from day one so upgrading to pi-style
  split-turn compaction later is non-breaking.
- **Manual drive** for tests/debuggers: `peek_action()` / `execute_action()`.
  **Action granularity DECIDED — message-level**: an action like
  `StreamAssistant` completes with the final message as its journaled result;
  streaming deltas flow on an advisory event channel and are never state. The
  state machine is thus a pure function of (entries, action results) →
  **deterministic golden-session replay** is the primary regression-testing
  strategy (replacing the dropped pi-conformance testing).

The embedding requirement (§2.6) hardens a design choice that was previously just
nice-to-have: the loop core is an **explicitly driven state machine** (the manual
drive API), and the "automatic" mode is a thin driver that loops it on tokio.
Hosts choose: hand rho a task to run, or own the loop themselves. No global
state, no singletons, no ambient runtime assumptions — many agents per process
is the normal case, not a special one.

### 2.3 `rho-tools`

The v1 default is pi's compact coding quartet: `read`, `write`, `edit`, and
`bash`. `grep`, `find`, and `ls` remain candidates only if shell composition is
not sufficient in practice. The built-ins adopt pi's important failure
semantics: edit does exact-then-normalized matching (Unicode quote/dash/space
and trailing-whitespace normalization) with uniqueness and overlap errors;
per-file mutations serialize on a canonical path while different files remain
parallel; edits preserve BOMs and line endings; and read/bash output has bounded
head/tail truncation with explicit markers. Relative paths resolve from the
configured working directory and absolute paths remain available.

**MCP is the first extensibility mechanism** — it's the ecosystem standard for
tools and both nanocodex and rig already speak it. Tool extensibility should not
wait for the WASM ABI.

**MCP v1 scope (DECIDED)**: client-launched stdio servers, using the current
`2026-07-28` stateless protocol with the specification's discovery probe and
fallback to the initialization-based `2025-11-25` through `2024-11-05`
revisions. Every configured server has a short stable name; remote tools are
exposed through the provider-portable `mcp__<server>__<tool>` namespace (MCP
dots normalize to underscores, and normalization collisions fail the whole
connection). Server declarations and annotations are untrusted: schemas are
validated at connection time, task-required tools are rejected until rho has
that extension, and every MCP tool is `ReplaySafety::Never`.

The first client deliberately snapshots all paginated tools at process start.
It ignores list-change notifications and does not restart a failed process;
reconnect is the refresh/recovery operation. Requests have deadlines and emit
MCP cancellation on timeout or caller abort. Text and images cross the native
content boundary, resource blocks get model-visible text representations,
unsupported audio is identified but omitted, and structured content is retained
as journal details. Server stderr is drained continuously and retained as an
8 KiB diagnostic tail. stdio servers inherit the host environment plus explicit
overrides, so their containment and credential scope are operator decisions.

**Safety stance (v1, DECIDED)**: no built-in tool gating — like pi, rho v1
delegates safety to the execution environment (containers, sandboxes, VMs) and
documents that posture plainly. The `before_tool` hook and the RPC interaction
request/answer primitive exist from the start, so a permission-policy layer can
ship later as a first-party extension (a good minimality test for the ABI)
without touching core. Until then, unattended operation is explicitly at the
operator's risk.

### 2.4 `rho-rpc` — the RPC core

Our own protocol, informed by pi's but not compatible. v1 choices (bias: simple,
debuggable, evolvable):
- JSON Lines over stdio and unix socket. (CBOR/framing can come with the remote
  stack later; don't optimize bytes now.)
- Every frame carries `v: 1`. Requests: `{v,id,method,params}`; responses
  `{v,id,ok,result|error}`; events `{v,event,data}`. Methods cover session
  lifecycle (create/open/list/fork/delete),
  prompt/steer/abort, get_snapshot, set_model/thinking, tool-permission answers,
  extension pass-throughs.
- **Snapshot-authoritative rule** (idea 7): `get_snapshot` and snapshot events
  carry full truth; `delta` events are advisory for rendering only.
- Client-interaction inversion: permission prompts / extension dialogs surface as
  server→client requests with ids (client answers or times out) — this is what
  keeps the core headless.

### 2.5 `rho-ext` — extension ABI

- **WASM component model** (wasmtime + WIT) as the one serialized ABI. Tier-1
  surface only: lifecycle/turn/message/tool hooks, tool + command + flag
  registration, session entry append/read, message sending, and a small dialog
  API (select/confirm/input/notify, status line, string-list widget) that RPC
  clients render.
- **Capability-scoped, not sandbox-pure**: extensions declare capabilities
  (exec, fs paths, net) granted at install/trust time; hosts expose them as
  imports. Sandboxing is a policy default, not an inescapable property — coding
  agents legitimately need host access.
- TS/JS authors compile to components (componentize-js/jco); Rust/Go/Python also
  land free. No embedded V8/Node anywhere.
- Native Rust plugins = just implementing rho-agent hook traits in-process
  (for embedders); no dylib story until someone needs it.
- Skills + prompt templates ship early (pure data, big leverage, no ABI needed).

### 2.6 Embedding contract & shelterwood integration

The seam between rho and shelterwood is a **session-actor message protocol**,
and it deliberately projects the same vocabulary the RPC protocol uses:

- **Commands in** (mailbox): `Prompt`, `Steer`, `FollowUp`, `Abort`,
  `SetModel/SetThinking`, `AnswerInteraction { id, .. }`, `Compact`, `Fork`, …
- **Events out** (to subscribers / parent): the agent event enum plus
  `InteractionRequest { id, .. }` for permission prompts and dialogs. All
  wire messages are owned serde data. The host translates durable-operation
  requests into actor commands carrying `shelterwood::Reply<T>` to get its
  `CallError` taxonomy (accepted-vs-answered-vs-crashed) and incarnation
  fencing — the key primitives for at-most-once command delivery.
- **Cancellation**: shelterwood ships its own observe-only `CancellationToken`
  (engine-fired). rho keeps its own internal cancel for *user aborts*; the
  actor maps shutdown-token-fired → rho cancel → loop reaches a clean point →
  lane journal records the outcome.
- **Turns use the ordinary automatic driver** inside the actor callback.
  `Steer`/`FollowUp`/`Abort` and interaction answers stay live through rho's
  separate control channel, avoiding a second manual-driver implementation.

The implemented topology, restart contract, and deliberate limits are in
**spec/05-embedding.md**.

**The restart/resume synergy (design for this on purpose).** rho sessions are
durable and the lane journal makes interruption a first-class, *inspectable*
state: on open, 0 open operations = idle, 1 = suspended/resumable. That is
exactly the contract a supervisor restart wants:

> shelterwood restarts a failed session actor (OneForOne) → the new incarnation
> opens the same session id → finds a suspended operation in the journal →
> `resume()` continues the run (replay-safe tools re-run; unsafe ones surface as
> failed results). Crash recovery composes from supervision + durability without
> any shared in-memory state between incarnations.

This means: treat the journal not as an internal detail but as the recovery API;
`resume()` is a phase-2 requirement, not a nice-to-have; and shelterwood's
incarnation identity can be recorded in the journal for observability.

**Sub-agents are an orchestration concern.** A "spawn sub-agent" tool in rho is
just a tool whose implementation asks its host to add a child session actor to a
dynamic scope and await its result — provided by the shelterwood host layer
(or any embedder), not by rho-agent. rho standardizes only the session-actor
protocol so such tools have a stable thing to talk to.

`rho-shelterwood` (integration crate, kept out of rho-agent so the core stays
runtime-light) provides the fixed-root/dynamic-session tree and exact
membership handles. The RPC host supplies the rho-specific restartable session
actor and maps Shelterwood shutdown into rho's control path. The byte listener
remains the outer connection boundary, while every durable session is
supervised and rehydrates from its journal. This dogfoods restart/fencing where
it adds recovery value without creating a second RPC request loop.

## 3. Phasing

1. **`rho-ai` + walking skeleton**: boundary types, `ProviderFactory` +
   `Provider` traits, nanocodex adapter, hand-rolled anthropic adapter; a
   `faux` provider for tests; minimal binary that runs one turn with a bash
   tool.
2. **`rho-agent`**: session tree + lanes + JSONL/memory backends + conformance
   suite; turn loop as driven state machine + automatic driver; queues + hooks;
   compaction; `resume()` from the journal (§2.6 makes this core, not optional).
3. **`rho-tools` + hosts**: built-in tools, MCP client; `rho-shelterwood`
   session actor + the RPC host built on it; print/JSON one-shot modes.
   **Milestone: a usable headless coding agent — scriptable over RPC, and
   supervisable/restartable as a shelterwood actor.**
4. **`rho-ext`**: WIT ABI + wasmtime host + capability grants; skills/templates.
5. **Later, in any order**: TUI (client of rho-rpc), remote-session stack
   (multi-client, leases — layers on the same RPC seam), SQLite backend,
   multi-agent patterns on shelterwood (fleets, orchestrator-with-subagent-tool),
   telemetry polish.

## 4. Decision log & open questions

Decided (details in the linked specs):

- rho-ai: transcript-authoritative provider sessions — `ProviderFactory` +
  live `Provider`, full transcript on every generation, rebase on doubt,
  poison-on-ambiguity (revised 2026-08-11 from stateless-first; the phase-1
  spike settled the nanocodex layer); langsec parse-or-reject validation
  with per-provider conformance suites; env/file credentials behind a trait
  consumed by the factory; single-owner retry. → spec/01-rho-ai.md
- Loop actions are message-level; deterministic golden-session replay is the
  regression-testing strategy. (§2.2)
- Compaction v1: simple summarize-and-truncate; entry format retained-tail
  ready. (§2.2)
- Safety v1: no built-in tool gating (pi's stance); hook + interaction
  primitives present for a later policy extension. (§2.3)
- Config v1: **user-level only** (`~/.rho`); project-local `.rho/` config,
  skills, and templates arrive together with the extension ABI and its trust
  design — they share a threat model. AGENTS.md-style context files are
  content, not config, and load from the project regardless.
- Session-actor protocol ownership + tool `replay: safe | never` markers.
  → spec/05-embedding.md (phase-3 embedding implemented)
- Observability: `tracing` with stable span names; no telemetry subsystem.
- **Implementation shape**: modular crates split along a pure-core /
  mutable-shell boundary; the session machine emits Effects + Actions, shells
  are dumb driver loops, replay is effect-diff verification. Crate layout,
  dependency rules, and CI enforcement → spec/06-implementation.md. (Refines
  the crate sketch in this doc: the pure machine lives in `rho-core`;
  `rho-agent` is the driver shell.)
- Partial tool-arg parsing is a client concern; harness forwards raw deltas
  only (corollary of langsec decision, spec/01 §1.2).

Open:

1. RPC protocol doc — **written**. It is a thin versioned projection of
   rho-agent's command/event enums, with explicit response-vs-completion
   semantics, snapshot authority, and server→client interaction requests.
   → spec/03-rpc.md
2. WIT ABI — **deferred until after the phase-3 milestone** (no-gating
   decision removed the only v1 consumer; project-config trust already lands
   with it; toolchain maturity is a phase-4 risk). Binding phase-2 design
   rule that makes deferral safe: **hook payloads/results are plain owned
   serde data — no handles, borrows, or callbacks — async request/response**;
   hooks are journaled actions, so ABI extensions inherit deterministic
   replay. → spec/04-ext-abi.md (unwritten)
3. Session entry + lane journal schema — **drafted** → spec/02-session.md
   (its §10 tracks remaining sub-questions: fsync default, queued-message
   payloads, compaction-as-operation, export format).
4. Thinking-level / model-switch mid-session semantics. The provider side is
   now settled (spec/01 §1.1: switch = reopen via the factory); still open is
   the loop side — keep pi's per-turn swap point?
5. MCP config location and supervision — **settled for v1**: MCP server entries
   live in the user-level host config; namespacing/collision and fixed-snapshot
   behavior are specified in §2.3. The Tokio stdio client owns child lifetime;
   automatic restart and Shelterwood supervision are deferred until runtime
   evidence warrants them.
6. Workspace/naming: binary `rho`, crate prefix `rho-` — confirm and scaffold.
