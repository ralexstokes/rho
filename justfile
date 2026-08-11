# The recipes used by `ci` mirror the checks built by the `rust-env` flake
# input (the authoritative CI definitions, run via `just ci-nix`) — keep them
# in sync when overriding anything in flake.nix.

default:
    @just --list

fmt:
    cargo +nightly fmt --all --check

lint:
    cargo +nightly clippy --locked --workspace --all-targets --all-features -- -D warnings

check:
    cargo check --locked --workspace --all-targets --all-features

build:
    cargo build --locked --workspace --all-targets --all-features

test:
    cargo nextest run --locked --workspace --all-features --lib --bins --tests --examples
    cargo test --locked --workspace --doc --all-features

doc-check:
    RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --no-deps --all-features

nixfmt-check:
    git ls-files -z '*.nix' | xargs -0 nixfmt --check

pure-deps:
    ./tools/check-pure-deps

# Fast local CI mirror — reuses the local cargo cache and incremental builds.
# The clean Nix lane retains the explicit all-target build for non-test codegen coverage.
ci: fmt lint test doc-check nixfmt-check pure-deps

# Full clean Nix CI lane; use before pushing or when touching Nix files.
ci-nix:
    nix flake check --no-update-lock-file

doc:
    cargo doc --workspace --no-deps --open
