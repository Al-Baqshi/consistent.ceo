---
name: mission-planner
description: Turn a goal into a well-formed mission — a sharp outcome, a definition of done that can be verified, milestones, risks, and the first tasks — then create it. Use when the user states a goal for a venture, says "I want to…", "let's get X done", "plan this", or asks agents to take on a multi-step piece of work.
---

# Mission planner

A mission is the unit of work Consistent CEO's agents push forward unattended.
A vague mission produces vague work and a "done" nobody can check. The planner's
job is to make the mission *verifiable* before any agent touches it.

## 1. Sharpen the outcome

Restate the goal as an outcome a stranger could check:

- Bad: "Improve onboarding."
- Good: "New users reach their first completed mission in under 10 minutes,
  measured on 5 fresh installs."

If the venture is unclear, ask. If the outcome is unclear, propose one and ask
the user to confirm or correct it — one question, not a questionnaire.

## 2. Definition of done

3-5 checks, each observable: a file exists, a test passes, a page returns 200, a
number crosses a threshold, a person approved something. Avoid "is good",
"works well", "is polished".

## 3. Milestones and first tasks

- 2-4 milestones in order; each one is shippable on its own.
- The first milestone's tasks, small enough for one agent run or one sitting.
- Mark any task that touches the outside world (emails, payments, publishing,
  production deploys) as needing human approval.

## 4. Risks

The 1-3 things most likely to sink it, each with an early warning sign.

## 5. Create it

Show the plan in under 200 words, then ask "Create this mission?" On a yes, call
`create_mission` with the outcome as the goal and the definition of done in the
description, then `create_task` for the first milestone's tasks.

## Rules

- One mission per goal. If the goal is really three goals, say so and plan the
  first.
- Never mark outward-facing work as auto-run.
