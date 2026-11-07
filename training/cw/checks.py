"""Rule-based graders for Consistent outputs.

Shared by distill.py (to keep only good teacher answers) and eval.py (to score
a model). Every rule mirrors an instruction the gateway actually gives the
model, so "passing" means "did what production asked".
"""

import difflib
import json
import re

# Word caps straight from director_runner.rs::intent_instruction.
WORD_CAPS = {"reminder": 120, "review": 150, "drive": 120, "generic": 120, "setup": 60, "ideas": 110, "loops": 60}

GREETING = re.compile(r"^\s*(hi|hello|hey|good (morning|afternoon|evening)|dear|greetings)\b", re.I)
SIGNOFF = re.compile(
    r"(let me know|hope this helps|best regards|cheers|talk soon|have a (great|good)|happy to help)[\s!.]*$",
    re.I,
)
MARKDOWN = re.compile(r"(^\s{0,3}#{1,6}\s|^\s*[-*•]\s|^\s*\d+[.)]\s|\*\*|__|`)", re.M)
# A beat has no tools, so any claim of having done something is false.
FALSE_ACTION = re.compile(
    r"\bI(?:'ve| have)?\s+(?:just\s+)?(created|scheduled|sent|saved|set up|booked|emailed|added|updated|filed|logged)\b",
    re.I,
)
STOP = set(
    "the a an and or of to in on for with at by from this that these those is are was were be been it its as "
    "your you we our their they he she his her them not no yes but if then so than into over under".split()
)


def words(text):
    return re.findall(r"[A-Za-z0-9][A-Za-z0-9'’\-]*", text)


def _salient_terms(text):
    """Names and numbers worth seeing echoed back: proof the answer is grounded."""
    terms = set()
    for w in words(text):
        if any(c.isdigit() for c in w):
            terms.add(w.lower())
        elif w[0].isupper() and len(w) > 2 and w.lower() not in STOP:
            terms.add(w.lower())
    return terms


def context_of(messages):
    """The part of the prompt describing this entity (not the boilerplate)."""
    system = messages[0]["content"] if messages and messages[0]["role"] == "system" else ""
    keep = []
    for marker in ("## Active entity", "## Active mission", "## Entity memory", "## Entity docs", "## Lessons learned"):
        i = system.find(marker)
        if i >= 0:
            j = system.find("\n## ", i + 3)
            keep.append(system[i : j if j > 0 else len(system)])
    user = messages[-1]["content"] if messages else ""
    return "\n".join(keep) + "\n" + user


def recent_beats(user_prompt):
    m = re.search(r"newest first[^\n]*:\n((?:\d+\. .*\n?)+)", user_prompt)
    if not m:
        return []
    return [re.sub(r"^\d+\. ", "", l).strip() for l in m.group(1).strip().split("\n")]


def open_items(user_prompt):
    """The `what` of each open item a check-in was shown ("- [tag] what (Entity)")."""
    items = []
    for line in user_prompt.splitlines():
        if line.startswith("- [") and "] " in line:
            what = line.split("] ", 1)[1]
            items.append(re.sub(r" \([^)]*\)$", "", what).strip().lower())
    return items


def mentions(answer, what):
    """Does `answer` talk about the item `what` (most of its content words)?"""
    answer = answer.lower()
    words = [w for w in re.findall(r"[a-z0-9]+", what.lower()) if len(w) > 3]
    if not words:
        return what.lower() in answer
    return sum(w in answer for w in words) >= max(1, (len(words) + 1) // 2)


def check_beat(row, answer):
    """Score one heartbeat answer. Returns {"pass": bool, "score": 0..1, "fails": [...]}."""
    intent = row.get("intent", "generic")
    text = (answer or "").strip()
    fails = []

    if not text:
        return {"pass": False, "score": 0.0, "fails": ["empty"]}

    # The hourly check-in may (and sometimes must) stay silent.
    is_skip = text.strip(" .!\n").upper() == "SKIP"
    if intent == "loops":
        expect_skip = bool((row.get("meta") or {}).get("expect_skip"))
        if expect_skip or is_skip:
            ok = expect_skip == is_skip
            return {"pass": ok, "score": 1.0 if ok else 0.0,
                    "fails": [] if ok else ["should_skip" if expect_skip else "skipped_needed_nudge"], "words": 1}
    n = len(words(text))
    cap = WORD_CAPS.get(intent, 120)
    if n > cap:
        fails.append(f"too_long:{n}>{cap}")
    if GREETING.search(text):
        fails.append("greeting")
    if SIGNOFF.search(text):
        fails.append("signoff")
    if intent != "ideas" and MARKDOWN.search(text):
        fails.append("markdown")
    if intent == "ideas" and re.search(r"\*\*|^\s{0,3}#", text, re.M):
        fails.append("markdown")
    if FALSE_ACTION.search(text):
        fails.append("claims_action")

    if intent == "setup":
        if text.count("?") != 1:
            fails.append(f"setup_questions:{text.count('?')}")
    if intent == "ideas":
        options = [l for l in text.split("\n") if l.strip()]
        if not (2 <= len(options) <= 3) and "nothing new" not in text.lower():
            fails.append(f"ideas_count:{len(options)}")

    # Setup beats exist because there is nothing to ground in yet.
    ctx_terms = _salient_terms(context_of(row["messages"]))
    hits = ctx_terms & _salient_terms(text)
    if intent == "loops":
        # A check-in is grounded when it talks about an item it was shown.
        grounded = any(mentions(text, w) for w in open_items(row["messages"][-1]["content"]))
    else:
        grounded = intent == "setup" or len(hits) >= 2 or not ctx_terms
    if not grounded:
        fails.append("ungrounded")

    for prior in recent_beats(row["messages"][-1]["content"]):
        if difflib.SequenceMatcher(None, prior.lower(), text.lower()).ratio() > 0.75:
            fails.append("repeats_prior_beat")
            break

    hard = [f for f in fails if not f.startswith("ungrounded")]
    score = max(0.0, 1.0 - 0.25 * len(fails))
    return {"pass": not hard and grounded, "score": score, "fails": fails, "words": n}


def _type_ok(value, schema):
    t = schema.get("type")
    if t == "string":
        ok = isinstance(value, str)
    elif t == "integer":
        ok = isinstance(value, int) and not isinstance(value, bool)
    elif t == "number":
        ok = isinstance(value, (int, float)) and not isinstance(value, bool)
    elif t == "boolean":
        ok = isinstance(value, bool)
    elif t == "array":
        ok = isinstance(value, list)
    elif t == "object":
        ok = isinstance(value, dict)
    else:
        ok = True
    if ok and "enum" in schema:
        ok = value in schema["enum"]
    return ok


def validate_call(call, tools):
    """Check one tool call against the offered schemas. Returns a list of problems."""
    by_name = {t["name"]: t for t in tools}
    name = call.get("name")
    if name not in by_name:
        return [f"unknown_tool:{name}"]
    args = call.get("arguments")
    if isinstance(args, str):
        try:
            args = json.loads(args)
        except ValueError:
            return [f"bad_json:{name}"]
    if not isinstance(args, dict):
        return [f"args_not_object:{name}"]
    params = by_name[name].get("parameters") or {}
    props = params.get("properties") or {}
    problems = [f"missing:{name}.{r}" for r in params.get("required", []) if r not in args]
    for k, v in args.items():
        if k not in props:
            problems.append(f"extra_arg:{name}.{k}")
        elif not _type_ok(v, props[k]):
            problems.append(f"bad_type:{name}.{k}")
    return problems


def check_chat_step(tools, response, expect=None):
    """Score the model's first move on a chat turn.

    `expect` (optional, from the scenario) may hold:
      tool: name the model should call, or "none" for a plain reply
      args: subset of arguments that must match (numbers within 10%)
    """
    calls = response.get("tool_calls") or []
    fails = []
    for c in calls:
        fails += validate_call(c, tools)
    content = (response.get("content") or "").strip()
    if not calls and not content:
        fails.append("empty")
    if not calls and FALSE_ACTION.search(content):
        fails.append("claims_action_without_tool")

    if expect:
        want = expect.get("tool")
        names = [c.get("name") for c in calls]
        if isinstance(want, list):
            # Any of several tools is right (e.g. a note or a reminder).
            if not set(want) & set(names):
                fails.append(f"expected_one_of:{want} got {names or 'none'}")
            want = next((n for n in names if n in want), None)
            if want and expect.get("args_any"):
                call = next(c for c in calls if c.get("name") == want)
                args = call.get("arguments")
                if isinstance(args, str):
                    args = json.loads(args or "{}")
                if not any(args.get(k) not in (None, "") for k in expect["args_any"]):
                    fails.append(f"missing_any:{expect['args_any']}")
            return {"pass": not fails, "score": max(0.0, 1.0 - 0.34 * len(fails)), "fails": fails}
        if want == "none" and calls:
            fails.append(f"unexpected_tool:{names}")
        elif want and want != "none" and want not in names:
            fails.append(f"expected_tool:{want} got {names or 'none'}")
        if want and want != "none" and want in names and expect.get("args"):
            call = next(c for c in calls if c.get("name") == want)
            args = call.get("arguments")
            if isinstance(args, str):
                args = json.loads(args or "{}")
            for k, v in expect["args"].items():
                got = args.get(k)
                if isinstance(v, (int, float)) and isinstance(got, (int, float)):
                    if abs(got - v) > max(1, 0.1 * abs(v)):
                        fails.append(f"arg:{k}={got}!={v}")
                elif isinstance(v, str) and isinstance(got, str):
                    # "a|b" accepts either (e.g. snooze|reschedule).
                    if not any(alt.lower() in got.lower() for alt in v.split("|")):
                        fails.append(f"arg:{k}")
                elif got != v:
                    fails.append(f"arg:{k}")
    return {"pass": not fails, "score": max(0.0, 1.0 - 0.34 * len(fails)), "fails": fails}
