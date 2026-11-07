"""Turn rendered prompts into SFT rows using a teacher model.

Inputs are the outputs of `consistent-world training render-beats/render-chats`
(exact production prompts) plus the scenario files (for chat expectations and
mock tool state). For every prompt the teacher answers K times; answers are
graded with cw/checks.py and the best passing one becomes a training row.

    python distill.py --teacher http://localhost:11435 --teacher-model gemma4:31b \
        --beats data/prompts/beats.jsonl --chats data/prompts/chats.jsonl \
        --scenarios data/scenarios --out data/sft
"""

import argparse
import json
import os
import random
import sys
from concurrent.futures import ThreadPoolExecutor

sys.path.insert(0, os.path.dirname(__file__))
from cw import checks, mock_tools  # noqa: E402
from cw.llm import make_endpoint, to_openai_messages  # noqa: E402

MAX_TOOL_ROUNDS = 6  # chat_agent.rs::MAX_TOOL_ITERATIONS

# Extra guidance shown only to the teacher. The student is trained on the
# plain production prompt, so it learns to behave this way unprompted.
BEAT_COACHING = """

(Quality bar for this answer: name the specific people, amounts, and dates from the context; lead with the single most important thing; end on one concrete next action the owner can do today. If a previous beat flagged something still open, say it is still open and escalate. Never claim you did anything — you have no tools here. Obey the word limit and the plain-text rule strictly.)"""

CHAT_COACHING = """

(Teacher note: use a tool only when the user's intent is clear; for relative times always pass in_minutes; after a tool result, confirm in one short human sentence with no IDs or JSON; if a reminder came back without a fire time, ask for a concrete time. For priorities/status questions call review_today first. Be warm, crisp, and proactive: when useful, suggest the next step.)"""


LOOPS_COACHING = """

(Teacher note for the hourly check-in: only items tagged reminded/no answer/overdue, or due within 60 minutes, need the owner this hour. If none do, reply exactly SKIP and nothing else. Otherwise pick at most two, lead with the most stuck, give one concrete next step each, and for items reminded 3+ times or with no answer ask plainly whether to keep it, snooze it, or drop it. At most 60 words.)"""


def merge_system(messages):
    """Join leading system messages — what the Ollama adapter sends Gemma."""
    i = 0
    while i < len(messages) and messages[i]["role"] == "system":
        i += 1
    if i <= 1:
        return list(messages)
    merged = {"role": "system", "content": "\n\n".join(m["content"] for m in messages[:i])}
    return [merged] + list(messages[i:])


def load(path):
    with open(path) as f:
        return [json.loads(l) for l in f if l.strip()]


def distill_beat(row, teacher, k):
    msgs = merge_system(row["messages"])
    coached = [dict(m) for m in msgs]
    coached[-1]["content"] += LOOPS_COACHING if row.get("intent") == "loops" else BEAT_COACHING
    best = None
    for _ in range(k):
        out = teacher.chat(coached, temperature=0.8, max_tokens=row.get("max_tokens") or 400)
        answer = (out.get("content") or "").strip()
        grade = checks.check_beat(row, answer)
        cand = (grade["pass"], grade["score"], -grade.get("words", 0), answer, grade)
        if best is None or cand[:3] > best[:3]:
            best = cand
        if grade["pass"] and grade["score"] == 1.0:
            break
    if not best or not best[0]:
        return None, best[4] if best else None
    return {
        "id": row["id"],
        "kind": "heartbeat",
        "intent": row.get("intent"),
        "messages": msgs + [{"role": "assistant", "content": best[3]}],
    }, best[4]


def run_chat_episode(row, scenario, teacher):
    tools = row["tools"]
    msgs = merge_system(row["messages"])
    convo = [dict(m) for m in msgs]
    teacher_view = [dict(m) for m in msgs]
    teacher_view[0]["content"] += CHAT_COACHING
    first = None
    for _ in range(MAX_TOOL_ROUNDS):
        out = teacher.chat(to_openai_messages(teacher_view), tools=tools, temperature=0.7, max_tokens=1024)
        if first is None:
            first = out
        if not out["tool_calls"]:
            final = {"role": "assistant", "content": (out.get("content") or "").strip()}
            convo.append(final)
            teacher_view.append(final)
            return convo, first
        step = {"role": "assistant", "content": out.get("content") or "", "tool_calls": out["tool_calls"]}
        convo.append(step)
        teacher_view.append(step)
        for c in out["tool_calls"]:
            result = mock_tools.run(c["name"], c["arguments"], scenario)
            tool_msg = {"role": "tool", "tool_call_id": c["id"], "name": c["name"], "content": json.dumps(result)}
            convo.append(tool_msg)
            teacher_view.append(tool_msg)
    return None, first


def distill_chat(row, scenario, teacher, k):
    expect = scenario.get("expect") or None
    last = None
    for _ in range(k):
        convo, first = run_chat_episode(row, scenario, teacher)
        if convo is None:
            continue
        grade = checks.check_chat_step(row["tools"], first, expect)
        final = convo[-1].get("content", "")
        if not final:
            grade["fails"].append("empty_final")
        if any(ch in final for ch in ("{", "task_", "hb_")) and '"' in final:
            grade["fails"].append("recites_ids_or_json")
        for m in convo:
            for c in m.get("tool_calls") or []:
                grade["fails"] += checks.validate_call(c, row["tools"])
        grade["pass"] = not grade["fails"]
        last = grade
        if grade["pass"]:
            return {"id": row["id"], "kind": "chat", "messages": convo, "tools": row["tools"],
                    "meta": scenario.get("meta")}, grade
    return None, last


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--teacher", required=True)
    p.add_argument("--teacher-model", required=True)
    p.add_argument("--beats")
    p.add_argument("--chats")
    p.add_argument("--mobile-chats", help="pre-rendered phone turns from gen_scenarios (mobile_chats.jsonl)")
    p.add_argument("--scenarios", default="data/scenarios")
    p.add_argument("--out", default="data/sft")
    p.add_argument("--k", type=int, default=3, help="teacher samples per prompt")
    p.add_argument("--workers", type=int, default=16)
    p.add_argument("--val-frac", type=float, default=0.05)
    args = p.parse_args()

    teacher = make_endpoint(args.teacher, args.teacher_model)
    os.makedirs(args.out, exist_ok=True)
    rows, rejects = [], []

    jobs = []
    if args.beats:
        for r in load(args.beats):
            jobs.append(("beat", r, None))
    if args.chats:
        scen = {s["id"]: s for s in load(os.path.join(args.scenarios, "chats.jsonl"))}
        for r in load(args.chats):
            jobs.append(("chat", r, scen.get(r["id"], {})))

    if args.mobile_chats:
        for r in load(args.mobile_chats):
            jobs.append(("chat", r, r))

    def work(job):
        kind, r, s = job
        try:
            if kind == "beat":
                return distill_beat(r, teacher, args.k) + (r["id"],)
            return distill_chat(r, s, teacher, args.k) + (r["id"],)
        except Exception as e:
            return None, {"fails": [f"error:{e}"]}, r["id"]

    with ThreadPoolExecutor(args.workers) as pool:
        for i, (row, grade, rid) in enumerate(pool.map(work, jobs)):
            if row:
                rows.append(row)
            else:
                rejects.append({"id": rid, "grade": grade})
            if (i + 1) % 50 == 0:
                print(f"{i + 1}/{len(jobs)} kept={len(rows)}", flush=True)

    random.Random(0).shuffle(rows)
    n_val = max(1, int(len(rows) * args.val_frac)) if rows else 0
    for name, part in (("val", rows[:n_val]), ("train", rows[n_val:])):
        with open(os.path.join(args.out, f"{name}.jsonl"), "w") as f:
            for r in part:
                f.write(json.dumps(r) + "\n")
    with open(os.path.join(args.out, "rejects.jsonl"), "w") as f:
        for r in rejects:
            f.write(json.dumps(r) + "\n")
    print(f"kept {len(rows)} ({n_val} val), rejected {len(rejects)} -> {args.out}")


if __name__ == "__main__":
    main()
