# local-models — Consistent-branded Ollama models

Rung 2 of the model ladder. Consistent routes work across three lanes — the
**Brain** (hosted frontier models, via the `brain` mode / `hosted_brain` tier),
the **Muscle** (local Ollama models doing the always-on, no-marginal-cost work:
heartbeats, routing, fallbacks — `muscle` mode, `local_*` tiers), and the
**Reflex** (`fast-fallback` mode: tiny, instant, canned-if-needed). The
Modelfiles in this directory are the Muscle, branded: they bake Consistent's
operating understanding and the correct parameters (context window,
temperature) into named local models, so a machine running Consistent pulls one
model name and gets behavior that is already tuned for the job instead of a
stock chat model with defaults.

## The models

| Model | Base | Job | Baked parameters |
| --- | --- | --- | --- |
| `consistent-router` | `gemma4:e2b` (small/fast) | Classify any user message into one canonical task kind (`prompt-interpretation`, `planning`, `operations`, `coding`, `debugging`, `review`, `growth`, `creative` — the `[routing.task_kinds.*]` list in `consistent-world.toml`) | `temperature 0.1`, `num_ctx 4096` |
| `consistent-beat` | `gemma4:latest` (**latest == E4B** — there is no separate `gemma4:e4b` tag) | Heartbeat pulses and general local muscle work | `temperature 0.4`, `num_ctx 16384` |

## Build

```sh
ollama create consistent-beat -f consistent-beat.Modelfile
ollama create consistent-router -f consistent-router.Modelfile
```

Verify with `ollama show consistent-beat` / `ollama show consistent-router` —
the Parameters section should list the values above.

## The SYSTEM-override caveat (read this before editing a SYSTEM)

Ollama applies a Modelfile `SYSTEM` **only when the API request carries no
system message of its own**. That splits the two models:

- **`consistent-beat`**: the gateway's assembled envelope (SOUL → agent
  operating contract → director block → entity → mission, built in
  `crates/gateway/src/prompt_assembly.rs`) always sends a system message, so
  through the gateway the baked SYSTEM is inert — a fallback persona for bare
  callers (`ollama run consistent-beat`, curl without a system message). The
  baked `PARAMETER`s, however, **always** apply; they are the real payload.
- **`consistent-router`**: the baked SYSTEM is the whole point. Callers send
  just the text to classify as a plain user message — no system message — and
  the model answers with one lowercase task-kind word.

## How the gateway reaches them

Point a named model handle at the created model — provider `ollama`, model =
the created name — in `consistent-world.toml`:

```toml
[llm.models.local-beat]
provider = "ollama"
model = "consistent-beat"

[llm.models.local-router]
provider = "ollama"
model = "consistent-router"
```

then reference the handle from a tier (e.g. `[routing.model_tiers.local_fast]`)
or wherever a `(provider, model)` pair is accepted. Nothing else changes: the
resolver already knows the `ollama` provider at `http://127.0.0.1:11434`.

## Publishing (future)

To let users `ollama pull` a Consistent-branded model instead of building from
a Modelfile, copy the local model into a registry namespace and push:

```sh
ollama cp consistent-beat  <namespace>/consistent-beat
ollama cp consistent-router <namespace>/consistent-router
ollama push <namespace>/consistent-beat
ollama push <namespace>/consistent-router
```

where `<namespace>` is a Consistent-owned account on ollama.com (or a
self-hosted registry). After that, install docs collapse to
`ollama pull <namespace>/consistent-beat`.
