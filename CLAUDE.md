# roundtable

Rust TUI console that runs Claude Code, Codex and OpenAI-compatible models in one chat and shows their conversation with each other.

## Commands
None yet: no code. Planned (ADR 0001): `cargo build`, `cargo nextest run`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`.

## Layout
- `docs/adr/`: architecture decisions (`NNNN-title.adr`), index and statuses in `docs/adr/README.md`.
- `README.md`: user-facing overview, config preview, roadmap.

## Conventions
- ADR first: no code for a feature until its ADR is Accepted by the owner. All of 0001-0009 are Proposed.
- Proposals may be reviewed together in one docs PR; implementation is one ADR per PR, lowest number first, commit subjects carry `[ADR-NNNN]`.
- When an ADR changes status, update the table in `docs/adr/README.md` in the same PR.
- Never put API keys in config examples, fixtures or the event log (ADR 0006).
- CLI JSON formats change between versions: pin tested `claude`/`codex` versions in the README once code exists.
