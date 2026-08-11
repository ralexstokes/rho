# rho-ai — provider boundary

Status: draft (2026-08-11). Decisions below are settled unless marked open.

`rho-ai` defines the vocabulary rho-agent speaks to models: boundary types and
one `Provider` trait. It implements no providers itself — backends are adapter
crates (`nanocodex` for OpenAI; a small hand-rolled Anthropic Messages adapter).

## 1. Decisions

### 1.1 Context regime: stateless-first (DECIDED)

The `Provider` trait is **stateless-transcript**: rho-agent owns the transcript
and sends the full context on every request. This is what makes sessions
durable, branchable, and compactable on rho's terms.

Server-side context (OpenAI Responses `previous_response_id` chains,
provider-managed compaction — the regime nanocodex is built around) is treated
as an **optimization**, expressed as an optional provider capability, and only
added when the stateless path is proven impractical or leaves real performance
on the table. The trait must not bake stateful assumptions into its core shape.

**Phase-1 spike resolution:** use `nanocodex-oai-api` 0.3 at its standalone
`OpenAi -> Session -> Response` layer, not `nanocodex-agent`. The adapter
creates a fresh Nanocodex session for every rho `Provider::stream` call and
passes the complete rho transcript through `ResponseInput::items`. Provider
checkpoint storage is disabled, history policy is `FullReplay`, and the
Nanocodex request retry budget is one attempt. This preserves rho's
stateless-transcript contract while reusing Nanocodex's typed Responses wire,
auth, streaming transport, reconnect handling, and error taxonomy. The
integration is quarantined in `rho-ai-openai`; its current fixed-model
constraint (`gpt-5.6-sol`) does not leak into `rho-ai`.

Adversarial review found one remaining limitation in that spike outcome:
`nanocodex-oai-api` 0.3 does not expose `max_output_tokens` in its request
profile, builder, session, or Tower attempt. The adapter validates the common
field and maps provider-reported `max_output_tokens` incompletes to
`StopReason::Length`, so truncated tool calls remain non-executable, but it
cannot transmit rho's requested hard limit. Closing this requires an upstream
Nanocodex request hook or the already-defined fallback of a hand-rolled
Responses transport within `rho-ai-openai`; silently claiming the limit is
enforced is not acceptable.

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

OAuth flows (Claude subscription auth, ChatGPT auth — possibly delegated to
nanocodex for OpenAI) arrive later behind the same trait. No auth UI, no token
refresh machinery in v1 beyond what a backend crate already does internally.

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

trait Provider {
    fn models(&self) -> &[ModelInfo];
    fn stream(&self, req: Request, cancel: CancelToken)
        -> impl Stream<Item = StreamEvent> + Send;
}

struct ProviderError { retryable: bool, kind: ErrorKind, message: String }
```

Rules carried over from the overview:
- `OpaqueBlob` (provider id + payload) carries reasoning signatures / replay
  state; only the producing adapter interprets it; cross-provider handoff
  drops it.
- `Done(AssistantMessage)` is the authoritative record (snapshot-authoritative
  rule at the provider layer); deltas are display hints.
- No model-catalog subsystem: a static table + config entries.

## 3. Testing

- A `faux` provider (scripted responses, scripted errors/aborts/truncations)
  is part of the crate and is what rho-agent's tests run against.
- **Per-provider conformance suites** (see 1.2): for each adapter, recorded
  fixture exchanges asserting: schema-conforming tool calls parse; known
  nonconforming habits are either fixed in the adapter or documented as
  rejected; stop reasons, usage, and opaque-blob round-tripping are correct.

## 4. Open

1. `Request` shape details beyond phase 1: cache-hint pass-through and any
   provider-specific advanced tool-definition fields. Phase 1 settles the
   common shape as system text, a complete typed transcript, JSON-Schema
   function tools, model id, output limit, and thinking effort.
2. Thinking-level compatibility policy for older Anthropic models that require
   fixed-budget thinking rather than the current adaptive-thinking API.
3. Whether `ModelInfo` carries cost tables in v1 (usage display) or later.
4. Add a Nanocodex request hook (or take the quarantined hand-rolled fallback)
   so the OpenAI adapter can transmit the common hard output-token limit.
5. `jsonschema` currently reaches `getrandom` through `ahash` even with its
   own default features disabled. Validation results and diagnostics are
   normalized deterministically, but removing ambient randomness from the
   pure dependency graph requires a deterministic upstream map configuration,
   a patched dependency, or a replacement validator.
