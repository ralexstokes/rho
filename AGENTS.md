# Project instructions

## Running anything

All tooling (`cargo`, `just`, `nextest`, `nixfmt`) comes from the Nix
devshell. It is **not** on the base PATH. Prefix commands with `./tools/dev`:

```sh
./tools/dev just ci          # the full local CI mirror
./tools/dev just test
./tools/dev cargo nextest run --workspace
```

`./tools/dev` exec's straight through when the devshell for *this* checkout is already
active, so it is free in an interactive shell and correct everywhere else. Use
it rather than assuming direnv has loaded — see below for why.

`just ci` is the local mirror of CI; `just ci-nix` runs the authoritative clean
Nix lane. CI is defined by the `rust-env` flake input (`mkRustProject` from
rust-nix-template) plus any overrides in `flake.nix`, and the `justfile`
recipes mirror those checks — keep the two in sync when changing either.

## Worktrees

Create them under `.worktrees`.

`./tools/dev` gives any worktree the correct toolchain no matter how it was created:

- direnv never loads in non-interactive (agent) shells, and `direnv allow` is
  keyed on the absolute `.envrc` path, so a fresh worktree has no toolchain on
  the base PATH at all.
- A shell spawned from another checkout inherits *that* checkout's devshell, so
  a worktree can appear to work while silently using the wrong toolchain.
  `./tools/dev` detects this via `REPO_DEVSHELL` and re-enters the right one.
