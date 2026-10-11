---
name: skill-author
description: Turn a workflow the user just walked through, or a process they describe, into a reusable skill saved for one venture or for all of them. Use when the user says "remember how we do this", "save this as a skill", "make this a playbook", "always do X this way", or wants to standardize a process for their agents.
---

# Skill author

A skill is a short, reusable procedure the agent loads when a matching request
comes in. Skills are how the user's way of working compounds: written once, used
on every future request, by chat and by agents.

## When to write one

Only for work that **repeats**. A one-off answer belongs in a note, not a skill.
If the user's request is a one-off, say so and offer `create_note` instead.

## Anatomy

- **name** — lowercase words joined by hyphens, max 64 characters:
  `vendor-invoice-review`, not `Invoice Stuff`.
- **description** — the most important line. It decides whether the skill is
  ever used. Say *what it does* and *when to use it*, with the phrases a user
  would actually type: "Use when the user asks for…, says…, or…".
  Max ~1000 characters.
- **instructions** — Markdown. Write for a capable colleague who lacks context:
  1. A short "why" paragraph — the purpose shapes judgment calls.
  2. Numbered steps, each one an action. Name the tools to call
     (`review_today`, `recall_recent`, `create_task`, `create_note`,
     `update_entity`, `create_mission`, `set_reminder`).
  3. An output format, in a code block, when the result has a shape.
  4. A short **Rules** list: what never to do, what to always check.
  Keep it under ~400 lines; shorter is better.

## Scope

- **project** — the procedure belongs to one venture (its pricing rules, its
  release checklist). Saved with the active project.
- **global** — it applies across every venture (how the user likes decision
  memos written).

## Steps

1. Draft the name, description, and instructions from what the user described
   or the workflow you just ran together.
2. Show the draft and ask for corrections. The user's own words beat yours.
3. On approval, call `save_skill` with the agreed scope.
4. Confirm in one line where it lives and how it will be triggered.

## Rules

- Never save a skill without showing it first.
- Never put secrets, passwords, or API keys in a skill.
- Don't duplicate an existing skill — if one is close, propose improving it.
