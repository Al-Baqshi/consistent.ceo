#!/usr/bin/env bash
# The whole consistent-gemma run on one Linux + NVIDIA machine, end to end:
# teacher (Ollama) -> scenarios -> production prompts (gateway CLI) ->
# distilled + graded data -> LoRA fine-tune -> GGUF -> eval vs. stock.
#
# Works on RunPod / Lambda / Vast (1x A100/H100, ~2-3h) and on a FREE Kaggle
# notebook with 2x T4 (enable GPU T4x2 + Internet; ~6-8h, fits the weekly
# quota). From the repo root:
#
#   bash training/gpu_box.sh                  # full run, sensible defaults
#   TEACHER_MODEL=gemma4:31b bash training/gpu_box.sh   # stronger, slower teacher
#   BEATS=300 CHATS=150 bash training/gpu_box.sh        # quick smoke run
#
# Output: training/out/consistent-gemma/gguf/*.gguf (+ eval summary). Copy
# the GGUF back and follow README "Install" / "Publish".
set -euo pipefail
cd "$(dirname "$0")"
ROOT=$(cd .. && pwd)

TEACHER_MODEL=${TEACHER_MODEL:-gemma4:26b}   # MoE, 4B active: fast teacher that fits 2x T4
STUDENT_BASE=${STUDENT_BASE:-unsloth/gemma-4-E2B-it}
BEATS=${BEATS:-1500}
CHATS=${CHATS:-800}
MOBILE=${MOBILE:-400}
REPLIES=${REPLIES:-300}
WORKERS=${WORKERS:-8}
PORT=11434

log() { printf '\n\033[1m== %s\033[0m\n' "$*"; }

log "1/7 toolchain"
# Native build deps of the gateway (same list as the Dockerfile builder).
if command -v apt-get >/dev/null && ! dpkg -s libasound2-dev >/dev/null 2>&1; then
  SUDO=$([ "$(id -u)" = 0 ] && echo "" || echo sudo)
  $SUDO apt-get update -qq
  $SUDO apt-get install -y -qq --no-install-recommends \
    pkg-config libasound2-dev libssl-dev ca-certificates curl zstd
fi
if ! command -v cargo >/dev/null; then
  curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal
  # shellcheck disable=SC1091
  source "$HOME/.cargo/env"
fi
command -v ollama >/dev/null || curl -fsSL https://ollama.com/install.sh | sh
python3 -m pip install -q -r requirements-gpu.txt

log "2/7 gateway CLI (renders prompts exactly as production does)"
(cd "$ROOT" && cargo build --release -p consistent-world-gateway)
BIN="$ROOT/target/release/consistent-world"

log "3/7 teacher: $TEACHER_MODEL"
if ! curl -sf "localhost:$PORT/api/tags" >/dev/null; then
  OLLAMA_NUM_PARALLEL=$WORKERS OLLAMA_CONTEXT_LENGTH=16384 nohup ollama serve >ollama.log 2>&1 &
  for _ in $(seq 60); do curl -sf "localhost:$PORT/api/tags" >/dev/null && break; sleep 1; done
fi
ollama pull "$TEACHER_MODEL"
ollama pull gemma4:e2b   # the stock student, for the before/after eval
TEACHER="http://localhost:$PORT"

log "4/7 scenarios"
python3 gen_scenarios.py --teacher "$TEACHER" --teacher-model "$TEACHER_MODEL" \
  --beats "$BEATS" --chats "$CHATS" --mobile "$MOBILE" --replies "$REPLIES" \
  --workers "$WORKERS" --out data/scenarios

log "5/7 production prompts + distillation"
"$BIN" training render-beats --input data/scenarios/beats.jsonl --out data/prompts/beats.jsonl
"$BIN" training render-chats --input data/scenarios/chats.jsonl --out data/prompts/chats.jsonl
"$BIN" training render-beats --input scenarios/benchmark_beats.jsonl --out data/prompts/bench_beats.jsonl
"$BIN" training render-chats --input scenarios/benchmark_chats.jsonl --out data/prompts/bench_chats.jsonl
python3 distill.py --teacher "$TEACHER" --teacher-model "$TEACHER_MODEL" \
  --beats data/prompts/beats.jsonl --chats data/prompts/chats.jsonl \
  --mobile-chats data/scenarios/mobile_chats.jsonl \
  --scenarios data/scenarios --out data/sft --workers "$WORKERS"
EXTRA=()
if ls ~/.consistent-world/training/traces/*.jsonl >/dev/null 2>&1; then
  python3 traces_to_sft.py --learn-from-outcomes --out data/sft/traces.jsonl
  EXTRA=(data/sft/traces.jsonl)
fi

log "6/7 fine-tune $STUDENT_BASE"
# Free the GPU for training: the teacher's job is done.
ollama stop "$TEACHER_MODEL" || true
pkill -f "ollama serve" || true
python3 train_unsloth.py --base "$STUDENT_BASE" --data data/sft/train.jsonl "${EXTRA[@]}" \
  --eval data/sft/val.jsonl --out out/consistent-gemma --gguf q4_k_m

log "7/7 eval: stock vs tuned (same prompts, same sampling as production)"
OLLAMA_CONTEXT_LENGTH=16384 nohup ollama serve >ollama.log 2>&1 &
for _ in $(seq 60); do curl -sf "localhost:$PORT/api/tags" >/dev/null && break; sleep 1; done
GGUF=$(ls out/consistent-gemma/gguf/*.gguf | grep -iv mmproj | head -1)
sed "s#^FROM .*#FROM $PWD/$GGUF#" Modelfile > out/Modelfile.local
ollama create consistent-gemma -f out/Modelfile.local
python3 eval.py --models gemma4:e2b consistent-gemma:latest \
  --beats data/prompts/bench_beats.jsonl --chats data/prompts/bench_chats.jsonl \
  --chat-scenarios scenarios/benchmark_chats.jsonl \
  --mobile-chats scenarios/benchmark_mobile.jsonl --out data/eval/final

log "done: $GGUF"
echo "Ship it only if consistent-gemma beats gemma4:e2b on every row above."
