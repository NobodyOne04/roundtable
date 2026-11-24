# Architecture decisions

One decision per file, `NNNN-short-title.adr` (Markdown content). Numbers are never reused.
Lower numbers are implemented first.

| ADR | Title | Status |
|---|---|---|
| [0001](0001-rust-tokio-ratatui.adr) | Rust, tokio and ratatui as the stack | Proposed |
| [0002](0002-single-owner-event-loop.adr) | One task owns the state; everything else sends events | Proposed |
| [0003](0003-sqlite-event-log.adr) | Append-only SQLite event log is the transcript | Proposed |
| [0004](0004-cli-agent-adapters.adr) | CLI agents through their JSON streaming modes, ACP later | Proposed |
| [0005](0005-openai-compatible-models.adr) | Free and side models through one OpenAI-compatible adapter | Proposed |
| [0006](0006-model-registry-config.adr) | Model registry in TOML, enable/disable, keys in the OS keychain | Proposed |
| [0007](0007-model-to-model-communication.adr) | Model-to-model talk: an `ask` tool, a policy matrix and hard limits | Proposed |
| [0008](0008-slash-commands-and-mentions.adr) | Slash commands and @mentions | Proposed |
| [0009](0009-tui-layout.adr) | Three-pane layout: threads, conversation, context | Proposed |
