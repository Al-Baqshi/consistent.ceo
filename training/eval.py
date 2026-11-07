"""Score local models on Consistent's real jobs, side by side.

Runs each model through Ollama exactly as the gateway calls it (native
/api/chat, think=false, merged system prompt) on held-out rendered prompts,
grades with cw/checks.py, and prints a comparison.

    python eval.py --beats data/prompts/beats_eval.jsonl --chats data/prompts/chats_eval.jsonl \
        --chat-scenarios data/scenarios/chats.jsonl --models gemma4:latest consistent-gemma:latest --limit 60
"""

import argparse
import collections
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(__file__))
from cw import checks  # noqa: E402
from cw.llm import OllamaEndpoint  # noqa: E402
from distill import merge_system  # noqa: E402


def load(path, limit=None):
    with open(path) as f:
        rows = [json.loads(l) for l in f if l.strip()]
    return rows[:limit] if limit else rows


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--models", nargs="+", required=True)
    p.add_argument("--beats")
    p.add_argument("--chats")
    p.add_argument("--mobile-chats", help="pre-rendered phone turns (carry their own expect)")
    p.add_argument("--chat-scenarios", default="data/scenarios/chats.jsonl",
                   help="scenario file holding each chat's `expect`")
    p.add_argument("--limit", type=int, default=50)
    p.add_argument("--ollama", default="http://localhost:11434")
    p.add_argument("--out", default="data/eval")
    args = p.parse_args()
    os.makedirs(args.out, exist_ok=True)

    beats = load(args.beats, args.limit) if args.beats else []
    chats = load(args.chats, args.limit) if args.chats else []
    expects = {}
    if chats and os.path.exists(args.chat_scenarios):
        expects = {s["id"]: s.get("expect") for s in load(args.chat_scenarios)}
    if args.mobile_chats:
        mobile = load(args.mobile_chats, args.limit)
        chats += mobile
        expects.update({r["id"]: r.get("expect") for r in mobile})

    summary = {}
    for model in args.models:
        ep = OllamaEndpoint(model, args.ollama)
        stats = collections.defaultdict(list)
        fails = collections.Counter()
        records = []
        t0 = time.time()
        def call(*a, **kw):
            try:
                return ep.chat(*a, **kw)
            except Exception as e:  # a slow/failed call counts as a failure, not a crash
                return {"content": None, "tool_calls": [], "error": str(e)}

        for row in beats:
            # Same sampling as production (rendered into the row by the gateway).
            out = call(merge_system(row["messages"]), max_tokens=row.get("max_tokens") or 400,
                       temperature=row.get("temperature"))
            g = checks.check_beat(row, out.get("content"))
            stats["beat:" + (row.get("intent") or "generic")].append(g["pass"])
            stats["beat:all"].append(g["pass"])
            fails.update(f.split(":")[0] for f in g["fails"])
            records.append({"id": row["id"], "answer": out.get("content"), "grade": g})
        for row in chats:
            out = call(merge_system(row["messages"]), tools=row["tools"], max_tokens=1024,
                       temperature=row.get("temperature", 0.65))
            g = checks.check_chat_step(row["tools"], out, expects.get(row["id"]))
            stats["chat:all"].append(g["pass"])
            if row["id"].startswith(("mobile_", "bench_mobile")):
                stats["chat:mobile"].append(g["pass"])
            fails.update(f.split(":")[0] for f in g["fails"])
            records.append({"id": row["id"], "response": out, "grade": g})
            print(".", end="", flush=True)
        elapsed = time.time() - t0
        with open(os.path.join(args.out, model.replace("/", "_").replace(":", "_") + ".jsonl"), "w") as f:
            for r in records:
                f.write(json.dumps(r) + "\n")
        summary[model] = {
            "rates": {k: round(sum(v) / len(v), 3) for k, v in sorted(stats.items())},
            "n": {k: len(v) for k, v in sorted(stats.items())},
            "top_fails": fails.most_common(6),
            "secs": round(elapsed),
        }
        print(f"\n{model}: {json.dumps(summary[model]['rates'])}")

    print("\n=== pass rates ===")
    keys = sorted({k for s in summary.values() for k in s["rates"]})
    print("metric".ljust(18) + "".join(m[:24].ljust(26) for m in args.models))
    for k in keys:
        print(k.ljust(18) + "".join(str(summary[m]["rates"].get(k, "-")).ljust(26) for m in args.models))
    for m in args.models:
        print(f"{m}: top failures {summary[m]['top_fails']}  ({summary[m]['secs']}s)")
    with open(os.path.join(args.out, "summary.json"), "w") as f:
        json.dump(summary, f, indent=2)


if __name__ == "__main__":
    main()
