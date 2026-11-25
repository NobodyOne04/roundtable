# Architecture decisions

One decision per file, `NNNN-short-title.adr` (Markdown content). Numbers are never reused.
Implementation order: see the note under the table.

| ADR | Title | Status |
|---|---|---|
| [0001](0001-rust-tokio-ratatui.adr) | Rust, tokio and ratatui as the stack | Accepted |
| [0002](0002-single-owner-event-loop.adr) | One task owns the state; everything else sends events | Accepted |
| [0003](0003-sqlite-event-log.adr) | Append-only SQLite event log is the transcript | Rejected |
| [0004](0004-cli-agent-adapters.adr) | CLI agents through their JSON streaming modes, ACP later | Accepted |
| [0005](0005-openai-compatible-models.adr) | Free and side models through one OpenAI-compatible adapter | Accepted |
| [0006](0006-model-registry-config.adr) | Model registry in TOML, enable/disable, keys in the OS keychain | Rejected |
| [0007](0007-model-to-model-communication.adr) | Model-to-model talk: an `ask` tool, a policy matrix and hard limits | Accepted |
| [0008](0008-slash-commands-and-mentions.adr) | Slash commands and @mentions | Accepted |
| [0009](0009-tui-layout.adr) | Three-pane layout: threads, conversation, context | Accepted |
| [0010](0010-filesystem-event-log.adr) | Append-only JSONL files are the transcript (replaces 0003) | Accepted |
| [0011](0011-hot-model-registry.adr) | Model registry lives in memory, changes hot, persists to TOML (replaces 0006) | Accepted |
| [0012](0012-checkout-writer-lock.adr) | The writer lock lives in the checkout (amends 0010) | Accepted |
| [0013](0013-log-command.adr) | `/log`: read the thread in your editor | Rejected |
| [0014](0014-usage-plan-limits.adr) | `/usage` shows each CLI's plan limits | Accepted |
| [0015](0015-vim-keys.adr) | Vim keys for moving around (amends 0008) | Accepted |
| [0016](0016-worktree-per-agent.adr) | A git worktree per read-only agent | Accepted |
| [0017](0017-writer-edits-from-any-message.adr) | The writer may edit from any message (amends 0004) | Accepted |
| [0018](0018-approvals-in-the-chat.adr) | Tool approvals in the chat | Accepted |
| [0019](0019-fork.adr) | `/fork`: branch the current thread | Accepted |
| [0020](0020-handoff.adr) | `/handoff`: pass the thread to another model | Accepted |
| [0021](0021-compact.adr) | `/compact`: summarize a free model's history | Accepted |
| [0022](0022-model-add.adr) | `/model add`: add a free model from the chat | Accepted |
| [0023](0023-acp-adapter.adr) | An ACP adapter for other coding agents | Accepted |
| [0024](0024-log-command.adr) | `/log`: the whole thread as Markdown in the configured editor (replaces 0013) | Accepted |

Implementation order follows dependencies, not numbers: 0001, 0002, 0010, 0004, 0011,
0005, 0007, 0008, 0009; 0012 lands with the writer fixes (0010 and 0011 replace earlier numbers that others depend on).

0014, 0015, 0016, 0017 and 0024 are built (0018 in review). 0016 deviates in one point: a
checkout with no commit yet is refused instead of getting an `--orphan` mirror, which would
add a branch to the owner's repo. 0016-0023 were accepted after Codex's critique, which
changed each of them; build order: 0017, 0018 (together they let the writer work from
roundtable: edits from any message, approvals in the chat), 0016, 0019 (after 0018: forks
start without grants), 0020, 0021, 0022, then 0023 (needs 0016's worktrees).
