"""Generate diverse heartbeat + chat scenarios for Consistent.

The teacher model invents realistic entity details; the matrix below forces
coverage (every life area x every situation x every beat intent) so the
dataset doesn't collapse onto one kind of user.

    python gen_scenarios.py --teacher http://localhost:11435 --teacher-model gemma4:31b \
        --beats 1500 --chats 800 --out data/scenarios
"""

import argparse
import json
import os
import random
import sys
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timedelta, timezone

sys.path.insert(0, os.path.dirname(__file__))
from cw.llm import make_endpoint  # noqa: E402

# (life area, director group) — groups match entity_heartbeat_defaults.rs routing.
AREAS = [
    ("a neighborhood bakery opening a second location", "operations"),
    ("a two-person B2B SaaS startup pre product-market fit", "full-stack-development"),
    ("a mobile app side project nearing App Store launch", "full-stack-development"),
    ("a freelance designer juggling four client retainers", "sales-marketing"),
    ("a YouTube channel about home cooking with 12k subscribers", "content-creative"),
    ("a newsletter writer trying to reach 5,000 subscribers", "content-creative"),
    ("a commercial lease negotiation for a small business", "law"),
    ("an employment contract dispute with a former employer", "law"),
    ("a personal fitness goal: first half marathon in spring", "operations"),
    ("a family home renovation (kitchen + bathroom)", "operations"),
    ("a job search for a senior product manager role", "strategy-partner"),
    ("a seed fundraising round for a climate-tech startup", "strategy-partner"),
    ("a wedding being planned for next autumn", "operations"),
    ("a university student preparing for final exams and a thesis", "operations"),
    ("a small nonprofit running a winter coat drive", "operations"),
    ("an e-commerce store selling handmade ceramics", "sales-marketing"),
    ("a real-estate agent managing buyer leads", "sales-marketing"),
    ("an open-source developer tool with a growing community", "full-stack-development"),
    ("personal finances: paying off a car loan and building savings", "operations"),
    ("a consulting practice moving from hourly to fixed-price offers", "strategy-partner"),
    ("a podcast relaunch after a six-month break", "content-creative"),
    ("caring for an aging parent: appointments, insurance, logistics", "operations"),
    ("a restaurant dealing with staff turnover", "operations"),
    ("a mobile game studio of three people shipping an update", "full-stack-development"),
]

SITUATIONS = [
    "healthy momentum, one clear next step",
    "blocked waiting on a specific other person who has gone quiet",
    "two items overdue, one of them owed to someone else",
    "nothing moved since the last beat",
    "a deadline lands within 48 hours",
    "something the previous beat flagged has just been resolved",
    "conflicting priorities between two goals",
    "recent setback (lost client, failed test, rejection)",
    "very early: the entity barely has context yet",
]

INTENT_WEIGHTS = [("reminder", 3), ("loops", 4), ("review", 2), ("ideas", 2), ("setup", 1), ("drive", 1), (None, 1)]

BEAT_PROMPT = """You are generating realistic test data for a proactive AI chief-of-staff app.
Invent ONE entity the user tracks, about: {area}.
Current situation: {situation}.
{sparse}
Return ONLY a JSON object with these keys:
  "entity": {{"name": str, "description": str, "goals": str, "profile": str (owner's voice/preferences, 1 line), "active_context": str (2-4 concrete facts with names, dates, numbers), "operating_rules": str or null}},
  "mission": null or {{"title": str, "goal": str, "narrative": str (3-5 sentences of what happened so far), "notes": [2-4 short pinned facts]}},
  "lessons": [0-3 short lessons the assistant learned about this owner, e.g. "Owner ignores nudges after 8pm"],
  "memory": [0-5 short remembered facts from past conversations],
  "recent": [{n_recent} previous short heartbeat messages about this entity, newest first, each <= 60 words, plain prose],
  "focus": one-sentence standing brief for this beat,
  "open_items": [3-6 short concrete to-dos the owner has open, each naming a person/thing, e.g. "send ABT the signed photo release"]
Use specific people's names, amounts, and dates (today is {today}). No markdown inside strings."""

SPARSE = "The entity is NEW: keep description short, goals empty or vague, mission null, lessons/memory empty."


def loops_items(todos, rng, skip):
    """Open-items lines in the exact shape reminders.rs::open_loop_lines writes."""
    if skip:
        # Nothing needs the owner this hour: only things due later.
        return [f"[due in {rng.randint(61, 89)} min] {t}" for t in todos[: rng.choice([1, 2])]]
    lines = []
    for t in todos[: rng.randint(1, 5)]:
        kind = rng.choice(["nudged", "nudged", "parked", "overdue", "soon"])
        if kind == "nudged":
            n = rng.randint(1, 3)
            lines.append(f"[reminded {n}x, first {rng.choice(['1h', '3h', '20h', '2d'])} ago] {t}")
        elif kind == "parked":
            lines.append(f"[reminded 4x, no answer, first {rng.randint(2, 6)}d ago] {t}")
        elif kind == "overdue":
            lines.append(f"[overdue {rng.choice(['3h', '1d', '4d'])}] {t}")
        else:
            lines.append(f"[due in {rng.randint(5, 55)} min] {t}")
    order = {"r": 1, "o": 2, "d": 3}
    lines.sort(key=lambda l: (0 if "no answer" in l else order.get(l[1], 4)))
    return lines


def weighted_intent(rng):
    pool = [i for i, w in INTENT_WEIGHTS for _ in range(w)]
    return rng.choice(pool)


def parse_json(text):
    text = (text or "").strip()
    if text.startswith("```"):
        text = text.strip("`")
        text = text[text.find("{") :]
    start, end = text.find("{"), text.rfind("}")
    return json.loads(text[start : end + 1])


def make_beat(i, teacher, rng):
    area, group = rng.choice(AREAS)
    situation = rng.choice(SITUATIONS)
    intent = weighted_intent(rng)
    sparse = intent == "setup" or situation.startswith("very early")
    if sparse:
        intent = "setup"
    n_recent = 0 if sparse else rng.choice([0, 1, 2, 3])
    fired = datetime(2026, rng.randint(1, 12), rng.randint(1, 28), rng.choice([7, 8, 9, 12, 15, 17]), 0, tzinfo=timezone.utc)
    prompt = BEAT_PROMPT.format(
        area=area, situation=situation, sparse=SPARSE if sparse else "", n_recent=n_recent, today=fired.date()
    )
    for _ in range(3):
        try:
            data = parse_json(teacher.chat([{"role": "user", "content": prompt}], temperature=1.0, max_tokens=1500)["content"])
            break
        except (ValueError, TypeError):
            data = None
    if not data or not data.get("entity", {}).get("name"):
        return None
    if data.get("mission") and not data["mission"].get("title"):
        data["mission"] = None
    meta = {"area": area, "situation": situation}
    open_items = []
    if intent == "loops":
        group = "operations"
        skip = rng.random() < 0.2
        meta["expect_skip"] = skip
        todos = [t for t in (data.get("open_items") or []) if isinstance(t, str) and t.strip()]
        if not todos:
            return None
        open_items = loops_items(todos, rng, skip)
        data["entity"]["name"] = "Personal"
        data["mission"] = None
        data["focus"] = ("Keep the owner's promises from slipping: the one or two open items that "
                         "matter this hour, each with the next concrete step.")
    return {
        "id": f"beat_{i:05d}",
        "director": group,
        "open_items": open_items,
        "entity": data["entity"],
        "mission": data.get("mission"),
        "lessons": data.get("lessons") or [],
        "memory": data.get("memory") or [],
        "recent": (data.get("recent") or [])[:3],
        "intent": intent,
        "focus": data.get("focus"),
        "fired_at": fired.isoformat().replace("+00:00", "Z"),
        "meta": meta,
    }


# ---- chat ------------------------------------------------------------------

RELATIVE = [("in 20 minutes", 20), ("in half an hour", 30), ("in 45 minutes", 45), ("in an hour", 60),
            ("in 2 hours", 120), ("in 3 hours", 180), ("in 90 minutes", 90), ("in 10 minutes", 10)]
ABSOLUTE = ["tomorrow at 9am", "tonight", "on Friday at 2pm", "next Monday morning", "at 6pm", "end of day"]
TODOS = ["call the landlord about the deposit", "send the invoice to Maria", "book the dentist", "review the pitch deck",
         "follow up with Jonas on the contract", "renew the domain", "pay the electricity bill", "stretch and drink water",
         "prep questions for the investor call", "check the oven supplier quote", "email the professor about the extension"]

CHAT_KINDS = [
    ("reminder_relative", 4), ("reminder_absolute", 3), ("reminder_no_time", 1), ("task", 3), ("note", 2),
    ("status", 3), ("ambiguous", 2), ("smalltalk", 2), ("heartbeat_manage", 1), ("recall", 1), ("proactive_followup", 2),
    ("note_action", 4), ("task_with_time", 2),
]

CHAT_PROMPT = """Generate realistic test data for a chief-of-staff AI chat app.
The user is working on: {area}. Situation: {situation}.
Return ONLY JSON: {{
 "entity": {{"name": str, "description": str, "goals": str, "active_context": str}},
 "history": [0-4 alternating {{"role": "user"|"assistant", "content": str}} turns that happened before, ending with assistant],
 "state": {{"open_tasks": [{{"title": str, "due": str|null, "priority": "low"|"normal"|"high"}}...3-6],
           "overdue_tasks": [0-2 same shape], "active_missions": [{{"title": str, "status": str}}...0-2],
           "recent_items": [{{"title": str, "kind": "task"|"note"|"reminder"}}...0-5]}},
 "message": the user's next message — {instruction}
}}
Casual, realistic phrasing; vary length and tone. No markdown in strings."""

INSTRUCTIONS = {
    "task": "they clearly ask to add/track a concrete piece of work",
    "note": "they explicitly ask to write down / save an idea or fact",
    "status": "they ask what they should focus on today, or what's overdue, or how things stand",
    "ambiguous": "a vague remark like 'this is interesting' or 'hmm we should do something about that' with no clear action",
    "smalltalk": "a casual question or chat that needs no action (advice, opinion, a quick fact)",
    "heartbeat_manage": "they ask to pause, resume, or change how often they get proactive check-ins",
    "recall": "they ask what they saved recently or what their tasks are",
    "proactive_followup": "a short reply to a proactive check-in the assistant sent, e.g. 'done, what's next?' or 'still waiting on him'",
    "note_action": "a quick jotted note of something they must do, with NO time, e.g. 'take a photo of the invoice and send it to ABT' or 'need to call the plumber about the leak'",
    "task_with_time": "they ask to add a task and mention when it is due, e.g. 'add a task to send Maria the invoice by friday'",
}
EXPECT = {
    "task": {"tool": "create_task"}, "note": {"tool": "create_note"}, "status": {"tool": "review_today"},
    "ambiguous": {"tool": "none"}, "smalltalk": {"tool": "none"}, "heartbeat_manage": {"tool": "list_heartbeats"},
    "recall": {"tool": "recall_recent"}, "proactive_followup": {},
    # A to-do with no time must still come back: a reminder, or a note/task with a follow-up time.
    "note_action": {"tool": ["set_reminder", "create_note", "create_task"], "args_any": ["when", "in_minutes"]},
    "task_with_time": {"tool": ["create_task", "set_reminder"], "args_any": ["when", "in_minutes"]},
}

REPLIES = [("done", "complete"), ("done, sent it", "complete"), ("did it 👍", "complete"),
           ("all sorted", "complete"), ("push it to tomorrow", "snooze|reschedule"),
           ("remind me in an hour", "snooze|reschedule"), ("later tonight", "snooze|reschedule"),
           ("drop it, not needed anymore", "cancel"), ("cancel that one", "cancel")]


def make_reminder_reply(i, rng):
    """The user answers a nudge. The chat sees open reminders with ids, so the
    right move is update_task on exactly that id (template-built, no teacher)."""
    todos = rng.sample(TODOS, 3)
    ids = [f"rem_{rng.randrange(16**8):08x}" for _ in todos]
    target = 0
    reply, action = rng.choice(REPLIES)
    return {
        "id": f"chat_r{i:05d}",
        "entity": {"name": "Personal", "description": "My own life admin."},
        "history": [{"role": "assistant", "content": f"Still open: {todos[target]}. Tap Done when it's handled, or snooze it."}],
        "message": reply,
        "open_reminders": [{"id": ids[0], "what": todos[0], "nudges": rng.randint(1, 3)}]
        + [{"id": x, "what": t, "nudges": 0, "in_minutes": rng.randint(30, 3000)} for x, t in zip(ids[1:], todos[1:])],
        "state": {},
        "expect": {"tool": "update_task", "args": {"task_id": ids[target], "action": action}},
        "meta": {"kind": "reminder_reply", "templated": True},
    }


def make_chat(i, teacher, rng):
    area, _ = rng.choice(AREAS)
    situation = rng.choice(SITUATIONS)
    kind = rng.choice([k for k, w in CHAT_KINDS for _ in range(w)])
    templated = kind.startswith("reminder")
    instruction = INSTRUCTIONS.get(kind, "a short casual message")
    for _ in range(3):
        try:
            data = parse_json(
                teacher.chat(
                    [{"role": "user", "content": CHAT_PROMPT.format(area=area, situation=situation, instruction=instruction)}],
                    temperature=1.0, max_tokens=1500,
                )["content"]
            )
            break
        except (ValueError, TypeError):
            data = None
    if not data:
        return None

    expect = dict(EXPECT.get(kind, {}))
    message = data.get("message", "")
    todo = rng.choice(TODOS)
    if kind == "reminder_relative":
        phrase, mins = rng.choice(RELATIVE)
        message = rng.choice(["remind me to {t} {w}", "{w} ping me to {t}", "can you nudge me {w} to {t}?", "set a reminder {w}: {t}"]).format(t=todo, w=phrase)
        expect = {"tool": "set_reminder", "args": {"in_minutes": mins}}
    elif kind == "reminder_absolute":
        phrase = rng.choice(ABSOLUTE)
        message = rng.choice(["remind me {w} to {t}", "{w}, remind me to {t}", "pls remind me to {t} {w}"]).format(t=todo, w=phrase)
        expect = {"tool": "set_reminder", "args": {"when": phrase.split()[-1] if phrase != "end of day" else "day"}}
    elif kind == "reminder_no_time":
        message = rng.choice(["remind me to {t}", "don't let me forget to {t}"]).format(t=todo)
        expect = {"tool": "set_reminder"}

    with_entity = rng.random() < 0.75
    return {
        "id": f"chat_{i:05d}",
        "entity": data.get("entity") if with_entity else None,
        "history": [h for h in (data.get("history") or []) if h.get("role") in ("user", "assistant")][-4:],
        "message": message,
        "state": data.get("state") or {},
        "expect": expect,
        "meta": {"area": area, "situation": situation, "kind": kind, "templated": templated},
    }


MOBILE_MESSAGES = [
    ("remind me to {t} {w}", "set_reminder"), ("{w} I need to {t}", "set_reminder"),
    ("need to {t}", None), ("don't let me forget: {t}", None), ("note: {t}", None),
    ("{t}", None), ("idea: {idea}", "create_note"),
]
IDEAS = ["a weekly newsletter of client wins", "selling the old desk on marketplace",
         "a loyalty card for regulars", "batching all calls on tuesdays"]


def make_mobile_chat(i, rng):
    """Phone-only offline turn, already rendered (no gateway prompt involved)."""
    from cw import mobile_agent

    when_phrase, mins = rng.choice(RELATIVE + [(a, None) for a in ABSOLUTE])
    tmpl, tool = rng.choice(MOBILE_MESSAGES)
    message = tmpl.format(t=rng.choice(TODOS), w=when_phrase, idea=rng.choice(IDEAS))
    day = rng.choice(["Monday, March 2", "Friday, July 17", "Sunday, October 18", "Wednesday, January 7"])
    time = rng.choice(["8:05 AM", "12:40 PM", "6:15 PM", "9:50 PM"])
    if tool == "set_reminder":
        expect = {"tool": ["set_reminder"], "args_any": ["when", "in_minutes"]}
    elif tool == "create_note":
        expect = {"tool": ["create_note", "set_reminder"]}
    else:
        expect = {"tool": ["set_reminder", "create_note"], "args_any": ["what", "content"]}
    return {
        "id": f"mobile_{i:05d}",
        "kind": "chat",
        "messages": [
            {"role": "system", "content": mobile_agent.system_prompt(day, time)},
            {"role": "user", "content": message},
        ],
        "tools": mobile_agent.TOOLS,
        "temperature": mobile_agent.TEMPERATURE,
        "expect": expect,
        "meta": {"surface": "mobile"},
    }


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--teacher", required=True, help="OpenAI-compatible base URL")
    p.add_argument("--teacher-model", required=True)
    p.add_argument("--beats", type=int, default=1500)
    p.add_argument("--chats", type=int, default=800)
    p.add_argument("--mobile", type=int, default=400, help="phone-only offline turns (no teacher needed)")
    p.add_argument("--replies", type=int, default=300, help="answers to nudges (no teacher needed)")
    p.add_argument("--out", default="data/scenarios")
    p.add_argument("--workers", type=int, default=16)
    p.add_argument("--seed", type=int, default=7)
    args = p.parse_args()

    teacher = make_endpoint(args.teacher, args.teacher_model)
    os.makedirs(args.out, exist_ok=True)

    def run(kind, n, fn):
        path = os.path.join(args.out, f"{kind}.jsonl")
        with ThreadPoolExecutor(args.workers) as pool, open(path, "w") as f:
            futures = [pool.submit(fn, i, teacher, random.Random(args.seed * 100003 + i)) for i in range(n)]
            ok = 0
            for fut in futures:
                try:
                    row = fut.result()
                except Exception as e:  # keep going; a few bad generations are fine
                    print("skip:", e, file=sys.stderr)
                    continue
                if row:
                    f.write(json.dumps(row) + "\n")
                    ok += 1
        print(f"{kind}: {ok}/{n} -> {path}")

    run("beats", args.beats, make_beat)
    run("chats", args.chats, make_chat)
    with open(os.path.join(args.out, "chats.jsonl"), "a") as f:
        for i in range(args.replies):
            f.write(json.dumps(make_reminder_reply(i, random.Random(args.seed * 104729 + i))) + "\n")
    print(f"reminder replies: {args.replies} appended to chats.jsonl")
    path = os.path.join(args.out, "mobile_chats.jsonl")
    with open(path, "w") as f:
        for i in range(args.mobile):
            f.write(json.dumps(make_mobile_chat(i, random.Random(args.seed * 7919 + i))) + "\n")
    print(f"mobile_chats: {args.mobile} -> {path}")


if __name__ == "__main__":
    main()
