"""Convert recorded gateway traces into SFT rows.

Start the gateway with CONSISTENT_WORLD_TRAINING_TRACES=1 and use it normally
(ideally with a strong cloud model as the brain). Every exchange lands in
~/.consistent-world/training/traces/*.jsonl; this script keeps the good ones.

    python traces_to_sft.py --traces ~/.consistent-world/training/traces --out data/sft_traces.jsonl
"""

import argparse
import glob
import hashlib
import json
import os
import re
import sys
from datetime import datetime, timedelta

sys.path.insert(0, os.path.dirname(__file__))
from cw import checks  # noqa: E402
from distill import merge_system  # noqa: E402

CANNED_PREFIX = "Heartbeat — "
# Within this long of a check-in, closing an item it mentioned counts as "it worked".
OUTCOME_WINDOW_H = 3
RESOLVING = {"done", "complete", "cancel", "drop", "delegate"}


def load_outcomes(traces_dir):
    """Owner actions on reminders, recorded by the gateway (training::record_outcome)."""
    out = []
    for path in glob.glob(os.path.join(traces_dir, "outcomes", "*.jsonl")):
        with open(path) as f:
            for line in f:
                if line.strip():
                    o = json.loads(line)
                    if o.get("action") in RESOLVING:
                        out.append((datetime.fromisoformat(o["ts"].replace("Z", "+00:00")), (o.get("what") or "").lower()))
    return out


def check_in_worked(trace_ts, user_prompt, answer, outcomes):
    """True when the check-in named an item and the owner closed it soon after."""
    answer = answer.lower()
    named = [w for w in checks.open_items(user_prompt) if checks.mentions(answer, w)]
    until = trace_ts + timedelta(hours=OUTCOME_WINDOW_H)
    return any(trace_ts <= ts <= until and what and any(what in n or n in what for n in named)
               for ts, what in outcomes)


def to_rows_messages(messages):
    names = {}
    out = []
    for m in messages:
        row = {"role": m["role"], "content": m.get("content") or ""}
        if m.get("tool_calls"):
            row["tool_calls"] = m["tool_calls"]
            for c in m["tool_calls"]:
                names[c["id"]] = c["name"]
        if m["role"] == "tool":
            row["tool_call_id"] = m.get("tool_call_id")
            row["name"] = names.get(m.get("tool_call_id"), "")
        out.append(row)
    return merge_system(out)


def conversation_key(messages):
    """Identity of a chat turn: everything up to and including the last user message."""
    last_user = max((i for i, m in enumerate(messages) if m["role"] == "user"), default=0)
    blob = json.dumps(messages[: last_user + 1], sort_keys=True)
    return hashlib.sha1(blob.encode()).hexdigest()


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--traces", default=os.path.expanduser("~/.consistent-world/training/traces"))
    p.add_argument("--out", default="data/sft_traces.jsonl")
    p.add_argument("--include-local", action="store_true",
                   help="also keep answers produced by the local Ollama model (off: avoid learning own mistakes)")
    p.add_argument("--learn-from-outcomes", action="store_true",
                   help="keep local check-ins that led the owner to close an item within 3h (self-improvement)")
    args = p.parse_args()
    outcomes = load_outcomes(os.path.expanduser(args.traces)) if args.learn_from_outcomes else []
    learned = 0

    beats, chats = [], {}
    seen = skipped = 0
    for path in sorted(glob.glob(os.path.join(os.path.expanduser(args.traces), "*.jsonl"))):
        with open(path) as f:
            for line in f:
                if not line.strip():
                    continue
                t = json.loads(line)
                seen += 1
                resp = t.get("response") or {}
                msgs = to_rows_messages(t["messages"])
                local = t.get("provider") == "ollama"
                worked = (
                    local
                    and outcomes
                    and t["kind"] == "heartbeat"
                    and "This is the hourly check-in" in msgs[-1]["content"]
                    and check_in_worked(
                        datetime.fromisoformat(t["ts"].replace("Z", "+00:00")),
                        msgs[-1]["content"],
                        resp.get("content") or "",
                        outcomes,
                    )
                )
                if local and not args.include_local and not worked:
                    skipped += 1
                    continue
                learned += bool(worked)
                if t["kind"] == "heartbeat":
                    answer = (resp.get("content") or "").strip()
                    if not answer or answer.startswith(CANNED_PREFIX):
                        continue
                    user = msgs[-1]["content"]
                    intent = "generic"
                    if answer.strip(" .").upper() == "SKIP":
                        continue  # silence is right sometimes, but traces can't tell us when
                    for tag, needle in (("loops", "This is the hourly check-in"), ("setup", "Ask exactly one question"), ("reminder", "needs their attention"),
                                        ("review", "review the period"), ("drive", "mission you are driving"),
                                        ("ideas", "two or three options")):
                        if needle in user:
                            intent = tag
                    row = {"id": f"trace_{seen}", "intent": intent, "messages": msgs}
                    if checks.check_beat(row, answer)["pass"]:
                        row["kind"] = "heartbeat"
                        row["messages"] = msgs + [{"role": "assistant", "content": answer}]
                        beats.append(row)
                elif t["kind"] == "chat":
                    step = {"role": "assistant", "content": resp.get("content") or ""}
                    if resp.get("tool_calls"):
                        step["tool_calls"] = resp["tool_calls"]
                    full = msgs + [step]
                    if resp.get("tool_calls"):
                        continue  # keep only finished turns; their prefixes hold the tool steps
                    problems = []
                    for m in full:
                        for c in m.get("tool_calls") or []:
                            problems += checks.validate_call(c, t.get("tools") or [])
                    if problems or not step["content"].strip():
                        continue
                    key = conversation_key(msgs)
                    if key not in chats or len(full) > len(chats[key]["messages"]):
                        chats[key] = {"id": f"trace_{seen}", "kind": "chat", "messages": full, "tools": t.get("tools") or []}

    os.makedirs(os.path.dirname(args.out) or ".", exist_ok=True)
    with open(args.out, "w") as f:
        for r in beats + list(chats.values()):
            f.write(json.dumps(r) + "\n")
    print(f"traces={seen} skipped_local={skipped} learned_from_outcomes={learned} "
          f"kept beats={len(beats)} chats={len(chats)} -> {args.out}")


if __name__ == "__main__":
    main()
