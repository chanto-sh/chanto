#!/usr/bin/env python3
"""Misbehaving stdio MCP server for chaos tests.

    python3 tests/chaos/fakemcp.py MODE [MARKER]

MODE:
  ok          well-behaved: one tool `echo`
  crash_init  exits before answering initialize
  hang_init   never answers initialize
  hang_call   answers everything but never answers tools/call
  crash_call  exits on tools/call
  garbage     writes non-JSON lines to stdout, then behaves
  huge        tools/call returns 20 MB of text
  many_tools  tools/list returns 5000 tools
  bad_schema  tool with a non-object inputSchema and weird names
  stderr_spam floods stderr while working
If MARKER is given, the server touches that file at start (to prove it ran).
"""
import json
import os
import sys
import time

MODE = sys.argv[1] if len(sys.argv) > 1 else "ok"
if len(sys.argv) > 2:
    open(sys.argv[2], "a").close()


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def tools():
    if MODE == "many_tools":
        return [{"name": f"t{i}", "description": "x" * 200, "inputSchema": {"type": "object", "properties": {}}} for i in range(5000)]
    if MODE == "bad_schema":
        return [
            {"name": "weird name/with spaces", "description": None, "inputSchema": "nope"},
            {"name": "", "inputSchema": {}},
            {"name": "a" * 300, "inputSchema": {"type": "array"}},
            {"name": "echo", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}},
        ]
    return [{"name": "echo", "description": "Echo text back", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}}]


if MODE == "garbage":
    sys.stdout.write("hello this is not json\n\x00\xff garbage {{{\n")
    sys.stdout.flush()
if MODE == "crash_init":
    sys.exit(3)

for line in sys.stdin:
    if MODE == "stderr_spam":
        sys.stderr.write("noise " * 2000 + "\n")
    try:
        msg = json.loads(line)
    except Exception:
        continue
    mid = msg.get("id")
    method = msg.get("method")
    if mid is None:
        continue
    if method == "initialize":
        if MODE == "hang_init":
            time.sleep(3600)
        send({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": msg.get("params", {}).get("protocolVersion", "2025-06-18"), "capabilities": {"tools": {}}, "serverInfo": {"name": "fake", "version": "1"}}})
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": mid, "result": {"tools": tools()}})
    elif method == "tools/call":
        if MODE == "hang_call":
            continue
        if MODE == "crash_call":
            os._exit(1)
        text = "z" * (20 * 1024 * 1024) if MODE == "huge" else json.dumps(msg.get("params", {}).get("arguments", {}))
        send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": text}]}})
    else:
        send({"jsonrpc": "2.0", "id": mid, "result": {}})
