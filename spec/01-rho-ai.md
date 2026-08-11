# rho-ai — provider boundary

Status: draft (2026-08-11). Decisions below are settled unless marked open.

`rho-ai` defines the vocabulary rho-agent speaks to models: boundary types and
two traits — `ProviderFactory` (shared: credentials, model catalog, opening
sessions) and `Provider` (one live logical model session). It implements no
providers itself — backends are adapter crates (`nanocodex` for OpenAI; a
small hand-rolled Anthropic Messages adapter).

## 1. Decisions

### 1.1 Context regime: transcript-authoritative provider sessions (DECIDED)

Revised 2026-08-11, superseding the earlier stateless-first decision; the
phase-1 spike is the evidence. Running Nanocodex against its grain (fresh
session per call, checkpoints disabled, `FullReplay`) works, but discards the
native continuation machinery of every naturally sessionful backend. It also left running
provider sessions with no lifecycle or ownership story for actor hosts.

A `Provider` is **one live logical model session owned by a rho session**. A
shared `ProviderFactory` owns credentials, the model catalog, and opening
sessions. This inverts which shape is primary rather than abandoning the old
goal: a sessionful interface trivially yields a stateless implementation
(every generation rebases from the transcript), whereas statefulness as a
capability flag on a stateless trait leaves session lifecycle with no owner.

Everything the stateless-first decision protected is retained as explicit
rules:

1. **The rho transcript and journal are the sole durable truth.** Provider
   checkpoints and external session ids (e.g. Responses chains) are optional 
   acceleration state — never required for correctness, never the only copy of anything.
2. **rho-agent passes the full authoritative transcript on every generation.**
   The adapter diffs it against what its native session has acknowledged:
   prefix match → continue natively; anything else (restart, branch, lost
   checkpoint, doubt) → **rebase**: open a fresh native session replayed from
   the transcript. rho-agent never tracks provider-side state and never sends
   deltas, so a staleness bug is a performance bug, never a correctness bug,
   and deterministic golden-session replay is unaffected. "Always rebase" is
   the degenerate, trivially correct implementation — it is what a stateless
   backend (the Anthropic Messages adapter) does on every call.
3. **Poison on ambiguity.** A failed, cancelled, or retried generation leaves
   the native session in doubt; the adapter marks it dirty and the next
   generation rebases. This keeps the single-retrier rule (§1.4) sound: the
   loop can re-invoke without double-appending to a provider-side chain.
4. **Resume and replay rebase.** The live `Provider` is incarnation-local
   state. A restarted incarnation (`origin: Replay`, spec/02 §8) reopens from
   the session file and never reuses a prior incarnation's continuation.
5. **Tool execution stays in rho.** A provider-side system that independently
   runs tools is a nested agent, not a Provider.
6. **Model or incompatible configuration change = a new provider session**
   (close, then `ProviderFactory::open` again). Whether a config delta is
   compatible is the adapter's judgment; when in doubt, reopen. Mid-session
   model swap is thus a factory concern, not a session mutation.
7. **Generations serialize by construction.** `Provider::generate` takes
   `&mut self` and the returned stream borrows the session, so a second
   generation cannot start until the first stream is dropped. Hosts that want
   an ownership/lifecycle home for the live session run it inside an actor
   (spec/05); the trait itself is actor-agnostic and must not depend on
   shelterwood.

**Phase-1 spike resolution (updated for this regime):** use a pinned post-0.3
`nanocodex-oai-api` revision at its standalone `OpenAi -> Session -> Response`
layer, not `nanocodex-agent`. The spike ran that layer statelessly (a fresh
Nanocodex session per call, complete rho transcript through
`ResponseInput::items`) and validated the typed Responses wire, auth,
streaming transport, reconnect handling, and error taxonomy. Under the
sessionful regime the adapter instead holds one live Nanocodex session per
rho `Provider`, continuing it natively while the transcript prefix matches
acked state and rebasing per rules 2–3 otherwise; the spike's per-call
behavior remains the rebase path. Provider checkpoint storage stays disabled
(acceleration hints, if ever persisted, come later — §4), and the Nanocodex
request retry budget stays at one attempt. The integration is quarantined in
`rho-ai-openai`. Its closed model set currently contains `gpt-5.6-luna` (the
CLI default) and `gpt-5.6-sol`; that policy does not leak into `rho-ai`.

Adversarial review found one remaining limitation in that spike outcome: the
pinned `nanocodex-oai-api` revision does not expose `max_output_tokens` in its
request profile, builder, session, or Tower attempt. The adapter validates the
common field and maps provider-reported `max_output_tokens` incompletes to
`StopReason::Length`, so truncated tool calls remain non-executable, but it
cannot transmit rho's requested hard limit. Closing this requires an upstream
Nanocodex request hook or the already-defined fallback of a hand-rolled
Responses transport within `rho-ai-openai`; silently claiming the limit is
enforced is not acceptable. Recheck this at the sessionful integration
surface — the live-session shape may reach different request hooks.

### 1.2 Validation: langsec at the boundary (DECIDED)

**If it doesn't parse, the library doesn't see it.** Every input from a
provider is parsed into typed structures at the adapter boundary or rejected
there; no partially-valid data crosses into rho-agent.

- Tool arguments: parse against the tool's JSON Schema (schemars-derived);
  failure → the tool call becomes a structured error result returned to the
  model (which retries with corrected args). **No coercion layer in the
  harness** — no `"5"`→5, no singleton-wrapping, none of pi's TypeBox
  `Value.Convert` lore.
- Where a provider habitually emits nonconforming args, that is a **per-provider
  tool-conformance concern**: tracked by per-provider conformance test suites,
  fixed in the adapter or upstream, never absorbed as harness logic.
- Same rule for streamed partial JSON: the harness never interprets unparsed
  fragments. Raw arg deltas may be forwarded to clients as advisory display
  events; parsing them (for live previews) is a client concern.
- Corollary of the truncation lesson: a `length`-terminated response's tool
  calls are never executed — args that parse may still be incomplete; the stop
  reason is part of the validity judgment.

### 1.3 Auth: minimal, trait-shaped (DECIDED)

v1 credential resolution is environment variables plus a plain credentials
file, behind one small trait:

```rust
trait CredentialSource {
    fn resolve(&self, provider: &ProviderId) -> Result<Credential, CredError>;
}
```

OAuth flows (ChatGPT auth — possibly delegated to nanocodex for OpenAI) arrive
later behind the same trait. No auth UI, no token refresh machinery in v1 
beyond what a backend crate already does internally.

The `ProviderFactory` is the consumer of this trait: resolution happens at
`open` time, and whatever refresh a backend crate does internally lives inside
the opened `Provider`. Individual generations never touch credential
resolution.

### 1.4 Retry: exactly one retrier (DECIDED)

Double-retry is a design error. The contract:

- An adapter either retries internally **or** classifies errors as
  `retryable: bool` in `ProviderError` — never both. Adapters wrapping crates
  with internal retries (nanocodex) must disable or bound them and declare it.
- rho-agent's loop-level policy is the **only** place a request is re-invoked.
- Aborts are terminal and never retried.

Phase 1 follows this rule directly: the Anthropic adapter performs no retries,
and the OpenAI adapter configures Nanocodex for one total request attempt.
Both project retryability into `ProviderError`; the future rho-agent loop is
the only component allowed to act on that classification.

## 2. Boundary types (sketch)

```rust
enum ContentBlock {
    Text(String),
    Thinking { text: String, opaque: Option<OpaqueBlob> },
    Image { data: Bytes, mime: String },
    ToolCall { id: ToolCallId, name: String, args: serde_json::Value },
}

struct AssistantMessage {
    blocks: Vec<ContentBlock>,
    stop: StopReason,          // Stop | ToolUse | Length | Error | Aborted
    usage: Usage,
    provider: ProviderId,
    model: ModelId,
}

enum StreamEvent {
    Start,
    Delta { index: usize, kind: DeltaKind, delta: String }, // advisory only
    BlockDone { index: usize },
    Done(AssistantMessage),    // authoritative
    Error(ProviderError),
}

trait ProviderFactory {
    fn models(&self) -> &[ModelInfo];
    async fn open(&self, config: SessionConfig)
        -> Result<Box<dyn Provider>, ProviderError>;
}

trait Provider {
    // One generation. `req` carries the complete authoritative transcript;
    // the returned stream borrows the session, so generations serialize.
    fn generate(&mut self, req: Request, cancel: CancelToken)
        -> impl Stream<Item = StreamEvent> + Send + '_;
}

struct SessionConfig { model: ModelId, /* fixed per-session parameters */ }

struct ProviderError { retryable: bool, kind: ErrorKind, message: String }
```

Rules carried over from the overview:
- `OpaqueBlob` (provider id + payload) carries reasoning signatures / replay
  state; only the producing adapter interprets it; cross-provider handoff
  drops it. Because the transcript carries these blobs, the rebase path stays
  faithful even for provider reasoning-replay state.
- `Done(AssistantMessage)` is the authoritative record (snapshot-authoritative
  rule at the provider layer); deltas are display hints.
- The semantic contract of a generation is a function of the transcript in
  `req` alone: whether the adapter continued natively or rebased must be
  unobservable to rho-agent (beyond latency/cost and ordinary sampling
  nondeterminism).
- No model-catalog subsystem: a static table + config entries, served through
  `ProviderFactory::models`.

## 3. Testing

- A `faux` provider (scripted responses, scripted errors/aborts/truncations)
  is part of the crate and is what rho-agent's tests run against. It gains a
  scripted sessionful mode (records continue-vs-rebase decisions) so harness
  tests can assert continuation is invisible to rho-agent.
- **Per-provider conformance suites** (see 1.2): for each adapter, recorded
  fixture exchanges asserting: schema-conforming tool calls parse; known
  nonconforming habits are either fixed in the adapter or documented as
  rejected; stop reasons, usage, and opaque-blob round-tripping are correct.
- Sessionful-regime cases per adapter (§1.1):
  - *continue/rebase equivalence*: the same transcript yields an equivalent
    authoritative `Done(AssistantMessage)` whether the adapter continued
    natively or rebased (asserted structurally on fixtures);
  - *poison-after-error*: after a failed or cancelled generation, the next
    generation observably rebases;
  - *branch → rebase*: a transcript that diverges from the acked prefix never
    continues the native session.

## 4. Open

1. `Request` shape details beyond phase 1: cache-hint pass-through and any
   provider-specific advanced tool-definition fields. Phase 1 settles the
   common shape as system text, a complete typed transcript, JSON-Schema
   function tools, output limit, and thinking effort; the model id moves to
   `SessionConfig` (session-fixed under §1.1 rule 6 — see open item 5).
2. Thinking-level compatibility policy for older Anthropic models that require
   fixed-budget thinking rather than the current adaptive-thinking API.
3. Whether `ModelInfo` carries cost tables in v1 (usage display) or later.
4. Add a Nanocodex request hook (or take the quarantined hand-rolled fallback)
   so the OpenAI adapter can transmit the common hard output-token limit;
   recheck at the sessionful integration surface (§1.1).
5. `SessionConfig` contents for phase 1 beyond `model`: which parameters are
   session-fixed (reopen to change) vs per-request on `Request`. Thinking
   effort is the first candidate to settle.
6. Whether external session ids / provider checkpoints are ever *persisted* as
   acceleration hints (e.g. a session fact), or stay strictly in-memory.
   Rule 1 of §1.1 permits persistence only as a hint that must be validated
   against the transcript before use; v1 keeps them in-memory only.
7. `jsonschema` currently reaches `getrandom` through `ahash` even with its
   own default features disabled. Validation results and diagnostics are
   normalized deterministically, but removing ambient randomness from the
   pure dependency graph requires a deterministic upstream map configuration,
   a patched dependency, or a replacement validator.
