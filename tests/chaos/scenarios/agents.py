# Subagent and context-accounting chaos. CASE:
#   recurse   every call (main or subagent) spawns another Task
#   fanout    one response with 30 Task calls
#   bogus_usage  usage numbers far past the context window on every reply
#   neg_usage    negative / non-numeric usage
import os

CASE = os.environ.get("CASE", "recurse")


def respond(body, n):
    msgs = body.get("messages") or []
    sysmsg = msgs[0].get("content", "") if msgs else ""
    if CASE == "recurse":
        if n < 60:
            return {"tool_calls": [{"name": "Task", "args": {"description": f"level {n}", "prompt": "Spawn another Task to go deeper.", "subagent_type": "general-purpose"}}]}
        return {"text": "bottom"}
    if CASE == "fanout":
        if n == 0:
            return {"tool_calls": [{"name": "Task", "args": {"description": f"job {i}", "prompt": f"say {i}", "subagent_type": "general-purpose"}} for i in range(30)]}
        return {"text": "ok"}
    if CASE == "bogus_usage":
        u = {"prompt_tokens": 10**12, "completion_tokens": 10**12}
        if n < 4:
            return {"tool_calls": [{"name": "Read", "args": {"file_path": "a.txt"}}], "usage": u}
        return {"text": "done", "usage": u}
    if CASE == "neg_usage":
        if n < 2:
            return {"raw": 'data: {"choices":[{"delta":{"content":"hi"},"finish_reason":"stop"}],"usage":{"prompt_tokens":-5,"completion_tokens":"lots","prompt_tokens_details":{"cached_tokens":1e300}}}\n\ndata: [DONE]\n\n'}
        return {"text": "done"}
    return {"text": "?"}
