# Shell output chaos. CASE picks the command.
import os

CASE = os.environ.get("CASE", "flood")
CMDS = {
    "flood": {"command": "yes 0123456789abcdef | head -c 50000000"},
    "binary": {"command": "head -c 3000000 /dev/urandom"},
    "longline": {"command": "head -c 5000000 /dev/zero | tr '\\0' x"},
    "forever": {"command": "yes", "timeout": 3000},
    "stderr_flood": {"command": "yes err 1>&2 | head -c 20000000"},
    "ansi": {"command": "printf '\\033]0;PWNED\\007\\033[2J\\033[31mred\\033[0m\\n'"},
    "bg_many": {"command": "sleep 30", "run_in_background": True},
    "child_daemon": {"command": "(sleep 30 &) ; echo started", "timeout": 10000},
    "kill_self": {"command": "kill -9 $PPID; echo after"},
    "exit_code": {"command": "exit 137"},
    "stdin_read": {"command": "read x; echo got $x", "timeout": 3000},
    "tty": {"command": "tty; stty -a | head -1", "timeout": 3000},
    "cd_away": {"command": "cd / && pwd"},
    "env_huge": {"command": "export X=$(head -c 200000 /dev/zero | tr '\\0' y); echo ${#X}"},
}


def respond(body, n):
    if CASE == "bg_many":
        if n < 20:
            return {"tool_calls": [{"name": "Bash", "args": CMDS["bg_many"]}]}
        if n == 20:
            return {"tool_calls": [{"name": "Bash", "args": {"command": "pwd"}}]}
        return {"text": "done."}
    if n == 0:
        return {"tool_calls": [{"name": "Bash", "args": CMDS[CASE]}]}
    if n == 1 and CASE == "cd_away":
        return {"tool_calls": [{"name": "Bash", "args": {"command": "pwd"}}]}
    return {"text": "done."}
