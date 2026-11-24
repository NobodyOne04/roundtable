# roundtable

Rust TUI console that runs Claude Code, Codex and OpenAI-compatible models in one chat and shows their conversation with each other.

## Commands
- `cargo build`, `cargo run` (needs a real terminal; `q` quits)
- `cargo nextest run` in CI; `cargo test` locally works the same
- `cargo clippy --all-targets --locked -- -D warnings`, `cargo fmt --check` (CI job `check` runs all three)
- Snapshot tests use insta: after an intended UI change, `INSTA_UPDATE=always cargo test`, then review the `.snap` diff.

## Layout
- `src/main.rs`: entry point, terminal setup/teardown, drawing; tests at the bottom, snapshots in `src/snapshots/`.
- `.github/workflows/check.yml`: CI (fmt, clippy, nextest), required check `check` on `main`.
- `rust-toolchain.toml`: pinned toolchain.
- `docs/adr/`: architecture decisions (`NNNN-title.adr`), index and statuses in `docs/adr/README.md`.
- `README.md`: user-facing overview, config preview, roadmap.
- `AGENTS.md`: Codex context (its role here, state, pending tasks); keep "Pending for Codex" current.

## Conventions
- ADR first: no code for a feature until its ADR is Accepted by the owner. Statuses live in `docs/adr/README.md`.
- Proposals may be reviewed together in one docs PR; implementation is one ADR per PR, in the order noted in `docs/adr/README.md`, commit subjects carry `[ADR-NNNN]`.
- When an ADR changes status, update the table in `docs/adr/README.md` in the same PR.
- Never put API keys in config examples, fixtures or the event log (ADR 0011, 0010).
- CLI JSON formats change between versions: pin tested `claude`/`codex` versions in the README once code exists.
