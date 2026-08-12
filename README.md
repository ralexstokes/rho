# rho

A durable, headless coding-agent harness in Rust. rho can run one prompt from
the shell or serve a versioned JSON Lines protocol for a long-lived client.

The phase-3 host includes:

- OpenAI and Anthropic providers behind a transcript-authoritative boundary.
- Durable, forkable JSONL sessions with recovery, compaction, steering,
  follow-ups, cancellation, hooks, and headless interactions.
- The `read`, `write`, `edit`, and `bash` coding tools, plus client-launched MCP
  stdio servers.
- Text and machine-readable one-shot modes, and RPC over stdio or a local Unix
  socket.
- Shelterwood-supervised session actors that reopen incarnation-owned provider,
  tool, and MCP state and resume suspended journals after a crash.

## Run rho

All development tooling comes from the Nix devshell:

```sh
./tools/dev cargo build -p rho-cli --bin rho
export OPENAI_API_KEY=...
./target/debug/rho "inspect this repository and run its tests"
```

The prompt alias above is equivalent to an explicit run, which can also choose
the agent's working directory:

```sh
./target/debug/rho run --cwd /path/to/project "fix the failing tests"
```

Text mode streams assistant text to stdout and writes the durable session ID to
stderr. `--json` instead emits versioned `agent.event` JSON Lines followed by a
final authoritative `session.snapshot` event:

```sh
./target/debug/rho --json "summarize the repository" > run.jsonl
```

Sessions default to `~/.rho/sessions`. Override that location with
`--sessions-dir PATH` or `RHO_SESSIONS_DIR`.

## Credentials and configuration

Environment credentials take precedence over `~/.rho/credentials.json` (or
`RHO_CREDENTIALS_FILE`):

```json
{
  "openai": { "type": "api_key", "api_key": "..." },
  "anthropic": { "type": "api_key", "api_key": "..." }
}
```

The optional `~/.rho/config.json` is strict JSON. An absent default file uses
the defaults shown below; an explicit `--config` or `RHO_CONFIG_FILE` path must
exist. For Anthropic, set `provider` to `anthropic` and choose one of
`claude-fable-5`, `claude-opus-5`, or `claude-sonnet-5`.

```json
{
  "provider": "openai",
  "model": "gpt-5.6-luna",
  "thinking": "high",
  "max_output_tokens": 16384,
  "system": "You are a coding agent. Inspect, edit, validate, and report concisely.",
  "compaction": {
    "threshold_tokens": 100000,
    "retain_messages": 20,
    "system_prompt": "Preserve decisions, constraints, exact paths, failures, and remaining work."
  },
  "mcp": [
    {
      "name": "project",
      "command": "my-mcp-server",
      "args": [],
      "env": {},
      "request_timeout_seconds": 60,
      "probe_timeout_millis": 1000
    }
  ]
}
```

An MCP entry without `cwd` inherits each session's working directory. A fixed
`cwd` can be supplied explicitly. rho snapshots the server's tools when the
session attaches and exposes them as `mcp__<server>__<tool>`.

## RPC host

Serve the [rho RPC v1 protocol](spec/03-rpc.md) on stdin/stdout:

```sh
./target/debug/rho rpc
```

Or serve one controlling connection at a time on a permission-restricted Unix
socket:

```sh
./target/debug/rho rpc --listen /tmp/rho.sock
```

Every frame is one JSON object followed by LF. A minimal request is:

```json
{"v":1,"id":"list","method":"session.list","params":{}}
```

The host supports session create/open/list/fork/delete/snapshot, prompt,
steering and follow-up queues, queued-message cancellation, abort, explicit
compaction, and provider/model/thinking reconfiguration. Commands are accepted
asynchronously; `agent.event` reports progress and `session.snapshot` is the
durable truth.

## Security stance

rho's v1 tools are intentionally unsandboxed and have no built-in permission
gate. `bash` executes commands and the file tools accept absolute paths. MCP
processes inherit the host environment plus configured overrides. Run rho in a
container, sandbox, VM, or user account whose filesystem, process, credential,
and network authority you are willing to grant to the model.

## Workspace

The workspace follows the pure-core/mutable-shell split in
[`spec/06-implementation.md`](spec/06-implementation.md):

- Pure cores: `rho-ai`, `rho-core`, and `rho-codec-jsonl`.
- Boundary traits and implementations: `rho-store` and `rho-tools`.
- Shells and integrations: `rho-ai-anthropic`, `rho-ai-openai`, `rho-agent`,
  `rho-rpc`, `rho-cli`, `rho-ext`, `rho-ext-wasm`, and `rho-shelterwood`.

```sh
direnv allow                  # interactive shells; agents use ./tools/dev
./tools/dev just ci           # local CI mirror
./tools/dev just ci-nix       # authoritative clean Nix lane
```
