---
name: venture-weekly-review
description: Run the weekly operating review for one venture or project — what shipped, what slipped, the numbers that matter, open decisions, and next week's three priorities — and write the result back into the venture's context. Use when the user asks for a weekly review, a week-in-review, a retro, "how did this week go" for a project, or a Friday/Monday planning pass.
---

# Venture weekly review

A weekly review is how a multi-venture CEO keeps each company honest without
living inside it. The output is short, comparable week to week, and it leaves a
durable trace in the venture so next week starts from this week's conclusions.

## 1. Pin down the venture

The review is for exactly one venture. If no project is active and the user did
not name one, call `list_entities` and ask which — do not review "everything".

## 2. Gather evidence

- `review_today` for the venture's open tasks, missions, and approvals.
- `recall_recent` for notes, captures, and agent work from the past 7 days.
- Read the venture's goals and active context (they are in your instructions
  under the active entity) — the review measures the week *against the goals*.

## 3. Write the review

```
## <Venture> — week of <date>

**Headline:** one sentence a board member would understand.

**Shipped** — what is done, with evidence (task closed, commit, artifact).
**Slipped** — what was planned and did not happen, and the real reason.
**Numbers** — the 1-3 metrics this venture lives by, with the delta vs last
  week. If no metric is tracked, say so and propose one.
**Open decisions** — each with the options and the default if nobody decides.
**Next week** — exactly three priorities, each with an owner (user, a director,
  or an agent) and what "done" looks like.
```

## 4. Close the loop

Ask before writing anything back. On a yes:

- `update_entity` — refresh the venture's `active_context` with the headline and
  next week's priorities, so every future chat about this venture starts there.
- `create_task` — one task per "next week" priority that does not already exist.

## Rules

- Slips are reported plainly. The review is useless if it is flattering.
- "Done" means evidence exists. Agent work that reported done without a diff or
  artifact is "reported, unverified".
- Keep it under ~250 words before the follow-ups.
