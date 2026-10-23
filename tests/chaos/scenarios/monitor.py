# Monitor tool chaos. CASE: forever | flood | binary | exit_fail
import os

CMDS = {
    "forever": "while true; do echo tick; sleep 0.2; done",
    "flood": "yes 'flood line with some text' | head -n 2000000",
    "binary": "head -c 200000 /dev/urandom",
    "exit_fail": "echo starting; exit 3",
}
CASE = os.environ.get("CASE", "forever")


def respond(body, n):
    if n == 0:
        return {"tool_calls": [{"name": "Monitor", "args": {"command": CMDS[CASE], "description": "chaos " + CASE}}]}
    return {"text": f"turn {n} ok."}
