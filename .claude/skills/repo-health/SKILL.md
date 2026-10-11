---
name: repo-health
description: Check this repo for the drift that hides real problems — unlanded branches, uncommitted work piling up, red CI on main, placeholder modules the docs describe as delivered, stale planning docs, and dead routes. Use when asked for a status report, a health check, "where are we", "what's half built", "anything drifting", or before landing a branch, cutting a release, or writing a status update. Also use at the start of a session that will touch this repo broadly.
---

# Repo health

On 9 September 2026 this repo was audited and found in a state no single
commit caused: `main` had not moved in 51 days, 88 commits and ~4,900
uncommitted lines sat on one branch, `docs/ROADMAP.md` described a plan
that had been abandoned six weeks earlier without anyone writing that
down, CI on `main` had been red since July, and `docs/VISION.md` called a
pipeline "the non-negotiable promise" while four of its five stages were
six-line placeholder functions.

None of that was a bug. Every individual commit was fine. The failure was
that nothing measured the gap between what the repo *said* and what it
*was*, so the gap grew for two months unnoticed.

This skill is that measurement.

## Run it

```bash
scripts/repo-health.sh          # fast, no network, ~2s
scripts/repo-health.sh --full   # + cargo check, clippy -D warnings, frontend build
```

Exit codes: `0` clean · `1` warnings · `2` breach. The `--full` gates are the
same ones CI runs, so a green `--full` means a green pipeline.

## What it checks, and why each one is here

| # | Check | The failure it would have caught |
|---|---|---|
| 1 | Unlanded work | `main` frozen 51 days; a 88-commit branch nobody could review |
| 2 | Uncommitted volume | 4,921 lines and two whole modules living only in one working tree |
| 3 | CI on main | red since July, invisible because nobody looked |
| 4 | Stub drift | four placeholder modules shipping while docs called them delivered |
| 5 | Doc drift | a roadmap 46 days behind the code, describing an abandoned plan |
| 6 | Dead surfaces | `/data` saying "Coming soon", `/api/compat/*` with no caller |

## Reading the output

**BREACH** means stop and deal with it before continuing the current task.
The two that matter most:

- *main has not moved in N days* — land what is landable today. Work that
  never lands exists on exactly one disk.
- *doc is N days behind the code* — this is the dangerous one, because it
  is silent. A plan nobody follows is worse than no plan: it hides that a
  decision was made. Fix it by editing the doc to describe what actually
  happened, not by deleting the check.

**WARN** means look at it this week. Placeholder modules and dead routes
are not urgent, but each one is a promise the product does not keep, and
they only ever accumulate.

## After running it

Update `STATUS.md` — the living record of where this project actually is.
It exists so the next person (or the next session) does not have to
re-derive the state from 99k lines of Rust. Keep it honest about what is
stubbed; that honesty is the whole point.

If a check fires because the *threshold* is wrong rather than the repo,
change the threshold at the top of the script and say why in the commit.
Do not delete a check to make the output green.

## When not to use this

It is a repo-level smoke alarm, not a code reviewer. For defects in a
specific change, use `/code-review`. For a design question about one
subsystem, read the module's header comment — they are unusually good in
this codebase and are the intended entry point.
