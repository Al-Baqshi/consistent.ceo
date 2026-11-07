"""Stand-ins for the gateway's chat tools during data generation.

Return shapes copy crates/gateway/src/chat_tools.rs so the teacher sees the
same kind of results the real tools give, and learns the follow-up turn
("confirm briefly, don't recite IDs") on realistic data.
"""

import json
import random
import uuid
from datetime import datetime, timedelta, timezone


def _id(prefix):
    return f"{prefix}_{uuid.uuid4().hex[:10]}"


def run(name, args, scenario, now=None):
    now = now or datetime.now(timezone.utc)
    ent = (scenario.get("entity") or {}).get("name")
    state = scenario.get("state") or {}
    if isinstance(args, str):
        try:
            args = json.loads(args)
        except ValueError:
            return {"ok": False, "error": "bad arguments: expected a JSON object"}

    if name == "set_reminder":
        mins = args.get("in_minutes")
        if isinstance(mins, (int, float)):
            fires = (now + timedelta(minutes=mins)).isoformat()
            return {"ok": True, "id": _id("task"), "kind": "reminder", "fires_at": fires}
        if args.get("when"):
            fires = (now + timedelta(hours=random.choice([3, 18, 26]))).replace(minute=0, second=0).isoformat()
            return {"ok": True, "id": _id("task"), "kind": "reminder", "fires_at": fires}
        return {
            "ok": True, "id": _id("task"), "kind": "reminder", "fires_at": None,
            "note": "time not understood; reminder saved but will not auto-fire — ask the user for a concrete time",
        }
    if name == "create_task":
        return {"ok": True, "id": _id("task"), "kind": "task", "title": args.get("title", "")}
    if name == "create_note":
        return {"ok": True, "id": _id("task"), "kind": "note", "title": args.get("title") or (args.get("content", "")[:40])}
    if name == "review_today":
        return {
            "ok": True,
            "generated_at": now.isoformat(),
            "open_tasks": state.get("open_tasks", []),
            "overdue_tasks": state.get("overdue_tasks", []),
            "active_missions": state.get("active_missions", []),
            "upcoming_heartbeats": state.get("upcoming_heartbeats", []),
        }
    if name == "recall_recent":
        return {"ok": True, "items": state.get("recent_items", [])[: int(args.get("limit") or 10)]}
    if name == "list_entities":
        return {"ok": True, "entities": state.get("entities") or ([{"id": "ent_scenario", "name": ent}] if ent else [])}
    if name == "list_heartbeats":
        return {"ok": True, "heartbeats": state.get("heartbeats", [])}
    if name == "manage_heartbeat":
        return {"ok": True, "action": args.get("action"), "id": args.get("id") or _id("hb")}
    if name in ("update_task", "update_entity", "record_lesson", "create_mission", "create_entity", "save_skill"):
        return {"ok": True, "id": args.get("id") or _id(name.split("_")[-1])}
    if name in ("save_note_to_desktop", "write_local_file", "create_folder"):
        return {"ok": True, "path": "~/Desktop/Consistent/" + (args.get("title") or args.get("path") or "note")}
    if name == "load_skill":
        return {"ok": False, "error": "no such skill"}
    if name == "run_on_computer":
        return {"ok": True, "queued": True, "id": _id("task")}
    return {"ok": True}
