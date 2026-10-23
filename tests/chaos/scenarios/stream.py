# Streaming/protocol chaos. Pick a case with CASE=<name>; the first chat
# request gets the chaos, later ones answer "done." so the turn can finish.
import base64
import json
import os

CASE = os.environ.get("CASE", "")
BIG = int(os.environ.get("BIG", 10 * 1024 * 1024))


def sse(*objs):
    return "".join("data: " + json.dumps(o) + "\n\n" for o in objs)


def delta(d, fin=None):
    return {"choices": [{"index": 0, "delta": d, "finish_reason": fin}]}


CASES = {
    # stream ends mid-JSON without [DONE]
    "truncated": {"raw": sse(delta({"content": "partial answer "})) + 'data: {"choices":[{"delta":{"content":"cut'},
    # tool call whose arguments never close
    "truncated_args": {"tool_calls": [{"name": "Write", "args_raw": '{"file_path": "x.txt", "content": "abc'}], "no_done": True},
    # output cap hit mid-Write: the server says so with finish_reason "length"
    "length_args": {"tool_calls": [{"name": "Write", "args_raw": '{"file_path": "a.txt", "content": "line1\\nline2\\nli'}], "finish_reason": "length"},
    # clean EOF mid tool-call arguments: no finish_reason, no [DONE]
    "eof_mid_args": {"raw": sse(delta({"tool_calls": [{"index": 0, "id": "c1", "type": "function", "function": {"name": "Write", "arguments": ""}}]}),
                                delta({"tool_calls": [{"index": 0, "function": {"arguments": '{"file_path": "a.txt", "content": "line1\\nline2\\nli'}}]}))},
    "eof_mid_text": {"raw": sse(delta({"content": "Here is the full plan: step 1"}))},
    "length_text": {"text": "The answer is", "finish_reason": "length"},
    "invalid_args": {"tool_calls": [{"name": "Read", "args_raw": "not json at all {{{"}]},
    "args_array": {"tool_calls": [{"name": "Read", "args_raw": "[1,2,3]"}]},
    "args_wrong_types": {"tool_calls": [{"name": "Read", "args": {"file_path": 42, "offset": "ten", "limit": -5}}]},
    "args_null": {"tool_calls": [{"name": "Read", "args_raw": "null"}]},
    "unknown_tool": {"tool_calls": [{"name": "DeleteEverything", "args": {}}]},
    "empty_name": {"tool_calls": [{"name": "", "args": {"file_path": "a.txt"}}]},
    "huge_index": {"tool_calls": [{"name": "Read", "args": {"file_path": "a.txt"}, "index": int(os.environ.get("IDX", 3000000))}]},
    "html_body": {"raw": "<html><body>502 Bad Gateway</body></html>"},
    "empty_body": {"raw": ""},
    "only_done": {"raw": "data: [DONE]\n\n"},
    "error_event": {"raw": sse({"error": {"message": "model overloaded", "code": 529}})},
    "reset": {"text": "x" * 4000, "chunk": 10, "reset_after": 50},
    "stall": {"text": "starting... and then nothing", "chunk": 5, "stall_after": 2},
    "drip": {"text": "slow " * 60, "chunk": 1, "drip_ms": 20},
    "huge_text": {"text": "lorem ipsum dolor sit amet\n" * (BIG // 27), "chunk": 65536},
    "huge_line": {"text": "y" * BIG, "chunk": BIG},
    "invalid_utf8": {"raw_b64": base64.b64encode(b'data: {"choices":[{"delta":{"content":"bad \xff\xfe bytes \xc3"}}]}\n\ndata: [DONE]\n\n').decode()},
    "unicode": {"text": "emoji \U0001F600\U0001F469‍\U0001F469‍\U0001F467 rtl שלום ‮evil‬ combining é́́́ zalgo Z̶̷̸̵ nul\u0000byte bell\u0007 esc\u001b[2J\u001b]0;pwned\u0007 done", "chunk": 3},
    "ansi_title": {"text": "\u001b]0;HACKED\u0007\u001b[31mred\u001b[0m \u001b[?1049h alt screen \u001b[H\u001b[2J cleared"},
    "empty": {"text": ""},
    "reasoning_only": {"reasoning": "thinking hard " * 20},
    "many_calls": {"tool_calls": [{"name": "Read", "args": {"file_path": "a.txt"}} for _ in range(int(os.environ.get("N", 1000)))]},
    "dup_ids": {"tool_calls": [{"name": "Read", "args": {"file_path": "a.txt"}, "id": "same"}, {"name": "Read", "args": {"file_path": "a.txt"}, "id": "same"}]},
    "status_429_0": {"status": 429, "headers": {"retry-after": "0"}, "repeat": 3},
    "status_429_huge": {"status": 429, "headers": {"retry-after": "999999999"}},
    "status_429_garbage": {"status": 429, "headers": {"retry-after": "Wed, 21 Oct 2015 07:28:00 GMT"}, "repeat": 2},
    "status_429_nan": {"status": 429, "headers": {"retry-after": "NaN"}, "repeat": 2},
    "status_429_neg": {"status": 429, "headers": {"retry-after": "-5"}, "repeat": 2},
    "status_500_storm": {"status": 500, "headers": {"retry-after": "0"}, "repeat": 100},
    "status_529_storm": {"status": 529, "headers": {"retry-after": "0"}, "repeat": 100},
    "status_400": {"status": 400, "body": json.dumps({"error": {"message": "context length exceeded: maximum context length is 8192 tokens"}})},
    "status_401": {"status": 401, "body": "unauthorized"},
    "status_413": {"status": 413, "body": "payload too large"},
    "loop_forever": {"tool_calls": [{"name": "Read", "args": {"file_path": "a.txt"}}]},
}


def respond(body, n):
    if n == 0 or (CASE.startswith("status_") and n < CASES[CASE].get("repeat", 1)):
        return CASES[CASE]
    if CASE == "loop_forever":
        return {"tool_calls": [{"name": "Read", "args": {"file_path": "a.txt"}}]}
    return None
