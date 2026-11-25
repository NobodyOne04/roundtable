# Changelog

## Unreleased

### Added
- Three-pane layout (ADR 0009): threads (`Ctrl-B`, `Enter` resumes), context per participant (`Ctrl-K`), status line, collapsible tool and handoff lines, Markdown replies, `Tab` focus
- `/resume` (ADR 0008, 0010): reopen a past thread with its chat, CLI sessions and model histories (only for participants still pointing where they did); threads from another checkout open read-only
- Commands (ADR 0008): `/help`, `/clear`, `/exit`, `/status`, `/usage`, `/context`, `/model test`, `/talk show|allow|deny`, `/budget`, `/turns`; `Tab` completion for commands and `@` ids, `Alt-Enter` new lines, `PgUp`/`PgDn` scrolling, `\/` escape; per-thread `usage.json`
- Models ask each other (ADR 0007): Claude's `ask` tool through `roundtable mcp`, talk policy, per-message turn and token limits, `/talk on|off`, inline handoffs, `Esc` cancels the chain
- Free models over the OpenAI-compatible API (ADR 0005): streaming replies, their own history fitted to the window, one retry on 5xx or a too-long request, keys from env or the OS keychain, redacted from the transcript
- Writer lock in the checkout (ADR 0012, accepted): one writer per checkout across every roundtable and data folder, survives renames; nobody edits outside git
- The writer may edit (ADR 0004, 0011): only a message to the writer alone, with its `permission_mode`; after a crash edits stay locked until `/quiet` (ADR 0010)
- Live model registry (ADR 0011): `models.toml` written on first run, edits applied while running (all or nothing), `/models`, `/model enable|disable|use`, identities bind sessions and history to a destination
- Claude Code and Codex in the chat (ADR 0004): `@claude`, `@codex`, `@all`, streaming replies, tool lines, queue when busy, `Esc` cancels (whole process group), sessions resumed, read-only for now, preflight with versions
- Transcript files (ADR 0010): append-only JSONL per thread, torn-line repair, atomic metadata, and only one roundtable running at a time
- Event loop (ADR 0002): one task owns the app state; terminal input and a 30 fps tick send events through one bounded channel; redraw only on change
- Rust skeleton (ADR 0001): pinned toolchain, empty table screen, terminal restored on exit and panic, file logging, CI
- Proposed ADRs 0001-0009 (stack, event loop, event log, adapters, model registry, model-to-model talk, commands, layout) and the README
- AGENTS.md: Codex context with its role, state and pending review
- Proposed ADR 0010 (JSONL files as the transcript) and 0011 (hot in-memory model registry)

### Fixed
- Transcript events carry `turn` (the user message they descend from) and `parent` (the event that caused them), as ADR 0010 says; they were always empty
- Long prompts wrap in the input box (up to 8 rows, then the last ones) and the cursor stays in view; a pasted multi-line block goes into the input whole instead of sending at its first line break
- The `ask` socket falls back to a short owner-only temp folder when the data folder path is too long for a Unix socket (startup failed)
- Writer safety after Codex review: broadcasts never write, explicit safe default modes, waiting writers keep order, the crash lock covers the whole git checkout and fails closed on unreadable history or git errors, `/quiet` unlocks only after the settle is on disk
- Transcript, sessions and logs are readable by the owner only (data folder 0700, files 0600)

### Changed
- Proposed ADRs 0014-0024: `/usage` plan limits, vim keys, worktree per agent, writer edits from any message, approvals in the chat, `/fork`, `/handoff`, `/compact`, `/model add`, ACP adapter, `/log` (0024; the first `/log` draft, 0013, rejected)
- `default = "all"` (or `/model use all`): a message without an @mention goes to everyone, read-only like `@all`; `@<id>` still picks one, and only the writer named alone may edit
- Speaker colours: you are cyan, Codex (the reviewer) is green
- README matches the ADRs: roadmap in build order with progress, worktree decision, hot-registry and disable behaviour, chat-only models, requirements
- ADRs 0010 and 0011 accepted
- ADRs 0001, 0002, 0004, 0005, 0007, 0008, 0009 accepted; 0003 (SQLite) and 0006 (registry) rejected
- ADR 0004: single-writer limit decided, git worktree per agent
