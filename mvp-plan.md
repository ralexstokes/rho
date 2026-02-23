RHO MVP HANDOFF

Last updated: 2026-02-23
Repo: https://github.com/ralexstokes/rho

Goal
- Build a minimal Rust reimplementation of pi-mono concepts with rho naming.
- Must support OpenAI and Anthropic.
- Must support remote operation: rho-tui connects to rho-agent over websockets.

Architecture Constraints (agreed)
- Shared contracts + providers live in: crates/rho-core
- Providers are modules under one crate:
  - rho_core::providers::openai
  - rho_core::providers::anthropic
- Agent runtime is a library crate: crates/rho-agent
- TUI is a library crate: crates/rho-tui
- Top-level command is in: bins/rho
- Env vars for keys:
  - OPENAI_API_KEY
  - ANTHROPIC_API_KEY
- No RHO_* API key aliases

GitHub Tracking
- Milestone: mvp-remote-chat
- Parent tracker: https://github.com/ralexstokes/rho/issues/8

Child issues:
1) https://github.com/ralexstokes/rho/issues/1
   MVP: scaffold workspace and define rho-core contracts
2) https://github.com/ralexstokes/rho/issues/2
   MVP: implement OpenAI provider module in rho-core
3) https://github.com/ralexstokes/rho/issues/3
   MVP: implement Anthropic provider module in rho-core
4) https://github.com/ralexstokes/rho/issues/4
   MVP: implement rho-agent loop and built-in tools
5) https://github.com/ralexstokes/rho/issues/5
   MVP: add websocket server transport to rho-agent
6) https://github.com/ralexstokes/rho/issues/6
   MVP: implement rho-tui websocket client UI
7) https://github.com/ralexstokes/rho/issues/7
   MVP: wire bins/rho CLI and end-to-end flow

Current Execution Status
- [x] #1 merged (completed on 2026-02-23)
- [x] #2 merged (completed on 2026-02-23)
- [x] #3 merged (completed on 2026-02-23)
- [ ] #4 pending
- [ ] #5 pending
- [ ] #6 pending
- [ ] #7 pending

Recommended Execution Order
1) Issues #1, #2, and #3 are complete (contracts + providers)
2) Issue #4 next (agent loop + tools)
3) Issues #5 and #6 in parallel (transport + tui client)
4) Issue #7 last (full wiring and e2e)

Suggested Branch Naming
- codex/issue-1-rho-core-scaffold
- codex/issue-2-openai-provider
- codex/issue-3-anthropic-provider
- codex/issue-4-agent-loop-tools
- codex/issue-5-agent-ws-server
- codex/issue-6-tui-ws-client
- codex/issue-7-cli-e2e

Per-Issue Acceptance Summary
- #1
  - cargo check --workspace passes
  - rho-core exports websocket protocol + unified stream/tool/message types
  - provider trait + module stubs compile
- #2
  - OpenAI module emits unified stream events
  - clear error on missing/invalid OPENAI_API_KEY
- #3
  - Anthropic module emits unified stream events
  - clear error on missing/invalid ANTHROPIC_API_KEY
- #4
  - rho-agent library runs prompt -> tool call(s) -> final answer
  - built-in tools: read, write, bash
- #5
  - websocket server in rho-agent supports connect/message/cancel
  - streams assistant deltas + tool lifecycle events
- #6
  - rho-tui connects to remote websocket server
  - renders streaming responses and tool events cleanly
- #7
  - bins/rho command works:
    - rho serve --bind ... --provider ... --model ...
    - rho tui --url ws://.../ws
  - cross-machine smoke test documented

Definition of Done (MVP)
- rho serve and rho tui work end-to-end across machines
- both OpenAI + Anthropic work through unified rho-core abstractions
- tool loop supports read/write/bash in rho-agent
- README quickstart includes setup + remote workflow

Validation Baseline (all PRs)
- cargo fmt --all --check
- cargo clippy --workspace --all-targets -- -D warnings
- cargo check --workspace
- cargo test --workspace

Cross-Machine Smoke Test (target)
1) On machine A:
   OPENAI_API_KEY=... rho serve --bind 0.0.0.0:8787 --provider openai --model <model>
2) On machine B:
   rho tui --url ws://<machine-a-host>:8787/ws
3) Send a prompt, verify:
   - streaming text appears
   - tool events appear
   - final response arrives

Notes for Coordinating Multiple Threads
- Keep PRs single-issue scoped.
- Rebase onto main frequently, especially before starting #4, #5, or #6.
- Treat rho-core contracts/providers from #1-#3 as authoritative for downstream work.
