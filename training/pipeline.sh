#!/usr/bin/env bash
# End-to-end data pipeline, driven from the Mac.
#
# The teacher model runs on a rented GPU (see README "GPU box"); reach it with
#   ssh -N -L 11435:localhost:11434 gpu-box
# then:
#   TEACHER=http://localhost:11435 ./pipeline.sh data
#   ./pipeline.sh upload gpu-box      # then train there (README step 4)
#   ./pipeline.sh install             # after copying the GGUF back
#   ./pipeline.sh eval
set -euo pipefail
cd "$(dirname "$0")"

BIN=${BIN:-../target/release/consistent-world}
TEACHER=${TEACHER:-http://localhost:11435}
TEACHER_MODEL=${TEACHER_MODEL:-gemma4:31b}
BEATS=${BEATS:-1500}
CHATS=${CHATS:-800}

step=${1:-help}
case "$step" in
  data)
    [ -x "$BIN" ] || (cd .. && cargo build -p consistent-world-gateway --release)
    python3 gen_scenarios.py --teacher "$TEACHER" --teacher-model "$TEACHER_MODEL" \
      --beats "$BEATS" --chats "$CHATS" --out data/scenarios
    "$BIN" training render-beats --input data/scenarios/beats.jsonl --out data/prompts/beats.jsonl
    "$BIN" training render-chats --input data/scenarios/chats.jsonl --out data/prompts/chats.jsonl
    "$BIN" training render-beats --input scenarios/benchmark_beats.jsonl --out data/prompts/bench_beats.jsonl
    "$BIN" training render-chats --input scenarios/benchmark_chats.jsonl --out data/prompts/bench_chats.jsonl
    python3 distill.py --teacher "$TEACHER" --teacher-model "$TEACHER_MODEL" \
      --beats data/prompts/beats.jsonl --chats data/prompts/chats.jsonl \
      --mobile-chats data/scenarios/mobile_chats.jsonl \
      --scenarios data/scenarios --out data/sft
    if ls ~/.consistent-world/training/traces/*.jsonl >/dev/null 2>&1; then
      python3 traces_to_sft.py --out data/sft/traces.jsonl
    fi
    ;;
  upload)
    host=${2:?usage: pipeline.sh upload <ssh-host>}
    ssh "$host" mkdir -p consistent-training/data
    scp train_unsloth.py requirements-gpu.txt "$host":consistent-training/
    scp -r data/sft "$host":consistent-training/data/
    ;;
  install)
    ollama create consistent-gemma -f Modelfile
    ollama show consistent-gemma | head -20
    ;;
  eval)
    python3 eval.py --models ${BASE_MODEL:-gemma4:e2b} consistent-gemma:latest \
      --beats data/prompts/bench_beats.jsonl --chats data/prompts/bench_chats.jsonl \
      --chat-scenarios scenarios/benchmark_chats.jsonl \
      --mobile-chats scenarios/benchmark_mobile.jsonl --out data/eval/compare
    ;;
  *)
    sed -n '2,12p' "$0"
    ;;
esac
