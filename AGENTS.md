# AGENTS.md

This file defines how coding agents should work in this repository.

## Project Intent

Build a minimal Rust reimplementation of core pi-mono ideas under `rho-*` naming.

Current MVP priorities:
- Multi-provider LLM support (OpenAI + Anthropic)
- Agent runtime as a library (`rho-agent`)
- Remote-capable TUI over websockets (`rho-tui` <-> `rho-agent`)
- Top-level CLI command `rho` from `bins/rho`

## Non-Negotiable Architecture

- Shared protocol/contracts live in `crates/rho-core`.
- Provider implementations are modules under `rho-core`:
  - `rho_core::providers::openai`
  - `rho_core::providers::anthropic`
- Agent runtime is a library crate: `crates/rho-agent`.
- TUI is a library crate: `crates/rho-tui`.
- CLI binary crate lives in `bins/rho` and exposes `rho`.
- API key env vars are only:
  - `OPENAI_API_KEY`
  - `ANTHROPIC_API_KEY`
- Do not introduce `RHO_*` aliases for API keys.

## Source of Truth

- Parent tracking issue: [#8](https://github.com/ralexstokes/rho/issues/8)
- MVP child issues: [#1](https://github.com/ralexstokes/rho/issues/1) to [#7](https://github.com/ralexstokes/rho/issues/7)
- Milestone: `mvp-remote-chat`
- Local handoff summary: `mvp-plan.md`

If a task conflicts with these constraints, stop and ask before implementing.

## Preferred Execution Order

1. `#1` workspace + `rho-core` contracts.
2. `#2` OpenAI and `#3` Anthropic in parallel.
3. `#4` agent loop + tools.
4. `#5` websocket server and `#6` TUI websocket client in parallel.
5. `#7` CLI wiring and end-to-end integration.

## Branch and PR Conventions

- Branch names must start with `codex/`.
- Keep each PR scoped to a single issue.
- Reference issue number in PR title/body.
- Rebase frequently on `main`, especially after `#1`.

Suggested names:
- `codex/issue-1-rho-core-scaffold`
- `codex/issue-2-openai-provider`
- `codex/issue-3-anthropic-provider`
- `codex/issue-4-agent-loop-tools`
- `codex/issue-5-agent-ws-server`
- `codex/issue-6-tui-ws-client`
- `codex/issue-7-cli-e2e`

## Coding Guidelines

- Keep code minimal and explicit; prefer simple abstractions over framework-heavy patterns.
- Preserve clean crate boundaries:
  - `rho-core` has shared types/protocol/provider interfaces.
  - `rho-agent` owns orchestration, session state, tools, and server runtime.
  - `rho-tui` is a client/UI that consumes server events.
  - `bins/rho` is wiring only.
- Avoid leaking provider-specific API shapes outside `rho-core`.
- Use typed protocol messages for websocket communication, versioned from day one.
- Prefer additive changes; do not refactor unrelated areas in MVP issues.

## Tooling and Validation

Run before opening a PR:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo check --workspace
cargo test --workspace
```

If using `just`, equivalent targets exist:

```sh
just fmt
just lint
just check
just test
```

## Websocket Contract Expectations (MVP)

`rho-agent` is authoritative for session state and tool execution.

Minimum message flow:
- Client -> Server: connect/start session, user message, cancel
- Server -> Client: session ack, assistant delta, tool started, tool completed, final, error

MVP can omit auth/TLS, but do not block adding them later (keep transport boundaries clean).

## Built-in Tools (MVP)

`rho-agent` should support:
- `read`
- `write`
- `bash`

Tool failures must be surfaced as structured events, not panics.

## Definition of Done (MVP)

- `rho serve` and `rho tui` work end-to-end across machines.
- OpenAI and Anthropic both function through shared `rho-core` abstractions.
- Tool loop supports `read`/`write`/`bash`.
- README includes a quickstart for local and cross-machine use.

## Cross-Machine Smoke Test

Machine A:

```sh
OPENAI_API_KEY=... rho serve --bind 0.0.0.0:8787 --provider openai --model <model>
```

Machine B:

```sh
rho tui --url ws://<machine-a-host>:8787/ws
```

Verify streaming text, tool events, and final response.

