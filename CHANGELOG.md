# Changelog

## Unreleased

### Added
- Event loop (ADR 0002): one task owns the app state; terminal input and a 30 fps tick send events through one bounded channel; redraw only on change
- Rust skeleton (ADR 0001): pinned toolchain, empty table screen, terminal restored on exit and panic, file logging, CI
- Proposed ADRs 0001-0009 (stack, event loop, event log, adapters, model registry, model-to-model talk, commands, layout) and the README
- AGENTS.md: Codex context with its role, state and pending review
- Proposed ADR 0010 (JSONL files as the transcript) and 0011 (hot in-memory model registry)

### Changed
- ADRs 0010 and 0011 accepted
- ADRs 0001, 0002, 0004, 0005, 0007, 0008, 0009 accepted; 0003 (SQLite) and 0006 (registry) rejected
- ADR 0004: single-writer limit decided, git worktree per agent
