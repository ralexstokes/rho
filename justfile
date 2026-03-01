default:
    @just --list

fmt:
    cargo +nightly fmt --all --check

lint:
    cargo +nightly clippy --workspace --all-targets --all-features -- -D warnings

check:
    cargo check --workspace

build:
    cargo build --workspace

test:
    cargo test --workspace

ci: fmt lint build test

build-book:
    mdbook build docs

serve-book:
    mdbook serve docs
