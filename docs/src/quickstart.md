# Quickstart

1. Clone repo:

```sh
git clone https://github.com/ralexstokes/rho
```

2. Set an API key for a supported LLM provider, and run with:

```sh
OPENAI_API_KEY=... cargo run -p rho
```
## Notes

See the help output for further options, including support providers and models.

```sh
cargo run -p rho -- --help
```

Override local mode options:

```sh
ANTHROPIC_API_KEY=... cargo run -p rho -- \
  --provider anthropic \
  --model claude-sonnet-4-6 \
  --bind 127.0.0.1:8787
```

### Run client and server separately

Run server (Machine A) with OpenAI:

```sh
OPENAI_API_KEY=... cargo run -p rho -- serve \
  --bind 0.0.0.0:8787 \
  --provider openai \
  --model gpt-5.2-2025-12-11
```

or Anthropic:

```sh
ANTHROPIC_API_KEY=... cargo run -p rho -- serve \
  --bind 0.0.0.0:8787 \
  --provider anthropic \
  --model claude-sonnet-4-6
```

Run TUI client (Machine B):

```sh
cargo run -p rho -- tui --url ws://<machine-a-host>:8787/ws
```
