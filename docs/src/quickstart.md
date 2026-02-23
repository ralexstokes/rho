# Quickstart

Run local mode (single process server + TUI):

```sh
OPENAI_API_KEY=... cargo run -p rho --
```

Override local mode options:

```sh
ANTHROPIC_API_KEY=... cargo run -p rho -- \
  --provider anthropic \
  --model claude-3-5-haiku-latest \
  --bind 127.0.0.1:8787
```

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
