# rho ρ

Minimal Rust reimplementation of core pi-mono ideas with:
- shared provider/protocol contracts in `rho-core`
- agent runtime + websocket server in `rho-agent`
- websocket TUI client in `rho-tui`
- top-level CLI in `rho`

## Quickstart

Run server (Machine A) with OpenAI:

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

## License

This project is dual-licensed under either of:
- MIT License ([`LICENSE-MIT`](LICENSE-MIT))
- Apache License, Version 2.0 ([`LICENSE-APACHE`](LICENSE-APACHE))

at your option.
