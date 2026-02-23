default:
    @just --list

fmt:
    cargo +nightly fmt --all --check

lint:
    cargo +nightly clippy --workspace --all-targets --all-features -- -D warnings

build:
    cargo build --workspace

ci: fmt lint build test

check:
    cargo check --workspace

test:
    cargo test --workspace

run:
    cargo run -p rho
