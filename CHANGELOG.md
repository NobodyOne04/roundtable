# Changelog

## Unreleased

### Added
- Live model registry (ADR 0011): `models.toml` written on first run, edits applied while running (all or nothing), `/models`, `/model enable|disable|use`, identities bind sessions and history to a destination
- Claude Code and Codex in the chat (ADR 0004): `@claude`, `@codex`, `@all`, streaming replies, tool lines, queue when busy, `Esc` cancels (whole process group), sessions resumed, read-only for now, preflight with versions
- Transcript files (ADR 0010): append-only JSONL per thread, torn-line repair, atomic metadata, and only one roundtable running at a time
- Event loop (ADR 0002): one task owns the app state; terminal input and a 30 fps tick send events through one bounded channel; redraw only on change
- Rust skeleton (ADR 0001): pinned toolchain, empty table screen, terminal restored on exit and panic, file logging, CI
- Proposed ADRs 0001-0009 (stack, event loop, event log, adapters, model registry, model-to-model talk, commands, layout) and the README
- AGENTS.md: Codex context with its role, state and pending review
- Proposed ADR 0010 (JSONL files as the transcript) and 0011 (hot in-memory model registry)

### Fixed
- Transcript, sessions and logs are readable by the owner only (data folder 0700, files 0600)

### Changed
- README matches the ADRs: roadmap in build order with progress, worktree decision, hot-registry and disable behaviour, chat-only models, requirements
- ADRs 0010 and 0011 accepted
- ADRs 0001, 0002, 0004, 0005, 0007, 0008, 0009 accepted; 0003 (SQLite) and 0006 (registry) rejected
- ADR 0004: single-writer limit decided, git worktree per agent
