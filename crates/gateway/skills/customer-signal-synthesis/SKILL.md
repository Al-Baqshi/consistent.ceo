---
name: customer-signal-synthesis
description: Synthesize customer feedback, support messages, reviews, sales-call notes, or user interviews into ranked themes with evidence and recommended actions for a venture. Use when the user shares feedback, asks what customers are saying, wants to find patterns in complaints or requests, or is prioritizing a roadmap.
---

# Customer signal synthesis

Raw feedback is loud and anecdotal. A synthesis turns it into a short list of
themes ranked by how much they matter, each backed by evidence, so the user can
decide what to build or fix next.

## Steps

1. **Collect.** Use what the user pasted, plus `recall_recent` for captures and
   notes under the venture that mention customers, users, support, or feedback.
2. **Code each signal.** For every piece of feedback note: who (segment, if
   known), what they want or what hurt, and how strongly (mild / blocking /
   churn risk).
3. **Cluster into themes.** Merge signals that share a root cause, not just
   similar words. "Can't find export" and "need CSV" may be one theme.
4. **Rank.** By (number of signals) × (severity) × (fit with the venture's
   goals). Show the reasoning in a few words.

## Output

```
### <Venture> — what customers are telling us

1. **<Theme>** — <n> signals, <severity>. <One-line root cause.>
   > "<one representative quote>"
   Recommended: <smallest action that tests or fixes it>.
2. …

Noise / one-offs: <brief list — kept so nothing is silently dropped>.
Gaps: <segments we have not heard from>.
```

## Close the loop

Offer to file the top themes. On a yes, `create_note` with the synthesis under
the venture and `create_task` for each recommended action the user accepts.

## Rules

- Quote customers exactly; never paraphrase a quote.
- Distinguish what customers *ask for* from the *problem* behind it.
- Five strong themes beat fifteen weak ones.
