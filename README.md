# roundtable

A terminal console where Claude Code, Codex and free models sit at one table, and you watch them talk.

> **Status: early build.** The architecture is decided in [`docs/adr/`](docs/adr/README.md).
> Built so far: a chat with Claude Code, Codex and any OpenAI-compatible model (Ollama,
> OpenRouter, …), streaming replies, a `writer` that may edit files, the transcript on disk,
> a live model registry (`/models`, `/model enable|disable|use`), and models asking each
> other (`ask`, `/talk on|off`).
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
  With `@all`, every enabled participant answers you independently. `default = "all"`
  (or `/model use all`) makes every message without a mention an `@all`. `@all?` sends to
  everyone with nobody editing.
- Slash commands like Claude Code (`/help` lists them, `Tab` completes them and `@` ids):
  `/status`, `/usage [thread|today|all]`, `/context [id]`, `/models`,
  `/model add`, `/model enable|disable|use|test <id>`, `/talk on|off|show|allow a->b|deny a->b`,
  `/budget`, `/turns`, `/clear`, `/resume [n|id]`, `/log`, `/handoff`, `/compact [id]`,
  `/exit`, `/quiet`. `Alt-Enter` adds a line, `PgUp`/`PgDn` scroll, `\/` sends a literal
  slash. `/fork` comes later ([ADR 0008](docs/adr/0008-slash-commands-and-mentions.adr)).
- `/model add` adds a free model one question at a time
  ([ADR 0022](docs/adr/0022-model-add.adr)): provider (`ollama`, `lmstudio`, `openrouter`
  or `other` with its URL), a key reference if it needs one (`env:VAR` or
  `keyring:<name>`; the chat prints the command to store the key, which is never typed into
  roundtable), the model from the endpoint's list (or typed), an id, the context window.
  The URL is checked before any key goes to it, the whole file is validated with the new
  entry before it is written (comments kept), then `/model test` runs on it. `Esc` aborts
  with nothing written.
- `/compact [id]` keeps a free model's long thread from losing its start
  ([ADR 0021](docs/adr/0021-compact.adr)): the model summarizes its own history in chunks
  that fit its window, and from then on gets the summary, then the newer messages. Without
  an id, every free model with history. A failed or cut-off summary changes nothing;
  `/context` shows where it was compacted. When fitting would drop more than a quarter of
  the window, the chat suggests it once. Claude and Codex compact on their own.
- `/handoff <id> [note]` passes the thread on: the default (or the writer, with
  `default = "all"`; or `from <id>`) summarizes it read-only, `<id>` gets the summary and
  answers with its first step, then becomes the default (`writer` stays). If the summarizer
  hasn't seen parts of the thread, you're asked before they go to it. `/handoff <id> raw [n]`
  sends the last `n` messages verbatim instead, after asking.
- `/resume` reopens a past thread: the chat, each CLI's session and each free model's
  history come back, and new messages continue it. A thread from another checkout opens
  read-only.
- Add any free model with a few lines of config, no rebuild, no restart: the registry is
  hot, so edits to the file or `/model …` commands apply at once. An edit with an error
  is not applied at all; the chat shows the error and the old registry stays live.
- Disable a model at any time: its running reply and queued messages are cancelled, and
  it keeps its history for when you enable it again.
- Model-to-model talk with a policy matrix (who may ask whom), turn and token limits, and
  a global off switch.
- One writer per checkout: only the `writer` agent edits files, one turn at a time across
  all threads; everyone else is read-only and runs without your own MCP servers and hooks,
  which could write. Each non-writer runs in its own git worktree, synced with the writer's
  uncommitted work before every turn, so what slips past its flags lands there (ADR 0016).
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
editor = "nvim"     # /log opens the thread here; the temp file is deleted on exit

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

Free models answer in the chat like the agents, from their own history (what you sent them
and what they replied), trimmed oldest-first to fit `context_window` (default 8192) while
keeping `max_output_tokens` (default 4096) free for the reply. They never edit files.

API keys are never written here: use `key = "keyring:<name>"` (OS keychain, service
`roundtable`: `security add-generic-password -s roundtable -a <name> -w` on macOS) or
`key = "env:<VAR>"`. Keys are read for each request (shorter than 8 characters is refused)
and removed from everything roundtable shows, logs or saves, whichever participant printed
them. `connect_timeout_s` (default 10) and `idle_timeout_s` (default 120) tune the HTTP
timeouts; redirects are never followed; a literal key is refused, and so is a key over plain `http://` to a
non-local host. Changing a model's `cmd`, `base_url` or `model` makes it a new destination:
its old session and history are not carried over.

## Build and run

```sh
cargo build --release
./target/release/roundtable          # Ctrl-C twice, or Ctrl-D on an empty line, to quit
./target/release/roundtable --outside-repo   # let Codex run outside a git repo
```

Type to talk to Claude; start a line with `@codex` or `@all` to address others. `Esc`
cancels running replies and switches to Normal mode.

`/usage` starts with each CLI's plan limits, as Claude's `/usage` and Codex's `/status`
show them: how much of the 5-hour and weekly window is used and when it resets, from
Claude's stream and Codex's own session files (`~/.codex/sessions`, or `$CODEX_HOME`).
A window at 80% or more also shows in the status line.

**Keys, vim-style** ([ADR 0015](docs/adr/0015-vim-keys.adr)). Insert mode (the input box) types;
`Esc` goes to Normal mode, shown in the status line. Normal mode: `j`/`k` move between chat
lines, `Enter` or `o` expands a tool or handoff line, `y` copies the selected line (OSC 52:
needs `set-clipboard on` in tmux), `gg`/`G` top and bottom, `Ctrl-U`/`Ctrl-D` scroll, `h`/`l`
threads and chat, `x` cancels running replies, `?` help, `q` hides the side panes and quits
when pressed twice; `i` or `a` back to typing, `/` or `:` starts a command. In both modes:
`PgUp`/`PgDn` scroll, `Ctrl-B`/`Ctrl-K` toggle the side panes, `Ctrl-C` twice quits.

**The screen.** Threads on the left (`Ctrl-B` hides it; pick one and `Enter` to resume),
the conversation in the middle (replies rendered as Markdown; tool calls and handoffs as
one-liners that `Enter` or `o` expands), each participant on the right (`Ctrl-K`): state, role,
how full its window is (`~NN%` for free models, `?` for CLI agents, which keep their own
context) and tokens this thread. The status line shows the talk switch and the latest
message's model-to-model turns and tokens against their limits. `Tab` completes, or moves
between input, chat and threads. Under 100 columns the side panes hide.

**Who may edit.** Only the `writer` from `models.toml` (default `claude`), whenever a message
reaches it, alone or in a broadcast ([ADR 0017](docs/adr/0017-writer-edits-from-any-message.adr)):
the chat shows `you → claude (may edit)` or `you → claude, codex (claude may edit)`. Start a
message with `@all?` and nobody edits. It runs with your own
setup and the `permission_mode` you give it; unset means `acceptEdits` for Claude and
`workspace-write` for Codex, never what your own CLI config says. Whatever else Claude wants
to do that needs approval (most shell commands) comes to you as a yellow card in the chat
([ADR 0018](docs/adr/0018-approvals-in-the-chat.adr)): `Ctrl-A` gives it the keys, then `y`
allows once, `a` allows Claude's suggested rule (e.g. `Bash(cargo test:*)`) for the rest of
the thread, `n` denies, `Esc` leaves it waiting. The status line counts waiting cards; every
decision is logged. Codex has no such channel: `auto_review = true` lets its own reviewer
approve the writer's escalations (wider than its sandbox, so off by default). Everyone else
is read-only: plan mode for Claude, the read-only sandbox for Codex, both without your own
MCP servers, hooks and plugins, and each in its own mirror of the checkout
(`<data>/worktrees/`): synced before each of its turns (HEAD, uncommitted changes and
untracked files; ignored files like `target/` are not copied, so it builds there itself),
never copied back (needs git 2.35 or later). `/worktrees` lists the mirrors with their sizes, `/worktrees clean [id]`
removes them. Not mirrored yet: a checkout with no commit, a sparse checkout, a populated
submodule, an untracked nested repository; those turns fail and say why rather than review a partial copy.

**After a crash.** Only one writer edits a checkout at a time, across every roundtable on
it, whatever its data folder: the lock and a crash marker live in the checkout's own git
folder (`.git/roundtable/`), so they move with it if it is renamed. If roundtable dies while
the writer works, the CLI may keep editing; on the next start in that checkout edits stay
locked (every turn read-only) and the chat shows the marker. Check nothing is running, then
type `/quiet`. Outside a git checkout nobody edits, and if git can't tell the checkout
(git missing, a broken worktree, `GIT_DIR`/`GIT_WORK_TREE` set) edits stay locked.

**Models asking each other.** Claude gets an `ask` tool: it can put a question to another
participant mid-turn and use the answer. Each exchange shows inline (`claude → codex`) and
is kept in the transcript, blocked ones too. Who may ask whom is the `[talk] allow` matrix
(default: nobody); `max_turns` and `budget_tokens` cap the model-to-model turns and tokens
per message you send; `/talk off` stops it all. An asked model never edits files, `Esc`
cancels the whole chain, and a busy model answers `busy` instead of making anyone wait.
Codex can be asked but doesn't ask yet.

**Waiting writers.** If you change `writer` while the old one is still editing, a message
to the new writer waits until the old one is done instead of editing alongside it.

## Requirements

- macOS or Linux.
- The agent CLIs, installed and logged in. Tested with Claude Code 2.1.331 and 2.1.332, and
  codex-cli 0.171.0 (0.172.0 for the plan-limit reader; its two-turn recording is pending).
  Their JSON output changes between versions, so others may misbehave.
- Start it inside a git repo; outside one, Codex is unavailable unless you pass
  `--outside-repo`. Codex is also unavailable in a project that has its own
  `.codex/config.toml` (here or in a parent folder): such a config can start MCP servers
  or run programs that a read-only turn couldn't stop.
- One roundtable at a time; a second instance exits with "already running".
- For free models: any OpenAI-compatible endpoint, e.g. Ollama (`brew install ollama`, then
  `ollama pull llama3.3`) or LM Studio locally, or OpenRouter / Groq / Gemini with a key.

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
| 6 | [0005](docs/adr/0005-openai-compatible-models.adr) | Free models over the OpenAI-compatible API | done |
| 7 | [0007](docs/adr/0007-model-to-model-communication.adr) | Model-to-model `ask`, talk policy, limits, `/talk on\|off` | done |
| 8 | [0008](docs/adr/0008-slash-commands-and-mentions.adr) | Full command set and completion | done |
| 9 | [0009](docs/adr/0009-tui-layout.adr) | Three-pane layout | done |
| 10 | [0024](docs/adr/0024-log-command.adr) | `/log`: the whole thread as Markdown in your configured editor | done |
| 11 | [0015](docs/adr/0015-vim-keys.adr) | Vim keys: Insert and Normal modes | done |
| 12 | [0014](docs/adr/0014-usage-plan-limits.adr) | `/usage` shows Claude's and Codex's 5-hour and weekly limits; 80%+ in the status line | done |
| 13 | [0017](docs/adr/0017-writer-edits-from-any-message.adr) | The writer edits from any message; `@all?` keeps everyone read-only | done |
| 14 | [0018](docs/adr/0018-approvals-in-the-chat.adr) | Tool approvals in the chat; the writer's unset mode is `acceptEdits` | done |
| 15 | [0016](docs/adr/0016-worktree-per-agent.adr) | A git worktree per read-only agent | done |
| 16 | [0020](docs/adr/0020-handoff.adr) | `/handoff`: pass the thread to another model | done |
| 17 | [0021](docs/adr/0021-compact.adr) | `/compact`: summarize a free model's history | done |
| 18 | [0022](docs/adr/0022-model-add.adr) | `/model add`: add a free model from the chat | done |
| 19 | [0023](docs/adr/0023-acp-adapter.adr) | An ACP adapter for other coding agents | next |
| 20 | [0019](docs/adr/0019-fork.adr) | `/fork`: branch the thread (waits for a recorded Codex `exec fork` fixture) | |

Later, each with its own ADR first.

## License

Not chosen yet.
