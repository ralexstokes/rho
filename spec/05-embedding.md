# rho × Shelterwood — supervised session host

Status: **IMPLEMENTED for phase 3** (2026-08-12).

The integration is pinned to Shelterwood `main` commit
[`02d2f236ac6f29dacf1dce5bd72c72b4bbdf790b`](https://github.com/ralexstokes/shelterwood/commit/02d2f236ac6f29dacf1dce5bd72c72b4bbdf790b),
verified against the remote branch on 2026-08-12. The exact revision, rather
than a moving branch, makes Cargo and Nix builds reproducible while satisfying
the requirement to integrate the latest `main` API available at implementation
time.

## 1. Runtime topology

`rho-shelterwood::SupervisedSessions` owns this tree:

```text
Tree (fixed ordered root)
└── sessions  DynamicTree (one-shot structural child)
    └── session-<id>  Tree (one per attached rho session)
        └── control  restartable SessionActor
```

The fixed root makes the dynamic session roster structurally non-restartable.
Each session subtree is its exact fate-sharing and removal unit. Its `control`
actor uses `ActorDef` with Shelterwood's default on-failure restart policy and
a bounded FIFO mailbox; a latest-value mailbox is forbidden because the
messages contain consuming `Reply` capabilities.

The RPC byte transport remains the outer process boundary. stdio has one
connection for the process lifetime and the Unix listener creates one
`HeadlessHost` tree per controlling connection. Making socket accept itself an
actor would add a second request loop without improving recovery: transport
disconnect already tears down the whole connection-owned tree, while the
durable, restartable unit is the session actor below it.

## 2. Durable args and incarnation-owned state

Restartable `SessionArgs` contain only cloneable host/durable state:

- session ID and `Arc<dyn SessionRepo>`;
- resolved host config and provider factories;
- the connection's RPC sender;
- shared busy/startup diagnostics; and
- a `ControlHub` that points at the current incarnation's rho control channel.

Every `SessionActor::init` reconstructs incarnation-owned state:

1. acquire the session writer lock;
2. rebuild the four native tools and reconnect configured MCP children;
3. reconstruct `SessionMachine` from the selected durable branch;
4. reopen the configured provider from the transcript-authoritative factory;
5. install a fresh rho control receiver in `ControlHub`; and
6. if the lane is suspended, run `Driver::resume` before the actor becomes
   ready.

The live actor then exclusively owns the writer, machine, provider, tool set,
MCP connections, and control receiver. Nothing process-local is treated as
authoritative across an incarnation boundary.

## 3. Commands, replies, and the responsive control lane

Prompt, explicit compaction, and configuration are Shelterwood actor calls.
Their messages carry `shelterwood::Reply<Result<_, ErrorObject>>`, so callers
get Shelterwood's acceptance/response failure taxonomy and the accepting
`Incarnation`. Prompt and compaction reply only when rho emits
`OperationStarted`, after the corresponding entry and journal records have
been appended.

A turn intentionally runs through rho's automatic `Driver` inside the control
callback. Steering, follow-ups, queued cancellation, abort, and interaction
answers do not enter that blocked mailbox: they use `ControlHub` to reach the
current incarnation's dedicated rho control channel, which the driver polls
before and during actions. This preserves one decision loop while keeping
abort and steering responsive; duplicating the manual machine driver inside
the actor would create a second orchestration implementation.

`CallErrorKind` determines retry posture:

- `AcceptanceTimedOut` proves non-acceptance and is safe to retry.
- `ResponseTimedOut` means accepted with an unknown outcome; reconcile from
  the journal.
- `ReplyDropped` means the accepting incarnation ended; wait for a
  superseding incarnation and reconcile before retrying.
- `Terminated` means the membership cannot accept more work.

Incarnations are compared only with `supersedes()`, never arithmetic.

## 4. Restart and journal resume

An unexpected callback panic is caught by Shelterwood. A driver boundary error
is surfaced as an actor failure after emitting its diagnostic snapshot, so it
uses the same supervised path. On restart, the old writer/provider/MCP
resources are gone and `init` follows §2 again. rho's journal reduction decides
whether there is work to resume and applies the existing replay-safety rules;
Shelterwood does not invent a second recovery protocol.

New operation host metadata includes the diagnostic representation of the
accepting Shelterwood incarnation. RPC also emits `agent.supervision` when an
incarnation has rehydrated and is ready.

The regression test injects a provider panic after the durable operation start.
It proves all of the following in one flow:

- the prompt acknowledgement was fenced by the first incarnation;
- the first incarnation died with a suspended journal;
- Shelterwood opened a second provider/session incarnation;
- rho resumed the same operation from the journal; and
- the final snapshot is idle and contains the recovered assistant message.

`rho-shelterwood` separately tests exact membership restart fencing: a call
after an injected crash is answered by an incarnation that `supersedes()` the
one that accepted the crash message.

## 5. Cancellation, shutdown, and removal

Shelterwood's scope shutdown token and rho's user-abort token have distinct
jobs. For each active driver call, the actor bridges the scope token into a
fresh rho cancellation token; when the call ends, that bridge task is aborted.
The host first requests abort through every current `ControlHub`, then consumes
the sole Shelterwood `System` owner and waits through a ten-second cooperative
shutdown grace. The framework joins actor resources before returning.

Dynamic removal uses the exact held session scope, never a string lookup, so a
same-name successor cannot be removed accidentally. Dropping a host without
explicit shutdown still drops the sole `System` owner and requests graceful
shutdown, but normal CLI paths always await it.

## 6. Deliberate phase-3 limits

- RPC events continue through the bounded connection sender. A separate
  `Mailbox::latest()` stream actor is deferred until rho has multiple event
  subscribers; snapshots already remain authoritative.
- Per-session dynamic tool/sub-agent scopes are deferred until supervised
  long-lived workers or sub-agents exist. MCP child processes are currently
  incarnation-owned resources and reconnect on restart.
- Idle eviction and a process-persistent open-session roster are deferred. A
  new RPC connection explicitly opens the durable sessions it needs.
- Supervision requires `panic = "unwind"`; `panic = "abort"` cannot recover an
  actor panic and is not a supported host build mode.
