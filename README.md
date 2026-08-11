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

## Getting started

All tooling comes from the Nix devshell — see `AGENTS.md` for the contract.

```sh
direnv allow                  # interactive shells; agents use ./tools/dev
./tools/dev just ci           # fast local CI mirror
./tools/dev just ci-nix       # authoritative clean lane (nix flake check)
```
