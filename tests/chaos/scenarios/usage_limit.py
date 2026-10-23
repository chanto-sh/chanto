# A plan usage limit mid-turn. CASE: soon (resets in 3 s) | far (9 days) |
# garbage (unparseable reset) | quota (billing quota, never resets)
import json, os

CASE = os.environ.get("CASE", "soon")
ERR = {
    "soon": {"type": "usage_limit_reached", "message": "You've hit your usage limit", "resets_in_seconds": 3},
    "far": {"type": "usage_limit_reached", "message": "You've hit your usage limit", "resets_in_seconds": 9 * 86400},
    "garbage": {"type": "usage_limit_reached", "message": "limit", "resets_at": "tomorrow-ish"},
    "huge": {"type": "usage_limit_reached", "message": "limit", "resets_at": 1e300},
    "quota": {"type": "insufficient_quota", "message": "You exceeded your current quota"},
}


def respond(body, n):
    if n == 0:
        return {"tool_calls": [{"name": "Read", "args": {"file_path": "a.txt"}}]}
    if n == 1:
        return {"status": 429, "body": json.dumps({"error": ERR[CASE]})}
    return {"text": "continued after the limit."}
