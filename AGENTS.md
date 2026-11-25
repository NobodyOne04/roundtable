# roundtable: Codex context

Read `CLAUDE.md` first: commands, layout and conventions live there. This file replaces
Codex's `CLAUDE.md` fallback, so it only adds what Codex needs. Global rules:
`~/.codex/AGENTS.md`.

## Your role here
- Reviewer and critic. Code has started (ADR 0001 skeleton); Claude implements the
  accepted ADRs in the order noted in `docs/adr/README.md`, one PR each.
- Review the ADRs as a design: contradictions between ADRs, wrong CLI flags (check
  `claude --help`, `codex exec --help`), guarantees the design cannot keep. Never edit
  `*.adr` files; findings go back to Claude.

## State (as of 2026-11-24)
- Repo `NobodyOne04/roundtable`, private, so no "protect main" ruleset.
- The owner accepted 0001, 0002, 0004, 0005, 0007, 0008, 0009 and rejected 0003 (SQLite:
  no database wanted) and 0006 (registry: must be hot, in memory). Their replacements,
  0010 (JSONL files) and 0011 (hot registry), are accepted too (2026-11-24), without a
  Codex critique (usage limit).
- Your earlier findings and what Claude did with each are in PR #1's body under
  `## Codex review`.

## Pending for Codex
- Review the ADR 0007 PR (`feature/adr-0007-ask`), if it was skipped there.
- Review the ADR 0008 PR (`feature/adr-0008-commands`), skipped (usage limit).
- Review the layout PR (`feature/adr-0009-layout`), skipped (usage limit).
- Review the `/resume` PR (`feature/adr-0008-resume`), skipped (usage limit): especially
  the identity checks when rebuilding sessions and model histories.
- Verify how Codex approves an MCP tool in `codex exec` (`mcp_servers.<name>.default_tools_approval_mode`
  values), so Codex can get the `ask` tool too; today it can be asked but not ask.
- Review the ADR 0005 PR (`feature/adr-0005-free-models`): its last commits (loop-wide key
  redaction in `Runtime::redact_output`, `StreamRedactor`, SSE event assembly, unknown
  usage) landed after your two reviews; the third was skipped (usage limit).
- Record a real two-turn Codex fixture (first turn plus `exec resume`):
  `tests/fixtures/codex-0.171.0-turn1.jsonl` was cut from one turn, and resume was never
  recorded. Sanitize as CLAUDE.md says, and hand it to Claude to commit.

Done 2026-11-25 (later): your review of everything merged in PRs #21-#26 (`09c50da..eac3c13`)
and your critique of ADRs 0016-0023; the fixes are in `bug/codex-review-fixes`.

Done 2026-11-25: a full-codebase review (every file in `src/`, against the ADRs) replaced the
per-PR reviews that were skipped during the usage limit; its fixes are in PR #15. The
deferred finding: events carry no `turn`/`parent` yet (ADR 0010), to be built with ADR 0007.

## Notes
- The writer lock moved into the checkout (ADR 0012, `src/writer_lock.rs`); the transcript
  scan is gone. Known, accepted limit: a rename *during* a writable turn (needs `openat`).
- ADR 0004: the owner chose a git worktree per agent for the single-writer limit
  (2026-11-24); its own ADR comes later. Don't re-report project hooks or detached tools
  as findings; review the worktree ADR when it exists.
- Earlier passes converged on edge cases in writer recovery; the fail-closed confirm
  dialog (now in ADR 0010) is deliberate, not an omission.
