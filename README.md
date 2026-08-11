# rho

A minimal, extensible coding-agent harness in Rust.

Bootstrapped from [rust-nix-template](https://github.com/ralexstokes/rust-nix-template).

## Workspace

The workspace follows the pure-core/mutable-shell split in
[`spec/06-implementation.md`](spec/06-implementation.md):

- Pure cores: `rho-ai`, `rho-core`, and `rho-codec-jsonl`.
- Boundary traits and reference implementations: `rho-store` and `rho-tools`.
- Shells and integrations: `rho-ai-anthropic`, `rho-ai-openai`, `rho-agent`,
  `rho-rpc`, `rho-cli`, `rho-ext`, `rho-ext-wasm`, and `rho-shelterwood`.

Internal dependencies are declared once at the workspace root. Core crates
depend only on other cores; integrations point inward toward those cores.

## Phase 1

The provider boundary and walking skeleton are implemented:

- `rho-ai` defines stateless requests, authoritative messages, streaming
  events, cancellation, credentials, strict JSON-Schema argument validation,
  and a deterministic `faux` provider.
- `rho-ai-openai` wraps the lower `nanocodex-oai-api` layer. Each request gets
  a fresh session with the complete rho transcript and a one-attempt retry
  budget. Nanocodex 0.3 currently constrains this adapter to `gpt-5.6-sol` and
  does not expose its request's `max_output_tokens` field. The adapter handles
  provider-reported length truncation safely, but cannot yet enforce rho's
  requested hard output limit; that requires an upstream hook or the fallback
  hand-rolled transport described in the provider spec.
- `rho-ai-anthropic` is a direct Messages HTTP/SSE adapter with a pure,
  incremental decoder and fixture-tested text, thinking, tool-use, usage, and
  stop-reason assembly.
- `rho-cli` runs one logical turn with a `bash` tool, continuing through tool
  calls until the provider completes the turn. It defaults to a 32-model-step
  safety cap; use `--max-model-steps N` to choose a different positive limit.

Run the walking skeleton with an environment credential:

```sh
export OPENAI_API_KEY=...
./tools/dev cargo run -p rho-cli -- "inspect this repository"

export ANTHROPIC_API_KEY=...
./tools/dev cargo run -p rho-cli -- \
  --provider anthropic "inspect this repository"
```

Environment variables take precedence over `~/.rho/credentials.json` (or the
path in `RHO_CREDENTIALS_FILE`). The file shape is:

```json
{
  "openai": { "type": "api_key", "api_key": "..." },
  "anthropic": { "type": "api_key", "api_key": "..." }
}
```

The Phase-1 `bash` tool runs `bash -lc` in the current directory without rho
permission prompts or sandboxing. Run it only inside an execution environment
whose filesystem, process, and network access you are willing to grant to the
model.

## Getting started

All tooling comes from the Nix devshell — see `AGENTS.md` for the contract.

```sh
direnv allow                  # interactive shells; agents use ./tools/dev
./tools/dev just ci           # fast local CI mirror
./tools/dev just ci-nix       # authoritative clean lane (nix flake check)
```
