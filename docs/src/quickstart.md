# Quickstart

1. Clone repo:

```sh
git clone https://github.com/ralexstokes/rho
```

2. Set an API key for a supported LLM provider, and run with:

```sh
ANTHROPIC_API_KEY=... cargo run
```
## Notes

See the help output for further options, including support providers and models.

```sh
cargo run -- --help
```

Override local mode options:

```sh
OPENAI_API_KEY=... cargo run -- \
  --provider openai \
  --model gpt-5.2-2025-12-11
```

### Run client and server separately

Run server with OpenAI:

```sh
OPENAI_API_KEY=... cargo run -- serve \
  --bind 127.0.0.1:8787 \
  --provider openai \
  --model gpt-5.2-2025-12-11
```

or Anthropic:

```sh
ANTHROPIC_API_KEY=... cargo run -- serve \
  --bind 127.0.0.1:8787 \
  --provider anthropic \
  --model claude-sonnet-4-6
```

Attach a TUI client:

```sh
cargo run -- attach --url ws://127.0.0.1:8787/ws
```

Adjust IPs and ports as appropriate to run across separate machines.
