# rho × shelterwood — embedding spec

Status: draft (2026-08-11). Grounded in shelterwood @ local checkout
(`../shelterwood`): SPEC.md, core `src/`, and the assistant-control-plane
acceptance test (`crates/shelterwood/tests/acceptance/assistant.rs`), which is
the closest existing model for rho's topology and should be treated as the
reference pattern.

Scope note: shelterwood Part II features (peer monitoring/`watch`, `contramap`,
`Context::project`, keyed conflation, group restart strategies, pinned refs,
serde, stats) are **not implemented** — this design uses core-only primitives
and must not depend on any Part II item.

## 1. Ownership boundary (answers overview OQ #7)

**rho defines all message enums as plain Rust enums; shelterwood imposes only
`Msg: Send + 'static`** — no wrapper types, no message traits, no envelopes.
The split:

- `rho-agent` defines runtime-agnostic `SessionCommand` / `AgentEvent` types
  with an *abstract reply slot* (so the core has no shelterwood dependency).
- `rho-shelterwood` defines the concrete actor `Msg` enum, embedding
  `shelterwood::Reply<T>` in request/response variants. Rationale: shelterwood's
  `CallError` taxonomy is the single most valuable thing it gives a journaled
  agent — it distinguishes
  - `AcceptanceTimedOut` → command never accepted → **safe to retry**,
  - `ResponseTimedOut` → accepted, outcome unknown → **reconcile via journal,
    never blind-retry**,
  - `ReplyDropped` → incarnation crashed mid-command → **wait for a superseding
    incarnation, then retry** —
  and every successful send resolves to the accepting `Incarnation`, which is
  exactly the fencing evidence at-most-once command delivery needs.

Fencing rule (from shelterwood docs, enforced by its tests): compare
incarnations with `supersedes()`, never arithmetic — restart storms advance by
more than one between observations.

## 2. Session actor shape

Topology (mirrors the acceptance test almost exactly):

```
Tree (ordered root)
├── "sessions"  DynamicTree            ← mounted add_subtree_once: structurally
│   │                                     cannot restart (see §6)
│   └── "session-<id>"  Tree (one per rho session)
│       ├── "control" actor            ← SessionCommand mailbox (queue)
│       ├── "stream"  actor            ← Mailbox::latest(), conflating deltas
│       └── "tools"   DynamicTree      ← runtime sub-agent / worker spawn
└── (hosts: rpc listener, etc.)
```

Rules, each load-bearing:

1. **Command mailbox is `Mailbox::queue(n)`, never `latest()`** — conflation
   silently drops embedded `Reply`s (callers see `ReplyDropped` for a live
   actor). `latest()` is reserved for the separate stream actor, which is a
   perfect match for rho's snapshot-authoritative protocol rule: deltas are
   transient hints, so a conflating mailbox losing intermediate deltas is
   *correct by design*.
2. **Turns never run inside `handle`.** A handler blocks the mailbox while it
   awaits, so `Steer`/`Abort` would queue behind the turn (and calling yourself
   is a documented deadlock). The turn advances via `ctx.offload(work,
   continuation, deadline)`: each unit of loop work (provider stream, tool
   batch) runs off-loop and re-enters the mailbox as a completion message. This
   is where rho-agent's **manual-drive state machine pays off**: the actor holds
   the session state machine, `peek_action()` decides the next unit, `offload`
   executes it, the continuation message feeds `execute_action()`. Automatic
   mode (rho's own tokio driver) is what the CLI host uses; the actor host
   drives explicitly.
3. **Offload loss is the crash model.** Offloads are incarnation-owned and
   cancelled at intake freeze with the continuation suppressed entirely — a
   restart silently loses in-flight work. That is fine *because* the lane
   journal already treats "operation started, no completion recorded" as the
   suspended/resumable state. Consequence: journal writes must happen at action
   boundaries (before/after offload), not batched at turn end.
4. **Abort must beat a full mailbox.** There is no priority lane in core.
   `Abort` = `try_send` **plus** a shared cancel flag in `Args` that the
   offloaded work observes (rho's internal cancellation token). The offload
   guard's drop-cancels behavior (`offload_scoped` → `Guard`) is the natural
   implementation: stash the guard for the active run; dropping it aborts.
5. **Pending interactions hold a `Reply`.** Permission prompts / dialogs:
   the actor stores `Option<Reply<InteractionAnswer>>` and resolves it when
   `AnswerInteraction` arrives — the acceptance test's bridge actor does
   exactly this shape.
6. **Idle eviction** via re-armed keyed timer (`set_timeout("idle", ..)`), and
   eviction = `sessions.remove_scope(&session)`. Note: `set_interval` requires
   `Msg: Clone`, which a `Reply`-bearing enum isn't — use re-armed
   `set_timeout` only.

## 3. Restart → resume (the synergy, concretely)

What survives a restart is exactly what `Args` carries (restart = re-run `init`
with freshly minted args). So:

```rust
#[derive(Clone)]
struct SessionArgs {
    session_id: SessionId,
    repo: Arc<dyn SessionRepo>,          // durable store — the real state
    events: broadcast::Sender<AgentEvent>, // survives restarts (in Args)
    cancel_flag: Arc<AtomicBool>,          // abort lane (§2.4)
    providers: Arc<ProviderSet>,
}
```

Rehydration protocol (copy the acceptance-test pattern):

1. `init` posts `context.continue_with(Rehydrate)` — continuations run ahead of
   queued mail, so **replay happens before any external command, for free**
   (mailbox acceptance is never gated on readiness, so commands sent during
   rehydration queue correctly behind it).
2. The `Rehydrate` handler opens the session, inspects the lane journal:
   0 open operations = idle; 1 = suspended → resume (replay-safe tools re-run,
   unsafe ones become failed results per the tool replay-safety marker);
   2+ = corruption → fail the incarnation (supervisor policy decides).
3. Declare `Readiness::Manual` and `mark_ready()` at the end of `Rehydrate`,
   so supervision-level readiness means "journal replayed, accepting work".
4. Record the incarnation in the journal on operation start (observability +
   post-mortem fencing evidence).

**Replay-provenance hazard (called out by shelterwood's own spec):** with only
same-`Msg` middleware in core, a journaling layer cannot distinguish an
external command from a self-issued `continue_with` regeneration — a replayed
effect can run twice. rho therefore carries provenance in its own enum:
`origin: Origin::External | Origin::Replay` on commands that have side effects.

## 4. Events out

Core shelterwood has no user event bus; fan-out is rho's:

- `broadcast::Sender<AgentEvent>` lives in `Args` (survives restarts);
  subscribers come from rho's session-registry API, not shelterwood.
- Additionally bridge `scope.subscribe_lifecycle()` into rho-level events
  (`SessionEvent::Crashed/Restarting {..}`) so clients observe supervision
  facts on the same stream. Use the documented catch-up protocol verbatim:
  subscribe first, then snapshot, then discard events at-or-before the
  watermark.
- Backpressure nuance: rho-agent's *internal* event sinks are awaited, but the
  actor boundary uses broadcast (lossy-by-lag) for observers; anything that
  must not be lost (journal writes, snapshots) is not an "event" — it's state.
  This is the snapshot-authoritative rule again, now enforced by construction.
- Teardown discipline: any send to a peer during shutdown/`on_stop` must be
  `try_send` (plain `send` can park forever against frozen intake). And never
  `let _ = actor.send(msg);` — the send future is lazy; unawaited = silent
  no-op. Both are lint-worthy in rho's codebase.

## 5. Sub-agents and workers

- Fire-and-forget supervised worker → `add_actor` in the session's `"tools"`
  DynamicTree.
- **Awaitable result → task, not actor**: actors have no typed completion in
  shelterwood; a sub-agent whose parent awaits its outcome is
  `add_task_once(id, TaskOnceDef::new(..))` yielding `OneShotTaskRef<T>` whose
  consuming `wait()` returns `Result<T, Exit>` (panics/timeouts arrive as
  structured errors, never hangs). A sub-*session* (full rho agent) is
  `add_subtree_once` of a session tree + a result channel in its args.
- Fate-sharing ("if the tool executor dies, restart the whole session") is not
  expressible as a policy (OneForOne only) — model it with subtree boundaries:
  the subtree is the restart unit; an intensity trip inside it fails the whole
  subtree to the parent.
- Same-id remount race: removal-in-progress fences re-add with
  `RemovalInProgress`; await the removal, then retry. Old handles report
  `Terminated`; replacement membership is deliberately incomparable.

## 6. Roster durability

Restart never re-creates runtime-added dynamic children — that is application
state by design. Therefore:

- Mount `"sessions"` with `add_subtree_once` under the ordered root so it
  structurally cannot restart; losing the root is process-level failure.
- **rho owns the session roster** (which sessions were open lives in the
  session store); on process start, re-add session actors from the roster.
  This composes with §3: process crash → restart process → re-add sessions →
  each rehydrates from its journal → suspended runs resume. Full crash
  recovery with no in-memory handoff at any level.

## 7. Cancellation & operational preconditions

- Shelterwood ships its **own** `CancellationToken` (observe-only:
  `is_cancelled`/`cancelled().await`; only the engine fires it), with the
  ordering guarantee shutdown-token-before-abort-token. rho-agent keeps its own
  internal cancellation (tokio-util or equivalent) for *user aborts* — distinct
  from shutdown — and the actor maps: shelterwood `shutdown_token` fired →
  trigger rho cancel → loop reaches clean point → journal records outcome.
- `panic = "unwind"` is a hard precondition for supervision (under
  `panic = "abort"` shelterwood sees nothing) — assert in embedding docs and CI.
- Shelterwood types have no serde; rho config stays in rho types, translated to
  `RestartPolicy`/`Intensity`/`Mailbox` at wiring time; construct
  policy values once (`OnceLock`), since constructors validate eagerly.
- `System` is `#[must_use]`, single-use; trees are consumed on spawn. "Restart
  the whole agent runtime" = rebuild a fresh tree from retained host state
  (documented shelterwood embedding pattern).

## 8. What this changes in rho-agent (feedback into core design)

1. Manual drive is the primary loop API (the actor host depends on it);
   the automatic driver is a convenience built on it.
2. Journal writes at **action** boundaries, not turn boundaries (§2.3).
3. Tool definitions carry a replay-safety marker (`replay: safe | never`) —
   required, not optional, for §3.2.
4. Commands with side effects carry `Origin` provenance (§3, hazard note).
5. `AgentEvent` and `SessionCommand` must be `Send + 'static` + serde
   (shared vocabulary of actor host and RPC host); reply slots abstract in
   core, concretized per host.
6. rho-agent must expose "open session + inspect open operations" as a cheap,
   first-class call — it is the supervisor's readiness/recovery primitive.

## 9. Open questions

1. Offload deadlines are mandatory — pick turn/step budget semantics (generous
   per-action deadline? per-tool timeout as the real bound?).
2. Does the stream actor conflate per-session (one `latest()` mailbox) or
   per-subscriber? (v1: per-session; RPC host fans out.)
3. Interaction timeout ownership: actor timer vs client-side — who resolves a
   dangling `Reply` when a client disappears?
4. How much of the `"tools"` DynamicTree is v1 vs deferred until sub-agents
   land? (v1 can run tool batches purely in offloads; the tools scope becomes
   necessary only for supervised long-lived workers / sub-agents.)
