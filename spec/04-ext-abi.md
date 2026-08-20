# rho-ext — extension ABI (constraints sketch)

Status: constraints sketch (2026-08-20). Implementation remains phase 4
(00-overview §3). What this document does is bind the *earlier* phases: the
decisions below are constraints on phase-2/3 types and CI so that
componentization stays a packaging choice instead of a refactor. The full WIT
text is deliberately unwritten — like the RPC protocol, it is a thin
projection of types that phase 2 produces, and writing it first would invert
that dependency.

## 1. Position: decisions as plugins, effects native (DECIDED)

The extension boundary is the pure-core / mutable-shell line of spec/06, made
loadable. The rule that locates it is **capability ownership**:

- The harness owns exactly what a plugin cannot own without *becoming* the
  harness: the journal (durability is the recovery API — its correctness must
  not depend on guest code), provider transport and credentials,
  process-touching tool execution, id/timestamp minting, cancellation, and
  supervision glue. None of these is ever behind the ABI.
- Everything deterministic is plugin-eligible: the session machine, context
  assembly, compaction policy, hook logic, edit-matching, permission policy.
  spec/06's purity discipline is what makes this set maximal — a subsystem
  built to the core/shell rule is componentizable by construction.

Corollary: "minimal harness" has a fixed point, and it is the effect
executor. The end-state harness contains no agent behavior at all; an agent
is a component plus a journal plus capability grants. Moving anything on the
shell side of the line into a plugin would not shrink the harness — it would
relocate the capability and add marshaling, versioning, and trap handling on
top.

## 2. One extension surface (DECIDED)

WASM components are the **only supported extension mechanism**. rho-agent's
native hook traits are an internal seam that hosts use to dispatch
`Action::InvokeHook`; they are not a public extension contract and carry no
stability promise. Native embedders do not need an extension system — they
own a shell and link code; that is embedding (spec/05), not extension. There
is no dylib story. Two supported surfaces is how a harness stops being
minimal.

Non-extensions, unchanged from 00-overview: skills and prompt templates are
pure data, and MCP remains the first tool-extensibility mechanism. WASM tools
arrive later to serve the case MCP cannot: capability-scoped execution as a
middle ground under the v1 no-gating safety stance.

## 3. Worlds

Three worlds, tiered by what they may import:

- **`agent-core`** — exports `handle(input) -> step` and
  `resolve(outcome) -> step`; **imports nothing**. The spec/06 §2 session
  machine is the first-party default component. Purity is structural: a
  component with no imports cannot observe a clock, randomness, a filesystem,
  or the environment, so replay determinism becomes a property of the sandbox
  rather than of code review (see spec/06 §5 for the corresponding CI gate).
- **`tool`** — exports describe (name, JSON Schema, replay marker) and
  execute; imports only capability-scoped host interfaces (fs subsets,
  wasi-http, host-mediated exec) matching its grants (§6).
- **`hook`** — request/response over plain owned data, per the phase-2
  guardrail already in force (00-overview: no handles, borrows, or
  callbacks); imports at most the same capability set as `tool`, default
  none.

**State.** An `agent-core` instance holds machine state in instance memory
between calls. The journal remains the sole durable truth; recovery
re-instantiates the component and rehydrates by replaying journaled inputs
and outcomes. A state-snapshot export may arrive later as acceleration only,
under the same rule as provider checkpoints (spec/01 §1.1 rule 1): never
required for correctness, never the only copy of anything.

**Deltas never cross the ABI.** Advisory stream events stay host-side;
extensions are message-level, exactly like the machine. Live-preview
rendering of partial output is a client concern (spec/01 §1.2 corollary), not
an extension hook.

## 4. Thin WIT, thick JSON (DECIDED)

The WIT surface is a handful of functions taking and returning
strings/bytes. Payloads are the versioned owned-serde types that already
serve the mailbox and the RPC protocol — **one vocabulary across all three
boundaries** (actor message, wire message, ABI payload), extending the
identity 00-overview §2.6 already establishes for the first two.

- Langsec at every crossing, both directions: parse-or-reject; an unknown
  payload version is a refusal to dispatch, never best-effort reading.
- World identity and versioning ride on the WIT package name
  (`rho:ext/agent-core@…`); payload evolution rides on the serde types'
  own versioning. The two move independently.

Trade, accepted: guest-side ergonomics are worse than fully-typed WIT
(authors parse JSON against published schemas instead of using bindgen
types). Gained: no triple-maintained type definitions, and minimal exposure
to WIT-ecosystem churn. Revisit only if guest authorship pain proves real —
a typed WIT layer can be generated *over* the JSON surface later without
breaking it.

## 5. Failure semantics

No extension-specific paths exist inside the machine — misbehavior arrives
through vocabulary the core already handles:

- A `tool` trap, timeout, or garbage result → a structured failed
  `ToolResult`; the model reacts, as with any tool error.
- A `hook` trap, timeout, or garbage result → the failure `ActionOutcome`
  variant for `InvokeHook` (spec/06 §4 phase 4 already requires this).
- An `agent-core` trap is a host-level failure: the CLI driver errors; the
  actor incarnation fails and its supervisor restarts it; recovery is
  re-instantiate + rehydrate from the journal. Effects are derived by the
  core and executed by the shell afterward, so a trap mid-call leaves the
  journal at the previous action boundary — nothing partial is durable.
  Determinism cuts both ways: a reproducible trap is a poison-pill input, the
  journal is its exact repro, and supervisor restart intensity is the brake
  on the retry loop.

## 6. Capabilities and trust

Capabilities (exec, fs paths, net) are declared by the component's manifest,
granted at install/trust time, and enforced as host imports — a world only
ever links the imports its grants allow. Sandboxing is a policy default, not
an inescapable property (00-overview §2.5): coding agents legitimately need
host access, and the grant is where that is made explicit and auditable.
Project-local extensions arrive together with project-local config — the
config decision in 00-overview already ties the two to this one trust design.

## 7. Identity and replay

Component identity is a content digest of the component binary. Record it in
`HostInfo` on `op_started` (spec/02 §6 — already host-opaque to the core, so
no format change), scoping identity per operation: replay tooling selects the
matching component, and a mid-session harness or component upgrade is
naturally visible at the operation boundary. Pinning the digest makes
golden-session replay bit-exact against the *exact* logic that produced the
session, across harness releases.

## 8. What binds now (phases 2–3)

1. **wasm32 gate** (normative home: spec/06 §5): pure crates build for
   `wasm32-unknown-unknown` in CI from now. The gate starts with `rho-core`
   and `rho-codec-jsonl`; `rho-ai` joins when spec/01 open item 7 (the
   validator's `getrandom` reach) is resolved — that item is upgraded from
   cosmetic to gate-blocking.
2. **ABI-expressible boundary types**: everything crossing
   `Input`/`Step`/`Effect`/`Action`/`ActionOutcome`, hook payloads, and tool
   descriptors is owned serde data — no handles, borrows, callbacks, or
   platform types.
3. **Deltas never cross the ABI** (§3): keeps the component call rate at
   actions, not tokens, which is what makes the boundary's cost negligible
   against an LLM call.
4. **Component-only extension surface** (§2): no public native hook contract
   is published in phases 2–3 that would have to be deprecated in phase 4.

## 9. Open

1. Full WIT text and world versioning policy — written when phase 4 starts,
   projected from the phase-2 types.
2. componentize-js maturity for TS authors — the weakest toolchain link;
   evaluate at phase-4 entry, and keep the thin-WIT surface small enough that
   hand-rolled guest glue is a viable fallback.
3. Whether provider adapters ever componentize (via wasi-http). Credentials
   crossing into guests, streaming/cancellation latency, and native backend
   crates argue no for the foreseeable future; the decoder halves are already
   pure and portable either way.
4. Timeout ownership and budgets for `tool` and `hook` calls (relates to
   spec/05 §9's offload-deadline question — the host's offload deadline and
   the component budget should be one design).
5. Instance lifecycle in fleet hosts: per-session instances, pooling, and
   eviction interplay with spec/05 §2.6 idle eviction.
6. Whether `agent-core` ever gains a *deliberate* nondeterminism import (e.g.
   host-minted randomness surfaced as journaled input). Default no: anything
   random enters as an `Input`/`ActionOutcome` value like ids and timestamps
   already do.
