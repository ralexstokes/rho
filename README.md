# rho

A minimal, extensible coding-agent harness in Rust.

Bootstrapped from [rust-nix-template](https://github.com/ralexstokes/rust-nix-template).

## Getting started

All tooling comes from the Nix devshell — see `AGENTS.md` for the contract.

```sh
direnv allow                  # interactive shells; agents use ./tools/dev
./tools/dev just ci           # fast local CI mirror
./tools/dev just ci-nix       # authoritative clean lane (nix flake check)
```
