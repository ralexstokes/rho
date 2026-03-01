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

## Handoff

Before committing, make sure all of the primary checks pass.
