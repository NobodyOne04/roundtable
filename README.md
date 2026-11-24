# roundtable

A terminal console where Claude Code, Codex and free models sit at one table, and you watch them talk.

> **Status: early build.** The architecture is decided in [`docs/adr/`](docs/adr/README.md).
> Built so far: a chat with Claude Code and Codex, streaming replies, a `writer` that may
> edit files, the transcript on disk, and a live model registry (`/models`,
> `/model enable|disable|use`).
> See [Roadmap](#roadmap).

## What it is

One chat, several participants:

- **Agents**: Claude Code and Codex, driven through their own CLIs and your own logins.
  They edit files and run tools.
- **Models**: any OpenAI-compatible endpoint: Ollama, LM Studio, OpenRouter free models,
  Groq, Gemini. Chat only: they answer, review and give second opinions; they don't edit
  files or start conversations with others.

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
  With `@all`, every enabled participant answers you independently.
- Slash commands like Claude Code: `/help`, `/usage`, `/context`, `/status`, `/models`,
  `/model enable|disable|use|test`, `/talk on|off|allow|deny|show`, `/budget`, `/turns`,
  `/resume`, `/clear`. Full list: [ADR 0008](docs/adr/0008-slash-commands-and-mentions.adr).
- Add any free model with a few lines of config, no rebuild, no restart: the registry is
  hot, so edits to the file or `/model …` commands apply at once. An edit with an error
  is not applied at all; the chat shows the error and the old registry stays live.
- Disable a model at any time: its running reply and queued messages are cancelled, and
  it keeps its history for when you enable it again.
- Model-to-model talk with a policy matrix (who may ask whom), turn and token limits, and
  a global off switch.
- One writer per checkout: only the `writer` agent edits files, one turn at a time across
  all threads; everyone else is read-only and runs without your own MCP servers and hooks,
  which could write. Each non-writer will get its own git worktree, so nothing it runs can
  touch the writer's files (decided in ADR 0004, details in a later ADR).
- Every message, handoff and blocked call is kept as plain JSONL files, one folder per
  thread (`tail -f` it to watch live): replay, resume, usage per model. A thread is tied to
  the checkout it started in; resumed elsewhere, it is read-only.

## Config

`models.toml` in your config dir (Linux: `~/.config/roundtable/`, macOS:
`~/Library/Application Support/roundtable/`), written on first run with `claude` and `codex`
and commented. Edit it while roundtable runs: changes apply within a fraction of a second.
A file with any error is not applied; the chat shows the error with its line and the
previous registry stays. `/model enable|disable|use <id>` change it from the chat, keeping
your comments. Example with a free local model added:

```toml
default = "claude"
writer = "claude"   # the only one that edits files

[[model]]
id = "claude"
kind = "cli"
cmd = "claude"
role = "lead"
enabled = true
# model = "claude-opus-5-5"         # passed to the CLI
# permission_mode = "acceptEdits"   # only for the writer's own turns; bypass is refused

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
`key = "env:<VAR>"`; a literal key is refused, and so is a key over plain `http://` to a
non-local host. Changing a model's `cmd`, `base_url` or `model` makes it a new destination:
its old session and history are not carried over.

## Build and run

```sh
cargo build --release
./target/release/roundtable          # Ctrl-C twice, or Ctrl-D on an empty line, to quit
./target/release/roundtable --outside-repo   # let Codex run outside a git repo
```

Type to talk to Claude; start a line with `@codex` or `@all` to address others. `Esc`
cancels running replies.

**Who may edit.** Only the `writer` from `models.toml` (default `claude`), and only when a
message goes to it alone: the chat shows `you → claude (may edit)`. It runs with your own
setup and the `permission_mode` you give it (`acceptEdits` lets Claude edit without asking;
unset means a safe explicit default, never what your own CLI config says: Claude
`default`, which refuses edits it can't ask about, and Codex `workspace-write`). Everyone else, and the
writer itself when reached through `@all` or several mentions, is read-only: plan mode for
Claude, the read-only sandbox for Codex, both without your own MCP servers, hooks and
plugins.

**After a crash.** If roundtable dies while the writer works, the CLI may keep editing.
On the next start in that checkout, edits stay locked (every turn read-only) until you
check nothing is running and type `/quiet`.

## Requirements

- macOS or Linux.
- The agent CLIs, installed and logged in. Tested with Claude Code 2.1.331 and
  codex-cli 0.171.0; their JSON output changes between versions, so others may misbehave.
- Start it inside a git repo; outside one, Codex is unavailable unless you pass
  `--outside-repo`. Codex is also unavailable in a project that has its own
  `.codex/config.toml` (here or in a parent folder): such a config can start MCP servers
  or run programs that a read-only turn couldn't stop.
- One roundtable at a time; a second instance exits with "already running".
- Planned (step 6): Ollama or LM Studio for free local models.

## Privacy

Prompts and replies are stored locally as JSONL files under `roundtable/threads/` in your
data dir, because the
transcript is the product. Configured API keys are redacted before anything is written;
secrets you paste into the chat yourself are not detected. Content goes only to the
participants you address **and** to those they are allowed to `ask` under `[talk]`, which
may be hosted models. `/talk off` stops all indirect delivery.

## Stack

Rust, tokio, ratatui, plain JSONL files for the transcript, MCP for the `ask` tool. Why each:
[`docs/adr/`](docs/adr/README.md).

## Roadmap

Built in dependency order, one ADR per PR ([index](docs/adr/README.md)):

| Step | ADR | What you get | State |
|---|---|---|---|
| 1 | [0001](docs/adr/0001-rust-tokio-ratatui.adr) | Skeleton: toolchain, terminal safety, CI | done |
| 2 | [0002](docs/adr/0002-single-owner-event-loop.adr) | Event loop: one owner of state, events in, effects out | done |
| 3 | [0010](docs/adr/0010-filesystem-event-log.adr) | Transcript as JSONL files, single-instance lock | done |
| 4 | [0004](docs/adr/0004-cli-agent-adapters.adr) | Claude Code and Codex in the chat, read-only until 0011 | done |
| 5 | [0011](docs/adr/0011-hot-model-registry.adr) | Hot model registry, `writer`, enable/disable | done |
| 6 | [0005](docs/adr/0005-openai-compatible-models.adr) | Free models over the OpenAI-compatible API | next |
| 7 | [0007](docs/adr/0007-model-to-model-communication.adr) | Model-to-model `ask`, talk policy, limits, `/talk on\|off` | |
| 8 | [0008](docs/adr/0008-slash-commands-and-mentions.adr) | Full command set and completion | |
| 9 | [0009](docs/adr/0009-tui-layout.adr) | Three-pane layout | |

Later, each with its own ADR first: git worktree per agent, `/model add` (guided),
`/compact`, `/handoff`, `/fork`, approvals, an ACP adapter.

## License

Not chosen yet.
