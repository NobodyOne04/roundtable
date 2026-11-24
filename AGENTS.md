# roundtable: Codex context

Read `CLAUDE.md` first: commands, layout and conventions live there. This file replaces
Codex's `CLAUDE.md` fallback, so it only adds what Codex needs. Global rules:
`~/.codex/AGENTS.md`.

## Your role here
- Reviewer and critic. The project is in its design phase: ADRs 0001-0009 in `docs/adr/`
  are all *Proposed*, no code exists yet.
- Review the ADRs as a design: contradictions between ADRs, wrong CLI flags (check
  `claude --help`, `codex exec --help`), guarantees the design cannot keep. Never edit
  `*.adr` files; findings go back to Claude.

## State (as of 2026-11-24)
- Repo `NobodyOne04/roundtable`, private, so no "protect main" ruleset.
- PR #1 merged as `eeec51a`: the nine ADRs and the README are on `main`, still *Proposed*
  (merging is not accepting; the owner accepts each ADR explicitly). You critiqued the
  drafts and reviewed the branch six times; your findings and what Claude did with each
  are in PR #1's body under `## Codex review`.

## Pending for Codex
- Review 7 of PR #1 was skipped (usage limit) and the PR is merged since. Review the
  merged commit: `codex review --commit eeec51a -c sandbox_mode="read-only"`.
  Report findings to Claude; Claude fixes them in a follow-up `docs/` PR.

## Notes
- Open owner decision, ADR 0004: the single-writer guarantee is best effort (project hooks,
  detached tools). Don't re-report it as a finding; comment on the three options instead.
- Earlier passes converged on edge cases in writer recovery; the fail-closed confirm
  dialog in ADR 0003 is deliberate, not an omission.
