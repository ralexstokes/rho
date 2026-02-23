# rho

Minimal Rust reimplementation of core pi-mono ideas with:
- shared provider/protocol contracts in `rho-core`
- agent runtime + websocket server in `rho-agent`
- websocket TUI client in `rho-tui`
- top-level CLI in `rho`

## Quickstart

Build once:

```sh
cargo build --workspace
```

Run server (Machine A):

```sh
OPENAI_API_KEY=... cargo run -p rho -- serve \
  --bind 0.0.0.0:8787 \
  --provider openai \
  --model gpt-4o-mini
```

or Anthropic:

```sh
ANTHROPIC_API_KEY=... cargo run -p rho -- serve \
  --bind 0.0.0.0:8787 \
  --provider anthropic \
  --model claude-3-5-haiku-latest
```

Run TUI client (Machine B):

```sh
cargo run -p rho -- tui --url ws://<machine-a-host>:8787/ws
```

Inside TUI:
- type a prompt and press Enter to send
- `/cancel` cancels an in-flight request
- `/quit` exits the client

## Cross-Machine Smoke Test

1. On Machine A, run `rho serve` with a valid provider key.
2. On Machine B, run `rho tui --url ws://<machine-a-host>:8787/ws`.
3. Send a prompt and verify:
   - assistant text streams incrementally
   - tool lifecycle events render
   - a final response arrives

## License

This project is dual-licensed under either of:
- MIT License ([`LICENSE-MIT`](LICENSE-MIT))
- Apache License, Version 2.0 ([`LICENSE-APACHE`](LICENSE-APACHE))

at your option.
