# AGENTS.md

This file defines how coding agents should work in this repository.

## Project Intent

`rho` is a minimal Rust reimplementation of core pi-mono ideas.

Current scope in this repo:
- Shared provider/protocol contracts in `crates/rho-core`
- Agent runtime + websocket server in `crates/rho-agent`
- Websocket TUI client in `crates/rho-tui`
- Top-level CLI binary in `bins/rho`

## Non-Negotiable Architecture

- Shared protocol/contracts live in `crates/rho-core`.
- Provider implementations are modules under `rho-core`:
  - `rho_core::providers::openai`
  - `rho_core::providers::anthropic`
- Agent runtime remains a library crate: `crates/rho-agent`.
- TUI remains a library crate: `crates/rho-tui`.
- CLI binary crate lives in `bins/rho` and exposes `rho`.
- API key env vars are only:
  - `OPENAI_API_KEY`
  - `ANTHROPIC_API_KEY`
- Do not introduce `RHO_*` aliases for API keys.

If a task conflicts with these constraints, stop and ask before implementing.

## Current Runtime Contract

- Websocket protocol is versioned from day one (`rho_core::protocol::PROTOCOL_VERSION`).
- `rho-agent` is authoritative for session state and tool execution.
- Minimum message flow:
  - Client -> Server: `start_session`, `user_message`, `cancel`
  - Server -> Client: `session_ack`, `assistant_delta`, `tool_started`, `tool_completed`, `final`, `error`
- Tool failures must be surfaced as structured events, not panics.

## Built-in Tools (Current)

`rho-agent` runtime supports:
- `read`
- `write`
- `bash`

Keep tool definitions and execution behavior aligned in `crates/rho-agent/src/runtime.rs`.

## CLI Surface (Current)

Top-level binary command: `rho`
- `rho serve --bind <addr> --provider <openai|anthropic> --model <model>`
- `rho tui --url <ws-url>`

Keep `bins/rho` as wiring only; core behavior belongs in crate libraries.

## Coding Guidelines

- Keep code minimal and explicit; prefer simple abstractions over framework-heavy patterns.
- Preserve clean crate boundaries:
  - `rho-core` has shared types/protocol/provider interfaces.
  - `rho-agent` owns orchestration, session state, tools, and server runtime.
  - `rho-tui` is a client/UI that consumes server events.
  - `bins/rho` is wiring only.
- Avoid leaking provider-specific API shapes outside `rho-core`.
- Prefer additive changes; do not refactor unrelated areas.

## Branch and PR Conventions

- Branch names must start with `codex/`.
- Keep each PR scoped to one coherent change.
- Reference the relevant issue (if any) in PR title/body.
- Rebase frequently on `main`.

## Tooling and Validation

Primary checks:

```sh
cargo +nightly fmt --all --check
cargo +nightly clippy --workspace --all-targets --all-features -- -D warnings
cargo build --workspace
cargo test --workspace
```

Fast dev check:

```sh
cargo check --workspace
```

`just` shortcuts:

```sh
just fmt
just lint
just build
just test
just ci
```
