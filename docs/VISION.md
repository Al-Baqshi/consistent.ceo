# Consistent CEO — Vision

> The local-first operating system for missions and entities. — [consistent.ceo](https://consistent.ceo)

## What it is

Consistent CEO is a persistent, evidence-backed operating graph for people who run things: founders, operators, agencies, and eventually teams. Businesses, projects, goals, people, files, approvals, and memories are **first-class objects** — not incidental prompt context — and AI pushes the work forward continuously under explicit human oversight.

It is *not* another AI assistant, another coding agent, or a chat wrapper. Chat is one surface. The product is the world model underneath it and the governance around it.

## The non-negotiable promise: evidence-backed execution

Every consequential claim the system makes must resolve to a cite — the entity registry, stored memory, or a cached external source. Every consequential action must pass a policy gate (auto-allow, require approval, or deny). Claims and outputs carry visible evidence states:

| State | Meaning | May trigger side-effects? |
|---|---|---|
| **Verified** | Resolved against workspace truth (entities, missions, memory) | Yes, under policy |
| **Observed** | Backed by a tool result or ingested source in this turn | Yes, under policy |
| **Inferred** | Model reasoning, explicitly labeled | No — plans/drafts/approval requests only |
| **Unverified** | Unresolved claim | No — never |
| **Not applicable** | The sentence asserts nothing about the world (a greeting, an offer, a question) | n/a |

`Not applicable` is an addition to the original four, made when the states
became a type (`crates/architect/src/cite.rs`). Without it a turn that makes no
factual claim has zero resolved claims, which is indistinguishable from a turn
whose every claim failed — so Verify would branch to ASK on "Happy to help".

**Status:** these states exist in code and are computed in shadow mode; the
Verify stage writes its verdict to `audit_trail`. They are not yet on the wire
or in the UI, so a user does not see them. `STATUS.md` is the authority on how
far this has actually got.

The system's personality follows from this: an **adversarial validator, not a cheerleader**. It challenges assumptions, asks instead of guessing, and refuses to fabricate. "No hallucination" is a product requirement, enforced by the Architect pipeline's Verify stage, not a marketing line.

## How it works (the Architect)

The spec is `docs/diagrams/consistent-world-future-state.v2.mmd`. Every turn — whether triggered by a human in chat or asynchronously by the scheduler, heartbeat, or proactive engine — traverses one pipeline:

**Inspect** (read entity, resources, history, lessons, recalled sessions) → **Look forward** (goals + gaps → next plausible states) → **Select director + model tier** → **Generate** (draft from inspected facts; may request gated tools or delegate to coding agents) → **Verify** (every claim must resolve, or the system asks instead of answering).

Around the pipeline sits the harness: cost/credit precheck, policy gate, and an append-only audit trail recording every step.

## Every user is an entity

Each user gets an auto-provisioned, private **Personal Space** — their own entity. Loose thoughts, captures, and canvases default there and smart-route to business entities when they belong elsewhere. Onboarding is conversational: the Entity Conductor turns a natural chat into a born entity.

## Product surfaces

- **Desktop / web** — execution and administration: missions, tasks, entities, review, console.
- **Mobile** — approvals, nudges, capture, the brain-map canvas. The phone is also the authenticator (device-approval login, passkeys).
- **Voice** *(parked vision, kept)* — full-duplex human-like companion (Gemini Live today, PersonaPlex-class brain-box later), with the gateway staying the brain. A2F face and screen perception remain parked until the core loop is trustworthy.

## The moat

Model access is commoditizing (MCP, ACP, A2A, multi-provider routers). The moat is what compounds:

1. **Structured world model** — the entity/mission/approval/evidence graph.
2. **Mission governance** — approval rails, policy gates, budgets, audit.
3. **Evidence states** — trust you can see, on every claim.
4. **Accumulated operational history** — session summaries, director lessons, memories that make the system smarter about *your* world every week.

Switching cost grows with the graph. The strategic end state: Consistent is the **system of record for operations** — the place a user's whole operating world lives.

## Business model (summary)

Software-subscription-first; managed AI wallet with hard caps and transparent overage as a supporting line, never token arbitrage. Tri-mode LLM access: managed (default onboarding), BYOK, and local runtimes. Personas in order: founder-operator → SMB ops team → agency/holding operator → enterprise. North-star activation metric: **time to first completed mission**.
