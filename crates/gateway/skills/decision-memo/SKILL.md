---
name: decision-memo
description: Turn a messy choice into a one-page decision memo — the question, real options, trade-offs, a recommendation, and whether the decision is reversible. Use when the user is weighing options, asks "should we…", is stuck between alternatives, wants to think something through, or needs to explain a decision to a co-founder, team, or investor.
---

# Decision memo

Good decisions are cheap to make when the question is sharp and the options are
real. The memo's job is to make the decision *easy*, not to look thorough.

## Steps

1. **Sharpen the question.** Restate it as a single question with a deadline,
   e.g. "Do we hire a second engineer for Acme before the March launch?" If you
   cannot, ask the user one clarifying question first.
2. **Name the reversibility.** One-way door (expensive or impossible to undo) or
   two-way door (cheap to reverse)? Two-way doors deserve speed, not analysis —
   say so and keep the memo to five lines.
3. **List 2-4 real options.** "Do nothing" is always an option; include it when
   it is genuinely viable. No straw men.
4. **For each option:** what you gain, what it costs (money, time, focus), what
   has to be true for it to work, and the main risk.
5. **Recommend.** Pick one and give the single strongest reason. Then state what
   would change your mind.

## Format

```
### Decision: <question> (by <date>)
Door: one-way | two-way

| Option | Gain | Cost | Must be true | Main risk |
|---|---|---|---|---|

**Recommendation:** <option> — <one-line reason>.
**Would change my mind:** <signal>.
**Next step:** <first concrete action>.
```

## Close the loop

Offer to record it. On a yes, save the memo with `create_note` under the active
venture so the reasoning is findable later, and create the next-step task with
`create_task`.

## Rules

- Use facts from the venture's context and docs; mark assumptions as such.
- Don't hedge the recommendation. A memo that recommends nothing has failed.
