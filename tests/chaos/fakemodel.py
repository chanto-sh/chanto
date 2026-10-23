#!/usr/bin/env python3
"""Scriptable fake model server for chaos-testing Chanto without real models.

Speaks OpenAI `/v1/chat/completions` (SSE) and Ollama `/api/chat` (NDJSON),
plus the probes Chanto makes (`/v1/models`, `/api/tags`, `/api/show`).

Usage:
    python3 tests/chaos/fakemodel.py SCENARIO [--port N] [--log FILE]

Prints `PORT <n>` on stdout once listening. Point Chanto at it with
    CHANTO_PROVIDER=openai CHANTO_BASE_URL=http://127.0.0.1:<n>/v1 CHANTO_MODEL=fake
or  CHANTO_PROVIDER=ollama CHANTO_BASE_URL=http://127.0.0.1:<n> CHANTO_MODEL=fake

SCENARIO is a .json file (a list of turns, or {"turns": [...], "default": turn})
or a .py file defining `TURNS` (list) and/or `respond(body, n) -> turn | None`.
Each chat request takes the first unused turn whose `when` (a substring of the
request's last message, or of the whole request with `when_any`) matches.
When none is left, `default` answers (plain text "done.").

A turn (all keys optional):
    text            assistant text (streamed in `chunk`-sized pieces)
    reasoning       reasoning text (`reasoning_content` / Ollama `thinking`)
    tool_calls      [{"name": "Read", "args": {...}}]  or "args_raw": "<string>"
                    also "id", "index" (OpenAI index override)
    status          HTTP status (default 200); body from `body` (string)
    headers         extra response headers, e.g. {"retry-after": "0"}
    raw             literal response body bytes (string; "\\x.." escapes via raw_b64)
    raw_b64         base64 literal response body
    chunk           characters per text delta (default 40)
    drip_ms         delay between stream events
    stall_after     stop sending (keep socket open) after N events, for stall_s seconds (default 3600)
    reset_after     close the socket abruptly after N events
    no_done         omit the final `[DONE]` / done line
    usage           {"prompt_tokens": n, "completion_tokens": m}
    finish_reason   default "stop" / "tool_calls"
    repeat          use this turn N times (default 1)
    when / when_any substring match (see above)
    sleep_s         wait before answering at all
"""
import base64
import http.server
import json
import os
import socket
import socketserver
import sys
import threading
import time
import uuid

LOCK = threading.Lock()
STATE = {"n": 0, "used": set()}


def load(path):
    if path.endswith(".py"):
        ns = {"__file__": path}
        with open(path) as f:
            exec(compile(f.read(), path, "exec"), ns)
        return ns.get("TURNS", []), ns.get("respond"), ns.get("DEFAULT")
    with open(path) as f:
        data = json.load(f)
    if isinstance(data, list):
        return data, None, None
    return data.get("turns", []), None, data.get("default")


def last_text(body):
    msgs = body.get("messages") or []
    if not msgs:
        return ""
    c = msgs[-1].get("content")
    if isinstance(c, list):
        return " ".join(p.get("text", "") for p in c if isinstance(p, dict))
    return c or ""


def pick(body):
    turns, respond, default = SCEN
    with LOCK:
        n = STATE["n"]
        STATE["n"] += 1
    if respond:
        t = respond(body, n)
        if t is not None:
            return t
    lt = last_text(body)
    whole = None
    with LOCK:
        for i, t in enumerate(turns):
            uses = STATE.setdefault("uses", {}).get(i, 0)
            if uses >= t.get("repeat", 1):
                continue
            if "when" in t and t["when"] not in lt:
                continue
            if "when_any" in t:
                whole = whole or json.dumps(body)
                if t["when_any"] not in whole:
                    continue
            STATE["uses"][i] = uses + 1
            return t
    return default or {"text": "done."}


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def log(self, kind, body):
        if not LOGF:
            return
        rec = {"t": time.time(), "path": self.path, "kind": kind}
        if isinstance(body, dict):
            msgs = body.get("messages") or []
            rec["n_messages"] = len(msgs)
            rec["n_tools"] = len(body.get("tools") or [])
            rec["roles"] = "".join((m.get("role") or "?")[0] for m in msgs)
            rec["last"] = str(last_text(body))[:300]
            rec["bytes"] = int(self.headers.get("content-length") or 0)
            rec["model"] = body.get("model")
        with LOCK, open(LOGF, "a") as f:
            f.write(json.dumps(rec) + "\n")
        # Full request bodies, for checking what reached the model.
        dump = os.environ.get("FAKE_DUMP")
        if dump and isinstance(body, dict):
            os.makedirs(dump, exist_ok=True)
            with LOCK:
                STATE["dumped"] = STATE.get("dumped", 0) + 1
                k = STATE["dumped"]
            with open(os.path.join(dump, f"{k:04d}.json"), "w") as f:
                json.dump(body, f)

    def send_json(self, code, obj, extra=None):
        b = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b)))
        for k, v in (extra or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(b)

    def do_GET(self):
        p = self.path
        if p.endswith("/models"):
            return self.send_json(200, {"object": "list", "data": [{"id": "fake", "object": "model", "context_length": CTX}]})
        if p.endswith("/api/tags"):
            return self.send_json(200, {"models": [{"name": "fake", "model": "fake"}]})
        if p.endswith("/api/version"):
            return self.send_json(200, {"version": "0.9.0"})
        self.send_json(404, {"error": "not found"})

    def read_body(self):
        n = int(self.headers.get("content-length") or 0)
        raw = self.rfile.read(n) if n else b""
        try:
            return json.loads(raw)
        except Exception:
            return {}

    def do_POST(self):
        body = self.read_body()
        if self.path.endswith("/api/show"):
            return self.send_json(200, {"model_info": {"general.architecture": "fake", "fake.context_length": CTX}, "capabilities": ["completion", "tools"]})
        ollama = self.path.endswith("/api/chat")
        if not (ollama or self.path.endswith("/chat/completions")):
            return self.send_json(404, {"error": f"unknown path {self.path}"})
        t = pick(body)
        self.log("chat", body)
        if t.get("sleep_s"):
            time.sleep(t["sleep_s"])
        status = t.get("status", 200)
        headers = t.get("headers", {})
        if status != 200 or "body" in t:
            b = t.get("body", json.dumps({"error": {"message": f"fake error {status}"}})).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(b)))
            for k, v in headers.items():
                self.send_header(k, v)
            self.end_headers()
            self.wfile.write(b)
            return
        events = self.ollama_events(t) if ollama else self.openai_events(t)
        if "raw" in t or "raw_b64" in t:
            events = [t["raw"].encode() if "raw" in t else base64.b64decode(t["raw_b64"])]
        self.send_response(200)
        self.send_header("Content-Type", "application/x-ndjson" if ollama else "text/event-stream")
        for k, v in headers.items():
            self.send_header(k, v)
        self.send_header("Connection", "close")
        self.end_headers()
        self.close_connection = True
        drip = t.get("drip_ms", 0) / 1000
        try:
            for i, ev in enumerate(events):
                if "stall_after" in t and i == t["stall_after"]:
                    self.wfile.flush()
                    time.sleep(t.get("stall_s", 3600))
                if "reset_after" in t and i == t["reset_after"]:
                    self.wfile.flush()
                    s = self.connection
                    s.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, b"\x01\x00\x00\x00\x00\x00\x00\x00")
                    s.close()
                    return
                self.wfile.write(ev)
                if drip:
                    self.wfile.flush()
                    time.sleep(drip)
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass

    @staticmethod
    def pieces(s, n):
        return [s[i:i + n] for i in range(0, len(s), n)] if s else []

    def openai_events(self, t):
        out = []
        def ev(obj):
            out.append(b"data: " + json.dumps(obj).encode() + b"\n\n")
        def delta(d, fin=None):
            ev({"id": "x", "object": "chat.completion.chunk", "choices": [{"index": 0, "delta": d, "finish_reason": fin}]})
        n = t.get("chunk", 40)
        for p in self.pieces(t.get("reasoning", ""), n):
            delta({"reasoning_content": p})
        for p in self.pieces(t.get("text", ""), n):
            delta({"content": p})
        for i, c in enumerate(t.get("tool_calls", [])):
            args = c["args_raw"] if "args_raw" in c else json.dumps(c.get("args", {}))
            idx = c.get("index", i)
            cid = c.get("id", "call_" + uuid.uuid4().hex[:8])
            first = {"index": idx, "id": cid, "type": "function", "function": {"name": c["name"], "arguments": ""}}
            delta({"tool_calls": [first]})
            for p in self.pieces(args, c.get("chunk", 64)):
                delta({"tool_calls": [{"index": idx, "function": {"arguments": p}}]})
        fin = t.get("finish_reason", "tool_calls" if t.get("tool_calls") else "stop")
        delta({}, fin)
        ev({"id": "x", "choices": [], "usage": t.get("usage", {"prompt_tokens": 100, "completion_tokens": 10})})
        if not t.get("no_done"):
            out.append(b"data: [DONE]\n\n")
        return out

    def ollama_events(self, t):
        out = []
        def ev(msg, done=False, **extra):
            o = {"model": "fake", "message": {"role": "assistant", "content": "", **msg}, "done": done}
            o.update(extra)
            out.append(json.dumps(o).encode() + b"\n")
        n = t.get("chunk", 40)
        for p in self.pieces(t.get("reasoning", ""), n):
            ev({"thinking": p})
        for p in self.pieces(t.get("text", ""), n):
            ev({"content": p})
        calls = []
        for c in t.get("tool_calls", []):
            args = c["args_raw"] if "args_raw" in c else c.get("args", {})
            calls.append({"function": {"name": c["name"], "arguments": args}})
        if calls:
            ev({"tool_calls": calls})
        if not t.get("no_done"):
            u = t.get("usage", {"prompt_tokens": 100, "completion_tokens": 10})
            ev({}, True, done_reason=t.get("finish_reason", "stop"), prompt_eval_count=u["prompt_tokens"], eval_count=u["completion_tokens"])
        return out


class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    allow_reuse_address = True


def main():
    global SCEN, LOGF, CTX
    args = sys.argv[1:]
    port = 0
    LOGF = None
    CTX = 131072
    if "--port" in args:
        i = args.index("--port"); port = int(args[i + 1]); del args[i:i + 2]
    if "--log" in args:
        i = args.index("--log"); LOGF = args[i + 1]; del args[i:i + 2]
    if "--ctx" in args:
        i = args.index("--ctx"); CTX = int(args[i + 1]); del args[i:i + 2]
    SCEN = load(args[0]) if args else ([], None, None)
    srv = Server(("127.0.0.1", port), Handler)
    print("PORT", srv.server_address[1], flush=True)
    srv.serve_forever()


if __name__ == "__main__":
    main()
