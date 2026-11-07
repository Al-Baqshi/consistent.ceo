# consistent-gemma — Consistent's own local model

A Gemma 4 E2B (Apache-2.0) fine-tune that does Consistent's two jobs well,
offline, on a laptop. E2B is the default because it runs about twice as fast
as E4B on processor-only machines (like Intel Macs) and trains on a free
Colab T4. Pass `--base unsloth/gemma-4-E4B-it` to train the bigger one with
the same data.

The jobs, all graded by `cw/checks.py`:

1. **Heartbeats**: the proactive nudge. One system + one user message, no
   tools, plain prose of at most 120–150 words, grounded in the entity's real
   state (`director_runner.rs`).
2. **Hourly check-in** (`loops` beats): sees the owner's open items (nudged,
   parked, overdue, due soon), nudges about the one or two that matter in at
   most 60 words, or answers exactly `SKIP` when nothing needs them this hour.
3. **Chat**: the chief-of-staff turn with 18 workspace tools (`set_reminder`,
   `create_task`, `review_today`, …) and up to 6 tool rounds (`chat_agent.rs`).
   This includes turning a jotted to-do with no time into something that comes
   back (`remind_when`), and handling "done" or "push it to tomorrow" replies
   to a nudge.
4. **Phone, offline**: the two-tool agent the mobile app runs on-device
   (`mobile/src/lib/localAgent.ts`, mirrored in `cw/mobile_agent.py`; keep the
   two in sync).

## Why this design

- **Train/serve parity.** Prompts are rendered by the gateway's own
  builders (`consistent-world training render-*`), and tool schemas come from
  the live registry. The data cannot drift from what the model sees in
  production. The Ollama adapter merges stacked system blocks into one,
  matching Gemma's template. The training script renders with Google's
  template, which Ollama's `gemma4` renderer reproduces.
- **Distillation from an open teacher.** Gemma 4 31B (also Apache-2.0)
  writes the answers, so there are no terms-of-service questions about
  training on the outputs. The teacher gets extra quality guidance that the
  student never sees, so the student learns to behave that way unprompted.
- **Graded, not trusted.** Every teacher answer passes `cw/checks.py`: word
  caps, no markdown/greeting/sign-off, exactly one question on setup beats,
  no false "I scheduled…" claims, grounded in the entity's names and numbers,
  valid tool calls, and expected tool and arguments (e.g. "in 2 hours" →
  `in_minutes: 120`). Failures are dropped.
- **Masked correctly.** Gemma 4 puts tool results inside the model turn. The
  trainer masks them, so the model learns to *call* tools, not to invent
  their output.
- **Real usage feeds back.** With `CONSISTENT_WORLD_TRAINING_TRACES=1`, the
  gateway records every exchange. `traces_to_sft.py` turns the good ones
  (from cloud models, by default) into more training data.

## Layout

| Path | What |
|---|---|
| `scenarios/benchmark_*.jsonl` | Fixed hand-written benchmark. Never trained on. |
| `gen_scenarios.py` | Teacher invents diverse entities across a life-area × situation × intent matrix |
| `distill.py` | Teacher answers the rendered prompts K times; the best passing answer becomes a row |
| `traces_to_sft.py` | Recorded gateway traces → rows |
| `cw/checks.py` | The graders (shared by distill + eval) |
| `eval.py` | Side-by-side pass rates of Ollama models, called exactly like the gateway |
| `train_unsloth.py` | LoRA/QLoRA fine-tune + GGUF export (GPU box) |
| `Modelfile` | Packages the GGUF for Ollama with the `gemma4` renderer/parser |
| `pipeline.sh` | The steps below as commands |

## Fastest path: one command on a GPU machine

```sh
bash training/gpu_box.sh
```

From the repo root on any Linux + NVIDIA machine, this does everything
below: installs Rust, Ollama and Unsloth; builds the gateway CLI; starts the
teacher; generates, renders, distills and grades the data; fine-tunes E2B;
exports the GGUF; and benchmarks stock against tuned.

- **Free:** a Kaggle notebook with *GPU T4×2* and *Internet* switched on.
  Upload the repo (or `git clone` it), then run `!bash training/gpu_box.sh`
  in a cell. The default teacher, `gemma4:26b` (mixture-of-experts, 4B
  active), fits across the two T4s. Expect roughly 6–8 hours.
- **Paid:** 1×A100 or H100 on RunPod, Lambda or Vast: about 2–3 hours,
  about $5–10. A stronger teacher fits too:
  `TEACHER_MODEL=gemma4:31b bash training/gpu_box.sh`.
- **Smoke test first:**
  `BEATS=40 CHATS=30 MOBILE=20 REPLIES=20 bash training/gpu_box.sh`.

## Steps (manual)

**0. Baseline** (Mac, any time):

```sh
./pipeline.sh eval   # with only gemma4:latest installed, drop the second model from the script
```

**1. GPU box.** Rent one H100 80GB (or A100 80GB) for roughly 3–5 hours. Expect about $10–15 on RunPod / Lambda / Vast.

```sh
pip install -r requirements-gpu.txt
curl -fsSL https://ollama.com/install.sh | sh
OLLAMA_NUM_PARALLEL=16 OLLAMA_CONTEXT_LENGTH=16384 ollama serve &
ollama pull gemma4:31b
```

The teacher is served by Ollama because its `gemma4` tool-call parser is the
same one the student runs under. Any OpenAI-compatible server works if you
prefer (`TEACHER=… TEACHER_MODEL=…`).

**2. Generate data** (Mac, teacher over an SSH tunnel):

```sh
ssh -N -L 11435:localhost:11434 gpu-box &
TEACHER=http://localhost:11435 ./pipeline.sh data
```

This writes `data/sft/{train,val,rejects}.jsonl`. Read a sample of
`rejects.jsonl`: a high reject rate on one check usually means the check
or the prompt needs a tweak, not the model.

**3. Upload:** `./pipeline.sh upload gpu-box`

**4. Train** (GPU box; stop vLLM first to free memory):

```sh
cd consistent-training
python train_unsloth.py --data data/sft/train.jsonl data/sft/traces.jsonl --eval data/sft/val.jsonl \
    --out out/consistent-gemma --no-4bit --gguf q4_k_m   # add --base unsloth/gemma-4-E4B-it for the bigger model
```

Check the printed "trained-on text" before the run starts. It must show only
assistant replies and tool calls. Gemma 4 E4B's loss starts around 13–15;
that is normal for this model family.

**5. Install** (Mac): copy `out/consistent-gemma/gguf/*.gguf` back into
`training/out/consistent-gemma/gguf/`, set the exact filename in
`Modelfile`, then `./pipeline.sh install`.

**6. Compare:** `./pipeline.sh eval`. Ship only if `consistent-gemma` beats
`gemma4:latest` on every row.

**7. Point Consistent at it:** pick `ollama / consistent-gemma` on the AI page,
or edit `~/.consistent-world/ai.json`.

**8. Publish to both download buttons.** The desktop button pulls an Ollama
model and the phone button downloads a GGUF file. Both read
`GET /api/local-model`.

- Desktop: run
  `ollama cp consistent-gemma <namespace>/consistent-gemma && ollama push <namespace>/consistent-gemma`.
  Then set `CONSISTENT_LOCAL_MODEL=<namespace>/consistent-gemma` for the
  gateway, or change `DEFAULT_LOCAL_CHAT_MODEL` in
  `crates/gateway/src/control_api/ollama_api.rs`.
- Phone: upload `out/consistent-gemma/gguf/*Q4_K_M.gguf` to a Hugging Face
  repo with `--push <you>/consistent-gemma` in step 4. Then set
  `CONSISTENT_MOBILE_MODEL_URL` and `CONSISTENT_MOBILE_MODEL_FILE`, or change
  `MOBILE_MODEL_*` in `crates/gateway/src/control_api/reminders_api.rs`.
  Phones that can't reach the gateway fall back to `FALLBACK_LOCAL_MODEL` in
  `mobile/src/api/endpoints.ts`, so update that too for the next app
  release.

Until then, both buttons install stock Gemma 4 E2B: `gemma4:e2b` on desktop,
and `unsloth/gemma-4-E2B-it-GGUF` Q4_K_M (3.1 GB) on the phone.

## Recording real traces

```sh
CONSISTENT_WORLD_TRAINING_TRACES=1 cargo run -p consistent-world-gateway
consistent-world training traces   # shows the directory and on/off
```

Traces contain your real conversations and stay on this machine. Review
them before uploading anywhere.

### Learning from what worked

With tracing on, the gateway also logs what you did with each reminder
(done, snooze, drop, delegate) to `traces/outcomes/`. Run:

```sh
python3 traces_to_sft.py --learn-from-outcomes
```

This keeps check-ins written by the *local* model when they named an item
and you closed, dropped or delegated it within 3 hours. Normally the local
model's own outputs are skipped so it doesn't learn its own mistakes. With
this flag, the ones that measurably helped become training data, so each
fine-tuning round learns from what moves you to act.
