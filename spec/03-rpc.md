# rho RPC v1

Status: **DECIDED for phase 3**. This protocol is rho-specific; it borrows the
debuggability of pi's RPC shape but is not wire-compatible with it.

## 1. Transport and framing

The same byte protocol runs over stdio and Unix domain sockets. Each frame is
one UTF-8 JSON object followed by LF; CRLF is accepted from clients. A frame may
not contain physical embedded newlines (JSON string newlines remain escaped)
and is limited to 16 MiB. Malformed JSON, invalid envelope shape, oversized
frames, or a version mismatch on a client response close the connection.

There is no protocol authentication in v1. stdio inherits the launching user's
authority. A Unix socket host must rely on its containing directory and socket
permissions. Remote transports, authentication, and binary framing belong to a
later remote stack.

The server has a bounded 256-message outbound queue. Producers await capacity,
so a slow client applies backpressure instead of causing unbounded memory use.
Method handlers run concurrently; response and event ordering across distinct
requests is intentionally unspecified. IDs provide correlation.

## 2. Envelopes and versioning

Every frame carries `"v": 1`. An unsupported version on a client request gets a
normal `unsupported_version` response containing `{"supported":[1]}`. IDs are
either non-negative JSON integers or strings and are opaque to the receiver.

Client request:

```json
{"v":1,"id":"r1","method":"session.prompt","params":{"session_id":"…","text":"inspect the repo"}}
```

Success and failure responses:

```json
{"v":1,"id":"r1","ok":true,"result":{"accepted":true}}
{"v":1,"id":"r1","ok":false,"error":{"code":"busy","message":"session already has an active operation","data":null}}
```

An advisory event has no ID:

```json
{"v":1,"event":"agent.event","data":{"session_id":"…","event":{"kind":"operation_started","op":"…","origin":"external"}}}
```

The server can invert the request direction for a headless interaction. The
client answers with the ordinary response envelope; it does not call a second
method:

```json
{"v":1,"id":"permission-7","method":"interaction.answer","params":{"session_id":"…","prompt":"continue?","timeout_ms":30000}}
{"v":1,"id":"permission-7","ok":true,"result":{"answer":"approved"}}
```

A client failure response is durably normalized to `declined`; no response by
the core request deadline is `timed_out`. Unknown, duplicate, or mismatched
interaction IDs are rejected and surfaced as `rpc.client_response_rejected`.

Stable error codes are `invalid_request`, `invalid_params`, `method_not_found`,
`not_found`, `locked`, `busy`, `conflict`, `unsupported_version`, and `internal`.
Error messages are diagnostic, not control flow. Optional `data` is structured
context and may grow compatibly.

## 3. Methods

All session IDs and entry IDs use their durable string representations.
Commands return after validation and acceptance, not after an agent operation
finishes. Clients observe completion through `agent.event`, then obtain full
truth from `session.snapshot` or `session.get_snapshot`.

| Method | Required params | Result / rule |
|---|---|---|
| `session.create` | `cwd` | Creates and attaches a writer-locked session; returns its snapshot. |
| `session.open` | `session_id` | Attaches an existing session and resumes recoverable work before accepting a new prompt. |
| `session.list` | none | Returns cheap headers, leaves, and reduced lane states. |
| `session.fork` | `session_id`, optional `entry_id` | Forks through the leaf or named complete tool boundary and attaches the fork. |
| `session.delete` | `session_id` | Deletes an unlocked session. Active sessions return `locked`. |
| `session.get_snapshot` | `session_id` | Returns the authoritative lock-free snapshot, including while an operation owns the writer lock. |
| `session.prompt` | `session_id`, `text` | Starts one run. The lane must be idle. |
| `session.steer` | `session_id`, `text` | Queues input for the active logical run; returns `queue_id`. |
| `session.follow_up` | `session_id`, `text` | Queues the next run; returns `queue_id`. |
| `session.cancel_queued` | `session_id`, `queue_id` | Durably cancels pending queued input. |
| `session.abort` | `session_id` | Durably requests cooperative cancellation. |
| `session.compact` | `session_id` | Starts explicit compaction on an idle lane. |
| `session.configure` | `session_id`, optional `provider`, `model`, `thinking` | Appends a settings entry on an idle lane and reopens the provider if its selection changed. |

Unknown methods return `method_not_found`. Params are method-owned strict serde
objects; rho does not coerce strings to numbers, accept unknown fields, or infer
missing session IDs from connection state.

## 4. Authoritative snapshots

`session.get_snapshot` and every `session.snapshot` event return this complete
durable shape:

```json
{
  "header": {"v":1,"id":"…","created_at":"…","cwd":"/workspace","parent":null},
  "leaf": "entry-id-or-null",
  "status": "Idle | Suspended | Corrupt (serde representation)",
  "items": ["the complete interleaved entry/record/fact log"]
}
```

The repository's `inspect` operation reads this without acquiring the writer
lock. `items` is the entire append-only log, not merely the selected transcript
branch. Consequently `{header, leaf, items}` can reconstruct all v1 state and
`status` is a convenient deterministic reduction, not separate authority.

`agent.event` mirrors the owned `rho_core::AgentEvent` and is advisory for
rendering. In particular provider deltas may be dropped by a client. The server
emits `session.snapshot` after durable state changes and at operation terminals;
a reconnecting client starts with `session.get_snapshot`. No client must replay
deltas to recover truth.

## 5. Host lifecycle

One attached session actor owns its writer handle, provider session, tool set,
control receiver, and stamps. RPC methods only send it typed commands. The actor
runs `rho_agent::Driver`; RPC introduces no parallel decision loop.

The stdio host has one connection for the process lifetime. The Unix-socket host
accepts one controlling connection at a time in v1. Losing the controlling
connection requests abort on active operations and releases attached sessions;
durable recovery on the next `session.open` uses the same replay choreography as
an ordinary process crash.
