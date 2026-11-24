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
| [0012](0012-checkout-writer-lock.adr) | The writer lock lives in the checkout (amends 0010) | Proposed |

Implementation order follows dependencies, not numbers: 0001, 0002, 0010, 0004, 0011,
0005, 0007, 0008, 0009; 0012 lands with the writer fixes (0010 and 0011 replace earlier numbers that others depend on).
