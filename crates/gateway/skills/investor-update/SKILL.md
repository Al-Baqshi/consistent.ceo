---
name: investor-update
description: Draft a monthly investor or stakeholder update for one venture — highlights, metrics, lowlights, asks — in the concise format founders use with angels and VCs. Use when the user asks for an investor update, board update, monthly update, shareholder letter, or "update for my investors".
---

# Investor update

Investors read dozens of these. The ones they remember are short, honest about
problems, and end with a specific ask. Write for a busy reader on a phone.

## Gather

1. Confirm the venture and the period (default: last calendar month).
2. `recall_recent` and `review_today` for what shipped, closed missions, and
   notes from the period. Use the venture's goals and docs for context.
3. Ask the user for any numbers you do not have in stored state (revenue, users,
   burn, runway). **Never invent a metric.** Leave `[TBD]` and list what is
   missing at the end.

## Structure

```
Subject: <Venture> — <Month> update

TL;DR: <one sentence: the single most important thing this month>

Highlights
- 3 bullets max. Outcomes, not activity.

Metrics
- <metric>: <value> (<change vs last month>)
- Runway: <months> — only if the user provided it.

Lowlights
- 1-2 bullets. What went wrong and what you are doing about it.
  Investors trust founders who report problems early.

Asks
- 1-3 specific asks: an intro to <type of person>, feedback on <thing>,
  candidates for <role>. Specific asks get answered; vague ones don't.

Thanks to <people who helped>.
```

## Rules

- Under 300 words.
- Present tense, active voice, no hype adjectives.
- This is an outward-facing message: draft only. Never send it yourself. Offer
  to save the draft with `create_note`.
