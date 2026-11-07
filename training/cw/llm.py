"""Minimal stdlib client for OpenAI-compatible chat endpoints.

Works with vLLM / llama.cpp server / Ollama's /v1 / hosted APIs alike, so the
teacher (data generation) and the student (evaluation) share one code path.
"""

import json
import os
import time
import urllib.error
import urllib.request


class Endpoint:
    def __init__(self, base_url, model, api_key=None, timeout=600):
        self.base_url = base_url.rstrip("/")
        if not self.base_url.endswith("/v1"):
            self.base_url += "/v1"
        self.model = model
        self.api_key = api_key or os.environ.get("TEACHER_API_KEY") or "none"
        self.timeout = timeout

    def chat(self, messages, tools=None, temperature=0.7, max_tokens=1024, retries=3):
        """Returns {"content": str|None, "tool_calls": [{"id","name","arguments"}]}."""
        body = {
            "model": self.model,
            "messages": messages,
            "temperature": temperature,
            "max_tokens": max_tokens,
        }
        if tools:
            body["tools"] = [
                {"type": "function", "function": {"name": t["name"], "description": t["description"], "parameters": t["parameters"]}}
                for t in tools
            ]
        # Ollama: keep Gemma's thinking off, exactly like the gateway does.
        body["think"] = False
        body["chat_template_kwargs"] = {"enable_thinking": False}
        data = json.dumps(body).encode()
        last = None
        for attempt in range(retries):
            req = urllib.request.Request(
                self.base_url + "/chat/completions",
                data=data,
                headers={"Content-Type": "application/json", "Authorization": "Bearer " + self.api_key},
            )
            try:
                with urllib.request.urlopen(req, timeout=self.timeout) as r:
                    out = json.loads(r.read())
                msg = out["choices"][0]["message"]
                calls = []
                for i, c in enumerate(msg.get("tool_calls") or []):
                    args = c["function"].get("arguments") or "{}"
                    try:
                        args = json.loads(args) if isinstance(args, str) else args
                    except ValueError:
                        pass
                    calls.append({"id": c.get("id") or f"call_{i}", "name": c["function"]["name"], "arguments": args})
                return {"content": msg.get("content"), "tool_calls": calls}
            except (urllib.error.URLError, KeyError, ValueError) as e:
                last = e
                time.sleep(2 * (attempt + 1))
        raise RuntimeError(f"{self.base_url}: {last}")


class OllamaEndpoint:
    """Ollama's native /api/chat, called exactly as crates/llm/src/ollama.rs does."""

    def __init__(self, model, base_url="http://localhost:11434", timeout=900):
        self.base_url = base_url.rstrip("/").removesuffix("/v1")
        self.model = model
        self.timeout = timeout

    def chat(self, messages, tools=None, temperature=None, max_tokens=None, **_):
        msgs = []
        names = {}
        for m in messages:
            out = {"role": m["role"], "content": m.get("content") or ""}
            if m.get("tool_calls"):
                out["tool_calls"] = []
                for c in m["tool_calls"]:
                    fn = c.get("function") or c  # OpenAI wire shape or gateway shape
                    args = fn.get("arguments") or {}
                    if isinstance(args, str):
                        args = json.loads(args or "{}")
                    names[c["id"]] = fn["name"]
                    out["tool_calls"].append({"id": c["id"], "function": {"name": fn["name"], "arguments": args}})
            if m["role"] == "tool":
                out["tool_name"] = m.get("name") or names.get(m.get("tool_call_id"), "")
            msgs.append(out)
        body = {"model": self.model, "messages": msgs, "stream": False, "think": False,
                "options": {"num_ctx": 16384}}
        if temperature is not None:
            body["options"]["temperature"] = temperature
        if max_tokens:
            body["options"]["num_predict"] = max_tokens
        if tools:
            body["tools"] = [{"type": "function", "function": t} for t in tools]
        req = urllib.request.Request(self.base_url + "/api/chat", data=json.dumps(body).encode(),
                                     headers={"Content-Type": "application/json"})
        with urllib.request.urlopen(req, timeout=self.timeout) as r:
            out = json.loads(r.read())
        msg = out.get("message") or {}
        calls = [
            {"id": c.get("id") or f"call_{i}", "name": c["function"]["name"], "arguments": c["function"].get("arguments") or {}}
            for i, c in enumerate(msg.get("tool_calls") or [])
        ]
        return {"content": msg.get("content"), "tool_calls": calls,
                "eval_count": out.get("eval_count"), "total_ms": (out.get("total_duration") or 0) / 1e6}


def to_openai_messages(messages):
    """Gateway trace/render messages -> OpenAI wire messages."""
    out = []
    for m in messages:
        role = m["role"]
        msg = {"role": role, "content": m.get("content", "")}
        if m.get("tool_calls"):
            msg["tool_calls"] = [
                {
                    "id": c["id"],
                    "type": "function",
                    "function": {
                        "name": c["name"],
                        "arguments": c["arguments"] if isinstance(c["arguments"], str) else json.dumps(c["arguments"]),
                    },
                }
                for c in m["tool_calls"]
            ]
            if not msg["content"]:
                msg["content"] = None
        if m.get("tool_call_id"):
            msg["tool_call_id"] = m["tool_call_id"]
        out.append(msg)
    return out


def make_endpoint(base_url, model, **kw):
    """Ollama gets its native API (thinking reliably off, as the gateway calls
    it); anything else is treated as OpenAI-compatible."""
    if ":1143" in base_url or "ollama" in base_url.lower():
        return OllamaEndpoint(model, base_url)
    return Endpoint(base_url, model, **kw)
