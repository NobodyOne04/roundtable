# roundtable

A terminal console where Claude Code, Codex and free models sit at one table, and you watch them talk.

> **Status: design phase.** No code yet. The architecture is drafted as ADRs in
> [`docs/adr/`](docs/adr/README.md), all *Proposed*, none approved.

## What it is

One chat, several participants:

- **Agents**: Claude Code and Codex, driven through their own CLIs and your own logins.
  They edit files and run tools.
- **Models**: any OpenAI-compatible endpoint: Ollama, LM Studio, OpenRouter free models,
  Groq, Gemini. They answer, review and give second opinions.

What makes it different from running them side by side: participants can **ask each other**
(`claude → codex`), and every exchange shows inline in the chat with the exact prompt sent,
the reply, and the tokens it cost.

```
┌ threads ─┬ conversation ──────────────────────────────┬ context ────────┐
│ #auth    │ you    › fix the login bug                 │ claude  ? 41k ↑ │
│ #review  │ claude › plan…                             │ codex   ? 12k ↑ │
│          │   ↳ claude → codex  [handoff ▸]            │ llama ██░ ~38%  │
│          │ claude › applying fix  [tool: edit ▸]      │ (disabled)      │
├──────────┴────────────────────────────────────────────┴─────────────────┤
│ > @codex review this                                    talk:on 2/6 turns│
└─────────────────────────────────────────────────────────────────────────┘
```

## Planned features

- `@claude`, `@codex`, `@all` to address participants; no mention goes to the default.
- Slash commands like Claude Code: `/help`, `/usage`, `/context`, `/status`, `/models`,
  `/model enable|disable|use|test`, `/talk on|off|allow|deny|show`, `/budget`, `/turns`,
  `/resume`, `/clear`. Full list: [ADR 0008](docs/adr/0008-slash-commands-and-mentions.adr).
- Add any free model with a few lines of config, no rebuild.
- Disable a model at any time: it stops receiving messages, keeps its history.
- Model-to-model talk with a policy matrix (who may ask whom), turn and token limits, and
  a global off switch.
- One writer per checkout: only the `writer` agent edits files, one turn at a time; everyone else is read-only.
  Read-only turns also run without your own MCP servers and hooks, which could write.
- Every message, handoff and blocked call is kept in a local SQLite log: replay, resume,
  usage per model.

## Config preview

`~/.config/roundtable/models.toml` (macOS: `~/Library/Application Support/roundtable/`):

```toml
default = "claude"
writer = "claude"   # the only one that edits files

[[model]]
id = "claude"
kind = "cli"
cmd = "claude"
role = "lead"
enabled = true

[[model]]
id = "codex"
kind = "cli"
cmd = "codex"
role = "reviewer"
enabled = true

[[model]]
id = "llama"
kind = "openai"
base_url = "http://localhost:11434/v1"
model = "llama3.3"
context_window = 128000
enabled = true

[talk]
enabled = true
max_turns = 6
budget_tokens = 50000
allow = { claude = ["codex", "llama"], codex = [] }
```

API keys are never written here: use `key = "keyring:<name>"` (OS keychain) or
`key = "env:<VAR>"`.

## Requirements (planned)

- macOS or Linux.
- The agent CLIs you want at the table, installed and logged in: `claude`, `codex`.
- For free local models: Ollama or LM Studio.

## Privacy

Prompts and replies are stored locally in `roundtable.db` in your data dir, because the
transcript is the product. Configured API keys are redacted before anything is written;
secrets you paste into the chat yourself are not detected. Content goes only to the
participants you address **and** to those they are allowed to `ask` under `[talk]`, which
may be hosted models. `/talk off` stops all indirect delivery.

## Stack

Rust, tokio, ratatui, SQLite (bundled), MCP for the `ask` tool. Why each:
[`docs/adr/`](docs/adr/README.md).

## Roadmap

1. **MVP**: chat with `@claude` / `@codex`, streaming, event log, `/help` `/models` `/status`.
2. OpenAI-compatible models, enable/disable, `/usage` `/context`.
3. Model-to-model `ask`, talk policy, limits, inline handoffs.
4. Resume, guided `/model add`, `/compact`, `/handoff`, `/fork`.
5. Git worktree per agent, approvals, ACP adapter.

## License

Not chosen yet.
