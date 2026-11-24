# roundtable

Rust TUI console that runs Claude Code, Codex and OpenAI-compatible models in one chat and shows their conversation with each other.

## Commands
- `cargo build`, `cargo run` (needs a real terminal; Ctrl-C twice quits; `--outside-repo` lets Codex run outside git)
- `cargo nextest run` in CI; `cargo test` locally works the same
- `cargo clippy --all-targets --locked -- -D warnings`, `cargo fmt --check` (CI job `check` runs all three)
- Snapshot tests use insta: after an intended UI change, `INSTA_UPDATE=always cargo test`, then review the `.snap` diff.

## Layout
- `src/main.rs`: entry point, terminal setup/teardown, the event loop and its source tasks (terminal input, tick), drawing; snapshots in `src/snapshots/`.
- `src/lib.rs`: the library (all logic); `main.rs` only runs the terminal.
- `src/agent.rs`: Claude Code and Codex adapters (command + JSON parser) and the process runner: process group, cancel, reap (ADR 0004).
- `tests/fixtures/`: recorded CLI output per version (`claude-<ver>-turnN.jsonl`, `codex-<ver>-turnN.jsonl`), stripped of paths and account fields.
- `src/registry.rs`: `models.toml` parsing and validation (all or nothing), default file, identities (FNV-1a), comment-keeping edits (ADR 0011).
- `src/transcript.rs`: transcript files, single-instance lock, atomic writes (ADR 0010).
- `src/app.rs`: `App` state, `Event`, `Effect` and `App::update`, the only place state changes (ADR 0002).
- `.github/workflows/check.yml`: CI (fmt, clippy, nextest), required check `check` on `main`.
- `rust-toolchain.toml`: pinned toolchain.
- `docs/adr/`: architecture decisions (`NNNN-title.adr`), index and statuses in `docs/adr/README.md`.
- `README.md`: user-facing overview, config preview, roadmap.
- `AGENTS.md`: Codex context (its role here, state, pending tasks); keep "Pending for Codex" current.

## Conventions
- Each ADR implementation PR marks its row `done` in the README roadmap and the next one `next`.
- New CLI version: record a fresh two-turn fixture (sanitize: no paths, account or credential fields, no request ids such as `api_message_id`, `msg_…`, `uuid`, thinking `signature`), keep the old one, and update the tested versions in the README.
- Agent turns are read-only until the writer PR lands; never pass a bypass/full-access flag to a CLI, and the registry refuses such `permission_mode`s.
- The registry lives in `App`; the loop does the file I/O (`Runtime::reload_registry`, `edit_registry`). Own writes are skipped by fingerprint, so never reload on every watcher event without the hash check.
- Transcript lines are append-only: never rewrite `events.jsonl`; a status change is a new `settle`/`delivery` line. Small JSON files go through `transcript::write_atomic`.
- State changes only in `App::update` (no I/O there); tasks send `Event`s, the loop runs `Effect`s. Never share `App` behind a lock.
- ADR first: no code for a feature until its ADR is Accepted by the owner. Statuses live in `docs/adr/README.md`.
- Proposals may be reviewed together in one docs PR; implementation is one ADR per PR, in the order noted in `docs/adr/README.md`, commit subjects carry `[ADR-NNNN]`.
- When an ADR changes status, update the table in `docs/adr/README.md` in the same PR.
- Never put API keys in config examples, fixtures or the event log (ADR 0011, 0010).
