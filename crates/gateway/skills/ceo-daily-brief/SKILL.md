---
name: ceo-daily-brief
description: Produce a one-screen morning brief across every venture the user runs — what needs a decision today, what is at risk, and what moved since yesterday. Use when the user says good morning, asks what to focus on, what is important today, "brief me", or opens the day.
---

# CEO daily brief

The user runs several ventures at once. Their scarcest resource is attention, so
the brief exists to answer one question: **where should my attention go today?**
It is not a status report and not a summary of everything that happened.

## Gather (do not skip)

1. Call `review_today` first. Everything in the brief must come from stored
   state, not memory or guesses.
2. If a venture's picture is thin, call `recall_recent` for that venture before
   deciding it is quiet. Quiet and unknown are different things — say which.

## Write the brief

Use exactly these sections, in this order. Leave a section out entirely when it
is empty; never write "None".

**Decide today** — items only the user can unblock: pending approvals, tasks
blocked on them, questions an agent is waiting on. One line each, with the
venture name first and the concrete ask.

**At risk** — overdue tasks, missions with no progress in 3+ days, deadlines
inside 48 hours. Say *why* it is at risk in a few words.

**Moved** — meaningful progress since yesterday (completed tasks, closed
missions, evidence-backed agent work). Group by venture. Skip trivia.

**Suggested focus** — at most three items, ranked. Each one is a verb phrase the
user could start on right now.

## Rules

- One screen. If it does not fit, cut from "Moved" first.
- Venture names lead every line so the user can scan across companies.
- Never claim progress that has no evidence behind it; say "reported done,
  unverified" when that is the truth.
- End with a single offer, e.g. "Want me to turn the focus list into tasks?" —
  and only create tasks (`create_task`) if they say yes.
