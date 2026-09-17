# duet

A terminal UI for running [`claude`](https://claude.com/claude-code) and
[`codex`](https://github.com/openai/codex) side by side in named, persistent
tabs — with a way to hand a conversation off from one agent to the other.

Each tab owns a real pty running one agent. Tabs survive restarts (resumed
via each CLI's own session mechanism). Switching the agent inside a tab asks
the outgoing agent to summarize the conversation, then opens the incoming
agent with that summary as its first prompt — a real handoff, not shared
context (Claude and Codex don't share a session store).

See [`docs/superpowers/specs/2026-09-17-duet-design.md`](docs/superpowers/specs/2026-09-17-duet-design.md)
for the full design.

## Build

```sh
cargo build --release
```

## Run

```sh
cargo run
```

Requires `claude` and `codex` on `PATH`.
