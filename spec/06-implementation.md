# rho implementation guidance — crates, and the pure core / mutable shell

Status: draft (2026-08-11). This document is normative for how rho is built:
workspace structure, the core/shell discipline, and where the pattern lands in
each spec'd phase.

## 1. The pattern, stated precisely

Every subsystem is split into:

- a **pure core**: deterministic, IO-free logic. No filesystem, network,
  clock, randomness, environment, or async. Given the same inputs it produces
  the same outputs, always. Cores are plain data + functions; they can be
  tested exhaustively, property-tested, and replayed.
- a **mutable shell**: the thin imperative layer that owns real resources
  (sockets, files, subprocesses, clocks, id minting, tokio), calls into the
  core, executes what the core asks for, and feeds results back.

"Pure/immutable" here means **deterministic and effect-free**, not persistent
data-structure dogma: core reducers take state by value (or `&mut`) and return
new state + outputs; plain owned Rust data throughout. What is banned in core
is *observing the world*, not mutation of local values.

The core never calls the shell. The shell is not clever. All decisions live in
the core; all effects live in the shell. Anything hard to test belongs to the
shell and must therefore be too simple to break.

This is not a new decision — it is the generalization of decisions already
made: message-level actions, deterministic golden-session replay, hooks as
owned serde data, langsec parse-at-the-boundary, and the driven state machine
the shelterwood embedding requires. This doc makes it the rule everywhere.

## 2. The central instance: the session machine

The phase-2 loop is the pattern's anchor, and its shape constrains everything
else. The core is a value; each step returns *instructions*:

```rust
// rho-core — pure
pub struct SessionMachine { /* plain data: context, phase, pending, config */ }

pub enum Step {
    Do { effects: Vec<Effect>, action: Option<Action> },
    AwaitingOutcome,          // an action is in flight
    Idle,                     // run complete, queues empty
}

pub enum Effect {             // deterministic outputs — journal writes, events
    AppendEntry(EntryBody /* + op stamp */),
    AppendRecord(RecordBody),
    Emit(AgentEvent),
}

pub enum Action {             // requests for the shell — the only IO
    StreamAssistant { request: Request },
    ExecuteTool { call: PreparedToolCall },
    Summarize { request: Request },          // compaction's LLM call
    AwaitInteraction { request: InteractionRequest },
    InvokeHook { hook: HookInvocation },
}

impl SessionMachine {
    pub fn handle(self, input: Input) -> (Self, Step);       // command arrives
    pub fn resolve(self, outcome: ActionOutcome) -> (Self, Step); // action done
}
```

The shell (a tokio driver, a shelterwood actor, a test harness) is a loop:
perform `effects` (append to the journal, publish events), execute `action`
(the one place IO happens), feed the `ActionOutcome` back. Nothing else.

Two properties fall out and both are load-bearing:

1. **Replay is verification, not simulation.** Because *effects are derived by
   the core*, replaying a session file means: feed the recorded outcomes back
   through the machine and assert the derived effects equal the file's actual
   entries/records, byte for byte. A golden-session test is a diff. Drift
   between code and journal is caught structurally.
2. **Every host is trivial.** The tokio driver, the actor host (one shelterwood
   `offload` per `Action`, outcome re-enters as a message), and the RPC server
   are all the same dumb loop around the same machine. The embedding story
   stops being special.

Determinism rules for the core, enforced (§5):
- No `SystemTime::now`, `Instant::now`, `rand`, `Uuid::new_v4/v7`, env reads.
  The **shell mints ids and timestamps** and passes them in — either inside
  `Input`/`ActionOutcome`, or via a `Stamps` value (a pre-minted id/timestamp
  batch) supplied per step. The core treats them as opaque.
- No async in core crates. `Future` appears only in shell crates and in
  boundary traits (`Provider`, `ProviderFactory`, `Session`).
- No `unsafe` in core crates (`#![forbid(unsafe_code)]`).

## 3. Crate layout

Modular crates, split along the core/shell line — not one crate with modules,
because the dependency rules below are only enforceable at crate granularity.

```
rho/
  Cargo.toml                 # workspace; deps + lints defined once at workspace level
  crates/
    # ---- pure cores (no tokio, no fs/net, no clock/rand, forbid(unsafe)) ----
    rho-ai/                  # boundary types: Message, ContentBlock, StreamEvent,
                             #   Usage, StopReason, OpaqueBlob, ProviderError;
                             #   Provider + ProviderFactory traits (async
                             #   signatures, no impls);
                             #   pure validation (schema parse-or-reject)
    rho-core/                # session model types (Entry, Record, facts),
                             #   SessionMachine + Step/Effect/Action/Outcome,
                             #   recovery reducer (records -> LaneStatus),
                             #   context assembly, compaction planning,
                             #   hook/event/command types, queue semantics
    rho-codec-jsonl/         # session wire format: line <-> typed item, header,
                             #   torn-tail truncation logic (pure: bytes in/out)

    # ---- boundary traits + reference impls ----
    rho-store/               # SessionRepo/Session traits; memory backend;
                             #   jsonl backend (fs shell over rho-codec-jsonl);
                             #   conformance suite (pub, reused by rho-store-sqlite later)
    rho-tools/               # Tool trait (schema descriptor + replay marker + async execute);
                             #   builtin tools. Pure compute extracted per tool
                             #   (edit matching, truncation, path rules) with thin
                             #   IO wrappers; MCP client adapter

    # ---- shells ----
    rho-ai-anthropic/        # hand-rolled adapter: pure SSE/event decoder module
                             #   + reqwest transport shell
    rho-ai-openai/           # nanocodex-backed adapter (spike outcome lives here;
                             #   whatever nanocodex layer we use is quarantined here)
    rho-agent/               # the drivers: automatic tokio driver around
                             #   SessionMachine + Session + Provider + tools;
                             #   resume choreography; faux-provider test kit
    rho-rpc/                 # phase 3: codec (pure module) + server loop (shell)
    rho-cli/                 # phase 3: binary; config loading; thin
    rho-ext/                 # phase 4: ABI types (pure) — hook wire structs
    rho-ext-wasm/            # phase 4: wasmtime host (shell)
    rho-shelterwood/         # parked: actor host (shell), per spec/05
```

Dependency rules (the point of the split):

1. Pure crates depend only on pure crates and inert libraries (serde,
   thiserror, bytes, futures-core for trait signatures). **Never** on tokio,
   reqwest, rusqlite, wasmtime, nanocodex, or any `rho-*` shell crate.
2. Shell crates depend on cores; cores never depend on shells. The arrow
   never points outward.
3. Third-party integration crates (nanocodex, wasmtime, MCP SDK, shelterwood)
   are each **quarantined in exactly one shell crate**, so a breaking upstream
   change or a swapped decision (e.g. dropping nanocodex for a hand-rolled
   Responses adapter) is a one-crate blast radius.
4. Binary crates (`rho-cli`) may use `anyhow` at the edge; library crates use
   `thiserror` typed errors exclusively.
5. `rho-ai` is a pure crate that *defines* an async trait: the trait's
   signature is the boundary; implementations live in shell crates. Same for
   `rho-store`'s traits. A trait with an async fn is an interface, not IO.

## 4. Where the pattern lands, phase by phase

**Phase 1 (rho-ai + adapters):**
- Boundary types and schema validation are pure (`rho-ai`); the langsec rule
  *is* the core/shell boundary — nothing unparsed crosses it.
- The Anthropic adapter is built sans-IO style: an incremental SSE/event
  decoder as a pure module (`&mut Decoder`, bytes in → typed events out),
  wrapped by a ~100-line reqwest shell. The decoder gets fixture-driven tests
  (captured streams, malformed frames, mid-token splits) without any HTTP.
- The nanocodex spike happens entirely inside `rho-ai-openai`; its outcome
  (which layer, or fallback to hand-rolled) changes nothing outside that crate.
- The faux provider is pure by construction: scripted `ActionOutcome`s.

**Phase 2 (rho-core + rho-store + rho-agent):**
- `SessionMachine` as in §2 — the phase's deliverable *is* the pure core.
- The recovery reducer (`&[Record] -> LaneStatus`) is a pure function with a
  truth-table test per corruption reason (spec/02 §8).
- Context assembly and compaction cut-point planning: pure functions over
  entry slices (spec/02 §5); the summarize call is an `Action` like any other.
- JSONL codec (line-level encode/decode, torn-tail rule) is pure
  (`rho-codec-jsonl`); `rho-store`'s jsonl backend is the fs shell around it.
  The conformance suite runs against memory and jsonl identically.
- Resume is not new machinery: the shell reads `LaneStatus`, then feeds the
  machine synthetic `Input::Resume{..}` — the machine derives the same kinds
  of effects/actions as live operation (with `origin: Replay`).

**Phase 3 (rho-rpc + rho-cli):**
- RPC message codec + framing: pure module, property-tested round-trip.
- The server is the same driver loop with a socket for a face; interaction
  requests surface as `Action::AwaitInteraction` outcomes arriving from the
  client instead of a TUI. No new decision logic appears in the server.
- CLI arg/config parsing → a plain `Config` value handed inward; the binary
  stays under ~a few hundred lines.

**Phase 4 (rho-ext):**
- Hook payloads are already owned serde data (00-overview §4 guardrail);
  hook dispatch is already an `Action`. The WASM host is therefore *only* a
  shell: marshal `HookInvocation` in, `HookResult` out. Extension misbehavior
  (trap, timeout, garbage) is an `ActionOutcome` variant the core already
  handles — no extension-specific paths inside the machine.

**Embedding (parked, spec/05):** the actor host was specified as "offload per
action, outcome re-enters as message" — §2's machine is exactly the shape that
makes that host a page of code. Nothing to redo when shelterwood matures.

## 5. Enforcement (so the discipline survives contact with development)

- **Workspace lints** (single `[workspace.lints]` table): `forbid(unsafe_code)`
  in pure crates; clippy `disallowed_methods` for `SystemTime::now`,
  `Instant::now`, `thread_rng`, `Uuid::new_v4`, `std::env::var` in pure crates
  (allow-listed in shells).
- **cargo-deny / CI dependency check**: pure crates' dependency trees must not
  contain tokio, mio, reqwest, or fs-touching crates. A ~20-line CI script
  asserting `cargo tree -p <pure-crate>` against an allowlist is enough; run
  it from day one, not after the first violation.
- **Test placement follows the split**: core crates carry the exhaustive
  unit/property/truth-table tests and (post format-stamp) golden replays;
  shell crates carry only conformance + integration tests. A complex test for
  a shell crate is a signal that logic leaked outward — move the logic, not
  the test.
- **Replay-verification test** (§2 property 1) runs in CI over a corpus of
  recorded sessions as soon as phase 2 produces its first file, and every
  future feature adds sessions to the corpus. This is the project's primary
  regression net; treat corpus breadth as a first-class review item.
- Public API hygiene: every crate has a deliberate `lib.rs` re-export surface;
  `pub(crate)` by default; `#[non_exhaustive]` on wire-adjacent enums that
  will grow (entry/record bodies, events, outcomes).

## 6. Anti-patterns to reject in review

- A branch in a shell (driver, backend, adapter) that *chooses* between
  behaviors → that choice belongs in the core; the shell executes.
- Core code taking a callback/closure that performs IO → that's the shell
  calling itself through the core; invert it into an `Action`/`Effect`.
- `tokio::spawn` anywhere except drivers/hosts.
- A "just this once" clock or uuid in core code — it silently breaks replay
  verification, the project's main safety net.
- Feature flags to make one crate serve both sides of the boundary; prefer
  another small crate.
- Mocks of shell traits inside core tests — core tests need no mocks by
  construction; if one seems needed, the boundary is drawn wrong.
