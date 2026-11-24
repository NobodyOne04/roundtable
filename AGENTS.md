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
  0010 (JSONL files) and 0011 (hot registry), are *Proposed*.
- Your earlier findings and what Claude did with each are in PR #1's body under
  `## Codex review`.

## Pending for Codex
- Review 7 of PR #1 was skipped (usage limit) and the PR is merged since. Review the
  merged commit: `codex review --commit eeec51a -c sandbox_mode="read-only"`.
  Report findings to Claude; Claude fixes them in a follow-up `docs/` PR.
- Review the ADR 0004 decision change (worktree per agent), skipped for the same reason:
  `codex review --commit 9491219 -c sandbox_mode="read-only"`.
- Critique the drafts of ADR 0010 and 0011 (`adr-critique`), skipped for the same reason.
  Check they keep every guarantee the rejected 0003 and 0006 gave, which the accepted ADRs
  rely on.
- Review the ADR 0001 PR (`feature/adr-0001-scaffold`), skipped for the same reason:
  `codex review --base origin/main -c sandbox_mode="read-only"` on that branch, or
  `--commit <its squash sha>` once merged.

## Notes
- ADR 0004: the owner chose a git worktree per agent for the single-writer limit
  (2026-11-24); its own ADR comes later. Don't re-report project hooks or detached tools
  as findings; review the worktree ADR when it exists.
- Earlier passes converged on edge cases in writer recovery; the fail-closed confirm
  dialog (now in ADR 0010) is deliberate, not an omission.
