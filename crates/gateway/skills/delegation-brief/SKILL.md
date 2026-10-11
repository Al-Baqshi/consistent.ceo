---
name: delegation-brief
description: Write a brief that lets someone else — a teammate, contractor, or an AI agent — do a piece of work without coming back with questions, including context, constraints, acceptance criteria, and what not to do. Use when the user wants to hand something off, assign work, brief a contractor or freelancer, write a spec or ticket, or set up an agent task.
---

# Delegation brief

Most delegated work fails at the brief, not the execution. The person (or
agent) doing the work has none of the user's context. The brief closes that gap
once, up front.

## The seven parts

1. **Outcome** — one sentence: what exists when this is done.
2. **Why it matters** — one or two sentences. People make better judgment calls
   when they know the purpose.
3. **Context** — only what they need: the venture, the audience, links to the
   relevant docs or files, prior attempts and why they failed.
4. **Constraints** — budget, deadline, tools or stack to use, brand/tone rules,
   things that must not change.
5. **Acceptance criteria** — a checklist the user can verify in five minutes.
   Each item is observable ("the page loads on mobile", not "looks good").
6. **Out of scope** — what not to touch. This prevents the most expensive
   surprises.
7. **Check-ins** — when to show progress and what to do when blocked.

## For an AI agent

When the work will run as an agent task in Consistent CEO, also:

- Name the directory or repository it should work in.
- State the definition of done as checks the agent can run itself (tests,
  builds, a file that should exist).
- Mark anything outward-facing (sending, publishing, paying, deploying to
  production) as requiring human approval.

Then offer to create it with `create_task` (or `create_mission` if it spans
several steps — use the mission-planner skill for that).

## Rules

- Under a page. If it's longer, the work should be split.
- Write it so it still makes sense in two weeks.
