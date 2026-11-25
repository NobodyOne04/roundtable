# roundtable

Rust TUI console that runs Claude Code, Codex and OpenAI-compatible models in one chat and shows their conversation with each other.

## Commands
- `cargo build`, `cargo run` (needs a real terminal; Ctrl-C twice quits; `--outside-repo` lets Codex run outside git)
- `cargo nextest run` in CI; `cargo test` locally works the same
- `cargo clippy --all-targets --locked -- -D warnings`, `cargo fmt --check` (CI job `check` runs all three)
- Snapshot tests use insta: after an intended UI change, `INSTA_UPDATE=always cargo test`, then review the `.snap` diff.

## Layout
- `src/ui.rs`: drawing: panes, chat lines, Markdown, status line (ADR 0009). The binary only; logic stays in the library.
- `src/main.rs`: entry point, terminal setup/teardown, the event loop and its source tasks (terminal input, tick, the `ask` socket); screen snapshots in `src/snapshots/`.
- `src/lib.rs`: the library (all logic); `main.rs` only runs the terminal.
- `src/agent.rs`: Claude Code and Codex adapters (command + JSON parser) and the process runner: process group, cancel, reap (ADR 0004).
- `tests/fixtures/`: recorded CLI output per version (`claude-<ver>-turnN.jsonl`, `codex-<ver>-turnN.jsonl`), stripped of paths and account fields.
- `src/registry.rs`: `models.toml` parsing and validation (all or nothing), default file, identities (FNV-1a), comment-keeping edits (ADR 0011).
- `src/commands.rs`: the slash-command table (`/help` and completion come from it) (ADR 0008).
- `src/mcp.rs`: `roundtable mcp`, the stdio MCP server giving Claude the `ask` tool, and its socket client (ADR 0007).
- `src/limits.rs`: plan limits (ADR 0014): Claude's `rate_limit_event`, Codex's session file (`token_count` lines), the `/usage` block and status warnings; `App::limits`, saved to `<data>/limits.json`.
- `src/log.rs`: `/log`'s Markdown render of a thread from its transcript (ADR 0024), as `entries` that `/handoff` (ADR 0020, `App::handoff*`) fits and checks one by one; the loop writes the temp file and hands the terminal to `editor` (`leave_screen`/`enter_screen`).
- `src/openai.rs`: OpenAI-compatible chat: streaming SSE, history fitting, retries, key resolution (ADR 0005).
- `src/worktree.rs`: the read-only agents' mirrors (ADR 0016): create, sync (the writer's index via `write-tree`/`read-tree --reset -u`, then changed and untracked files copied, listed in `<id>.copied`), list, remove; git runs with hooks and fsmonitor off, in its own process group.
- `src/writer_lock.rs`: checkout resolution (git root, git dir) and the writer lock and crash marker (ADR 0012).
- `src/transcript.rs`: transcript files, single-instance lock, atomic writes (ADR 0010).
- `src/app.rs`: `App` state, `Event`, `Effect` and `App::update`, the only place state changes (ADR 0002).
- `.github/workflows/check.yml`: CI (fmt, clippy, nextest), required check `check` on `main`.
- `rust-toolchain.toml`: pinned toolchain.
- `docs/adr/`: architecture decisions (`NNNN-title.adr`), index and statuses in `docs/adr/README.md`.
- `README.md`: user-facing overview, config preview, roadmap.
- `AGENTS.md`: Codex context (its role here, state, pending tasks); keep "Pending for Codex" current.

## Conventions
- A new slash command goes in `commands::COMMANDS` and in `App::command`; `/help` and completion read the table.
- Keys (ADR 0015): the mode is the focus: `Focus::Input` is Insert, `Chat`/`Threads` are Normal (`App::normal_key`). A new Normal key goes there and in the input box hint in `ui.rs`; keys for both modes go before the mode split in `App::key`.
- Live-testing keys in tmux: send `Escape` and the next key in separate `send-keys` calls with a pause, or the terminal reads them as one Alt key.
- `ask` (ADR 0007): the loop gives each Claude turn that may ask a one-time token (`Runtime::ask_tokens`) and only accepts asks carrying it; `App::asked` runs the checks in the ADR's order and every ask is answered exactly once (`Effect::Answer`), refusals included. Asked turns are never writable.
- Resolved API keys reach the loop only as `Event::KeyResolved` (`Secret` hides its value in Debug) and live in `Runtime::secrets`. All participant output is cleaned by `Runtime::redact_output` before the app sees it, and transcript writes by `Runtime::record`; never log or show a key value, only its reference. Provider text is not logged.
- Events the loop raises itself go into its `local` queue, never `try_send` on its own channel (a full channel would drop them).
- Each ADR implementation PR marks its row `done` in the README roadmap and the next one `next`.
- New CLI version: record a fresh two-turn fixture (sanitize: no paths, account or credential fields, no request ids such as `api_message_id`, `msg_…`, `uuid`, thinking `signature`), keep the old one, and update the tested versions in the README.
- Only the `writer` is ever writable, on any message that reaches it unless it starts with `@all?` or edits are locked (`App::submit`, ADR 0017); every other turn gets the read-only flags in `agent::command`. Never pass a bypass/full-access flag to a CLI; the registry refuses such `permission_mode`s.
- Mirrors (ADR 0016): `Effect::StartTurn { mirror: true }` runs in `worktree::sync`'s folder, synced inside the turn's task; a sync failure fails the turn, never falls back to the checkout. Native sessions are keyed `id/identity/cwd` (`session_key` in main.rs). Cleanup only touches mirrors with a record; never `git worktree prune`.
- Approvals (ADR 0018): only the writer's Claude turns get `--permission-prompt-tool stdio` and an open stdin (`Runtime::approvals`); `App::approvals` holds waiting cards, answered only while `App::approving` (`Ctrl-A`). Grants are matched by roundtable, never passed as `--allowedTools`. Shapes come from `tests/fixtures/claude-2.1.332-approval.txt`.
- Writer lock (ADR 0012): `writer_lock::CheckoutLock` in `<git dir>/roundtable/` is the only authority. The loop takes it and writes `writer.json` before recording a writable delivery (`Runtime::begin_writer`) and clears it after the reply (`end_writer`); `/quiet` removes exactly the marker shown. Transcript `writable` lines are history only.
- The registry lives in `App`; the loop does the file I/O (`Runtime::reload_registry`, `edit_registry`). Own writes are skipped by fingerprint, so never reload on every watcher event without the hash check.
- Transcript lines are append-only: never rewrite `events.jsonl`; a status change is a new `settle`/`delivery` line. Small JSON files go through `transcript::write_atomic`.
- State changes only in `App::update` (no I/O there); tasks send `Event`s, the loop runs `Effect`s. Never share `App` behind a lock.
- ADR first: no code for a feature until its ADR is Accepted by the owner. Statuses live in `docs/adr/README.md`.
- Proposals may be reviewed together in one docs PR; implementation is one ADR per PR, in the order noted in `docs/adr/README.md`, commit subjects carry `[ADR-NNNN]`.
- When an ADR changes status, update the table in `docs/adr/README.md` in the same PR.
- Never put API keys in config examples, fixtures or the event log (ADR 0011, 0010).
