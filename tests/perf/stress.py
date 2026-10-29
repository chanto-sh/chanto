#!/usr/bin/env python3
"""Stress Chanto where the per-turn gate doesn't look: a very long session and
a very large tree.

    python3 tests/perf/stress.py --bin target/release/chanto [--json out.json]
                                 [--summary-md out.md] [--config budgets.json]

Both drive one `-p` session against tests/chaos/fakemodel.py, so no model or
network is involved, with the session's own HOME and CHANTO_HOME.

- Long session: `turns` tool calls that each read another file, at a 32k
  context window, so Chanto trims and compacts many times (the fake model
  answers a compaction with a handoff summary). It fails if the session stops
  short, never compacts, goes over its memory ceiling, or gets slower: the
  median gap between model requests in the last quarter against the first.
- Large tree: `files` generated files (a C project with a Makefile). Glob over all of them, Grep for paths
  (newest first, with a limit) and counts, and a large Read. Grep for paths
  must stay within a multiple of `rg -l` on the same tree (files of about
  6 KB, so searching dominates), which a search forced onto one thread
  (ripgrep's --sortr) is not; every step has a
  ceiling, and so does memory.

- Resume: `--continue` the long session. Its request must carry the
  session's history, within a ceiling on time to that request and on memory.

- Cleanup: a session starts clangd (where installed), a background command
  and an MCP server, then ends normally, or on SIGTERM or SIGINT during a
  command. None of them may outlive it, and a signal gives 128 + its number.

- Language servers, where clangd is installed: in the large tree, reading and
  editing a C file must not start one (too many files to start one on a Read,
  and no compilation database for clangd); in a small C project the same
  steps must.

Ceilings are in budgets.json under "stress".
"""
import argparse
import json
import os
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
FAKE = REPO / "tests/chaos/fakemodel.py"
FAKE_MCP = REPO / "tests/chaos/fakemcp.py"
CONFIG = REPO / "tests/perf/budgets.json"

LONG_SCENARIO = r'''
import json, os
TURNS = int(os.environ["STRESS_TURNS"])
FILES = int(os.environ["STRESS_FILES"])
STATE = {"calls": 0, "compactions": 0}
SUMMARY = ("## Goal\nRead every file in order.\n## Progress\nRead files up to the last one named.\n"
           "## Next\nRead the next file.\n" + "Context detail. " * 60)


def last_user(body):
    for m in reversed(body.get("messages") or []):
        if m.get("role") == "user":
            c = m.get("content")
            return c if isinstance(c, str) else json.dumps(c)
    return ""


def respond(body, n):
    usage = {"prompt_tokens": max(1, len(json.dumps(body)) // 4), "completion_tokens": 20}
    if "Write a handoff summary" in last_user(body):
        STATE["compactions"] += 1
        with open(os.environ["STRESS_LOG"], "a") as f:
            f.write("compaction\n")
        return {"text": SUMMARY, "usage": usage}
    i = STATE["calls"]
    STATE["calls"] += 1
    if i >= TURNS:
        return {"text": "finished", "usage": usage}
    if i % 50 == 49:
        call = {"name": "Edit", "args": {"file_path": f"f{i % FILES}.txt", "old_string": "line 1 ",
                                         "new_string": f"line 1 edited {i} "}}
    elif i % 25 == 24:
        call = {"name": "Bash", "args": {"command": f"wc -l f{i % FILES}.txt"}}
    else:
        call = {"name": "Read", "args": {"file_path": f"f{i % FILES}.txt"}}
    return {"tool_calls": [call], "usage": usage}
'''

TREE_SCENARIO = r'''
import json, os
STEPS = [
    ("glob_all", {"name": "Glob", "args": {"pattern": "**/*.c"}}),
    ("grep_paths", {"name": "Grep", "args": {"pattern": "STRESS_NEEDLE", "output_mode": "files_with_matches",
                                             "head_limit": 50}}),
    ("grep_count", {"name": "Grep", "args": {"pattern": "struct widget", "output_mode": "count"}}),
    ("read_large", {"name": "Read", "args": {"file_path": "big/generated.h"}}),
    ("read_c", {"name": "Read", "args": {"file_path": "mod000/unit0.c"}}),
    ("edit_c", {"name": "Edit", "args": {"file_path": "mod000/unit0.c", "old_string": "struct widget w0;",
                                         "new_string": "struct widget w0; /* edited */"}}),
]


def respond(body, n):
    usage = {"prompt_tokens": max(1, len(json.dumps(body)) // 4), "completion_tokens": 20}
    if n < len(STEPS):
        return {"tool_calls": [STEPS[n][1]], "usage": usage}
    return {"text": "done", "usage": usage}
'''
TREE_STEPS = ["glob_all", "grep_paths", "grep_count", "read_large", "read_c", "edit_c"]

# A small C project: reading and editing a file starts its language server.
# The last step keeps the session open long enough for the process sampler
# (every 0.1 s) to see clangd; without it the session can end, and stop
# clangd, between two samples.
SMALL_SCENARIO = r'''
import json
STEPS = [
    {"name": "Read", "args": {"file_path": "main.c"}},
    {"name": "Edit", "args": {"file_path": "main.c", "old_string": "return 0;", "new_string": "return 1;"}},
    {"name": "Bash", "args": {"command": "sleep 2"}},
]


def respond(body, n):
    usage = {"prompt_tokens": max(1, len(json.dumps(body)) // 4), "completion_tokens": 20}
    if n < len(STEPS):
        return {"tool_calls": [STEPS[n]], "usage": usage}
    return {"text": "done", "usage": usage}
'''


# Continuing the long session: the first request must carry its history (the
# compaction summary).
RESUME_SCENARIO = r'''
import json, os


def respond(body, n):
    usage = {"prompt_tokens": max(1, len(json.dumps(body)) // 4), "completion_tokens": 5}
    if n == 0:
        if "Read every file in order" in json.dumps(body):
            with open(os.environ["STRESS_LOG"], "a") as f:
                f.write("history\n")
        # Hold the session for the memory sampler.
        return {"tool_calls": [{"name": "Bash", "args": {"command": "sleep 1"}}], "usage": usage}
    return {"text": "resumed", "usage": usage}
'''

# Start everything Chanto can leave behind, then hold a foreground command
# (STRESS_HOLD seconds; Chanto refuses a bare foreground `sleep` of more than
# a moment) while the session ends or is signalled.
BACKGROUND_MARKER = "sleep 3071"
CLEANUP_SCENARIO = r'''
import json, os
STEPS = [
    {"name": "Read", "args": {"file_path": "main.c"}},
    {"name": "Bash", "args": {"command": "sleep 3071", "run_in_background": True}},
    {"name": "mcp__fake__echo", "args": {"text": "ping"}},
    {"name": "Bash", "args": {"command": "python3 -c 'import time; time.sleep(%s)'" % os.environ["STRESS_HOLD"]}},
]


def respond(body, n):
    usage = {"prompt_tokens": max(1, len(json.dumps(body)) // 4), "completion_tokens": 20}
    if n < len(STEPS):
        return {"tool_calls": [STEPS[n]], "usage": usage}
    return {"text": "done", "usage": usage}
'''


def proc_name(comm):
    """A `ps` command name without its directory, or macOS's parentheses
    around a process that is exiting."""
    name = Path(comm.strip()).name
    return name[1:-1] if name.startswith("(") and name.endswith(")") else name


def tree_usage(root):
    """Resident MB of `root` and everything under it, and their command names
    by pid."""
    out = subprocess.run(["ps", "-A", "-o", "pid=,ppid=,rss=,comm="], capture_output=True, text=True).stdout
    kids, rss, comm = {}, {}, {}
    for line in out.splitlines():
        parts = line.split(None, 3)
        try:
            pid, ppid, r = int(parts[0]), int(parts[1]), int(parts[2])
        except (ValueError, IndexError):
            continue
        kids.setdefault(ppid, []).append(pid)
        rss[pid] = r
        comm[pid] = proc_name(parts[3]) if len(parts) > 3 else ""
    total, names, todo = 0, {}, [root]
    while todo:
        p = todo.pop()
        total += rss.get(p, 0)
        names[p] = comm.get(p, "")
        todo += kids.get(p, [])
    return total / 1024, names


def session(binary, work, home, scenario, num_ctx, env_extra, timeout, args=(), signal_at=None):
    """One `-p` session: (exit code, peak MB, model request times relative to
    start, stderr's end, every process it ran as {pid: name}). `args` go before
    the prompt; `signal_at` = (n, signum) sends Chanto that signal a second
    after the fake model gets its nth request."""
    log = home / "model.jsonl"
    log.write_text("")
    script = home / "scenario.py"
    script.write_text(scenario)
    mock_env = dict(os.environ, **env_extra)
    mock = subprocess.Popen([sys.executable, str(FAKE), str(script), "--log", str(log)],
                            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, env=mock_env)
    port = None
    t0 = time.time()
    while time.time() - t0 < 10 and port is None:
        line = mock.stdout.readline()
        if line.startswith("PORT"):
            port = line.split()[1]
    if port is None:
        mock.kill()
        raise SystemExit("the fake model did not start")
    (home / "home").mkdir(exist_ok=True)
    chanto_home = home / "chanto"
    chanto_home.mkdir(exist_ok=True)
    (chanto_home / "config.toml").write_text("max_turns = 100000\n")
    env = dict(os.environ, HOME=str(home / "home"), CHANTO_HOME=str(chanto_home), CHANTO_NO_PRESENCE="1",
               PWD=str(work), **env_extra)
    cmd = [str(binary), "-p", "--provider", "openai", "--base-url", f"http://127.0.0.1:{port}/v1",
           "--model", "fake", "--num-ctx", str(num_ctx), "--mode", "bypassPermissions",
           "--output-format", "json", *args, "Work through the task."]
    start = time.time()
    p = subprocess.Popen(cmd, cwd=work, env=env, stdin=subprocess.DEVNULL,
                         stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    peak = [0.0]
    seen = {}

    def poll():
        signal_due = None
        while p.poll() is None:
            mb, names = tree_usage(p.pid)
            peak[0] = max(peak[0], mb)
            seen.update(names)
            if signal_at and signal_due is None and len(log.read_text().splitlines()) >= signal_at[0]:
                signal_due = time.time() + 1
            if signal_due and time.time() >= signal_due:
                p.send_signal(signal_at[1])
                signal_due = float("inf")
            time.sleep(0.1)

    th = threading.Thread(target=poll, daemon=True)
    th.start()
    try:
        _, err = p.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        p.kill()
        _, err = p.communicate()
    th.join(timeout=1)
    mock.kill()
    times = [json.loads(l)["t"] - start for l in log.read_text().splitlines() if l.strip()]
    return p.returncode, peak[0], times, err.decode(errors="replace")[-800:], seen


def long_session(binary, cfg, scratch):
    turns, files = cfg["turns"], cfg["files"]
    work = scratch / "long"
    work.mkdir()
    for i in range(files):
        (work / f"f{i}.txt").write_text("".join(f"line {j} of file {i}: lorem ipsum dolor sit amet\n"
                                                for j in range(1, 60)))
    subprocess.run(["git", "init", "-q"], cwd=work)
    home = scratch / "long-home"
    home.mkdir()
    marks = home / "compactions.log"
    marks.write_text("")
    code, peak, times, err, _ = session(binary, work, home, LONG_SCENARIO, 32768,
                                     {"STRESS_TURNS": str(turns), "STRESS_FILES": str(files),
                                      "STRESS_LOG": str(marks)}, cfg["timeout_s"])
    compactions = len(marks.read_text().splitlines())
    gaps = [b - a for a, b in zip(times, times[1:])]
    q = max(1, len(gaps) // 4)
    first, last = statistics.median(gaps[:q] or [0]) * 1000, statistics.median(gaps[-q:] or [0]) * 1000
    tool_calls = len(times) - compactions - 1
    checks = {
        "every turn ran": (tool_calls >= turns, f"{tool_calls} of {turns}"),
        "it compacted": (compactions >= 1, f"{compactions} compactions"),
        "peak memory": (peak <= cfg["rss_mb"], f"{peak:.1f} MB (ceiling {cfg['rss_mb']})"),
        "no slowdown": (last <= first * cfg["drift"] + 2, f"median gap {first:.1f} ms first quarter, {last:.1f} ms last"),
    }
    return {"exit": code, "stderr": err if code else "", "checks": checks}


def large_tree(binary, cfg, scratch):
    n = cfg["files"]
    work = scratch / "tree"
    filler = "".join(f"static int helper_{j}(int x) {{ return x * {j} + 1; }}\n" for j in range(120))
    for i in range(n):
        d = work / f"mod{i // 200:03d}"
        if i % 200 == 0:
            d.mkdir(parents=True)
        needle = "STRESS_NEEDLE\n" if i % 7 == 0 else ""
        # About 6 KB each, like real source: searching the content, not
        # walking directories, is most of the time, as in a real repository.
        (d / f"unit{i}.c").write_text(f"/* unit {i} */\nstruct widget w{i};\n" + filler + needle
                                      + f"int f{i}(void) {{ return {i}; }}\n")
    # The marker that makes Chanto consider clangd for these files.
    (work / "Makefile").write_text("all:\n")
    (work / "big").mkdir()
    (work / "big/generated.h").write_text("".join(f"#define REG_{i:06d} 0x{i:08x}\n" for i in range(200_000)))
    subprocess.run(["git", "init", "-q"], cwd=work)
    rg = shutil.which("rg")
    baseline = None
    if rg:
        t = time.time()
        subprocess.run([rg, "-l", "STRESS_NEEDLE", "."], cwd=work, stdout=subprocess.DEVNULL)
        baseline = time.time() - t
    home = scratch / "tree-home"
    home.mkdir()
    code, peak, times, err, seen = session(binary, work, home, TREE_SCENARIO, 1_000_000, {}, cfg["timeout_s"])
    steps = {name: times[i + 1] - times[i] for i, name in enumerate(TREE_STEPS) if i + 1 < len(times)}
    checks = {
        "every step ran": (len(steps) == len(TREE_STEPS), f"{len(steps)} of {len(TREE_STEPS)}"),
        "peak memory": (peak <= cfg["rss_mb"], f"{peak:.1f} MB (ceiling {cfg['rss_mb']})"),
    }
    for name, secs in steps.items():
        ceiling = cfg["step_s"][name]
        checks[f"{name} time"] = (secs <= ceiling, f"{secs:.2f} s (ceiling {ceiling})")
    if baseline is not None and "grep_paths" in steps:
        limit = baseline * cfg["grep_vs_rg"] + 0.3
        checks["grep_paths vs rg -l"] = (steps["grep_paths"] <= limit,
                                         f"{steps['grep_paths']:.2f} s vs rg -l {baseline:.2f} s (limit {limit:.2f})")
    if shutil.which("clangd"):
        # Too large to start a language server on a Read, and no compilation
        # database for clangd: none may run.
        ran = "clangd" in seen.values()
        checks["no language server"] = (not ran, "clangd ran" if ran else "none started")
    return {"exit": code, "stderr": err if code else "", "checks": checks}


def small_project(binary, cfg, scratch):
    """The control for the large tree: a small C project does get clangd."""
    work = scratch / "small"
    work.mkdir()
    (work / "Makefile").write_text("all: main\n")
    (work / "main.c").write_text("#include <stdio.h>\nint main(void) {\n    puts(\"hi\");\n    return 0;\n}\n")
    subprocess.run(["git", "init", "-q"], cwd=work)
    home = scratch / "small-home"
    home.mkdir()
    code, _, _, err, seen = session(binary, work, home, SMALL_SCENARIO, 200_000, {}, cfg["timeout_s"])
    ran = "clangd" in seen.values()
    checks = {"language server started": (ran, "clangd ran" if ran else "none started")}
    return {"exit": code, "stderr": err if code else "", "checks": checks}


def resume(binary, cfg, scratch):
    """Continue the long session: load its whole history, then one turn."""
    work, home = scratch / "long", scratch / "long-home"
    marks = home / "history.log"
    marks.write_text("")
    code, peak, times, err, _ = session(binary, work, home, RESUME_SCENARIO, 32768,
                                        {"STRESS_LOG": str(marks)}, cfg["timeout_s"], args=["--continue"])
    first = times[0] if times else None
    checks = {
        "history loaded": ("history" in marks.read_text(),
                           "the request carries the session" if "history" in marks.read_text() else "a fresh session"),
        "first request": (first is not None and first <= cfg["first_request_s"],
                          f"{first:.2f} s (ceiling {cfg['first_request_s']})" if first is not None else "none"),
        "peak memory": (peak <= cfg["rss_mb"], f"{peak:.1f} MB (ceiling {cfg['rss_mb']})"),
    }
    return {"exit": code, "stderr": err if code else "", "checks": checks}


def alive(pid, name):
    """`pid` still runs as `name` (not a reused pid)."""
    out = subprocess.run(["ps", "-p", str(pid), "-o", "comm="], capture_output=True, text=True).stdout.strip()
    return bool(out) and proc_name(out) == name


def marked(marker):
    """Pids of processes whose command line contains `marker`."""
    out = subprocess.run(["ps", "-A", "-o", "pid=,args="], capture_output=True, text=True).stdout
    return [int(l.split(None, 1)[0]) for l in out.splitlines() if marker in l]


def cleanup(binary, cfg, scratch, how):
    """A session that starts clangd, a background command and an MCP server,
    then ends: normally, or on SIGTERM or SIGINT mid-command. Nothing it
    started may outlive it."""
    work = scratch / f"cleanup-{how}"
    work.mkdir()
    (work / "Makefile").write_text("all: main\n")
    (work / "main.c").write_text("int main(void) {\n    return 0;\n}\n")
    subprocess.run(["git", "init", "-q"], cwd=work)
    home = scratch / f"cleanup-{how}-home"
    home.mkdir()
    mcp_ran = home / "mcp-ran"
    mcp = home / "mcp.json"
    mcp.write_text(json.dumps({"mcpServers": {"fake": {"command": sys.executable,
                                                       "args": [str(FAKE_MCP), "ok", str(mcp_ran)]}}}))
    signum = {"exit": None, "SIGTERM": signal.SIGTERM, "SIGINT": signal.SIGINT}[how]
    code, _, _, err, seen = session(binary, work, home, CLEANUP_SCENARIO, 200_000,
                                    {"STRESS_HOLD": "2" if signum is None else "60"}, cfg["timeout_s"],
                                    args=["--mcp-config", str(mcp)],
                                    signal_at=(4, signum) if signum else None)
    # Give children Chanto signalled a moment to go.
    deadline = time.time() + cfg["grace_s"]
    while True:
        left = sorted({name for pid, name in seen.items() if alive(pid, name)}
                      | ({"sleep (background)"} if marked(BACKGROUND_MARKER) else set()))
        if not left or time.time() >= deadline:
            break
        time.sleep(0.2)
    for pid, name in seen.items():
        if alive(pid, name):
            os.kill(pid, signal.SIGKILL)
    for pid in marked(BACKGROUND_MARKER):
        os.kill(pid, signal.SIGKILL)
    started = ["MCP server" if mcp_ran.exists() else None, "background command" if "sleep" in seen.values() else None]
    if shutil.which("clangd"):
        started.append("clangd" if "clangd" in seen.values() else None)
    want = {"exit": 0, "SIGTERM": 128 + signal.SIGTERM, "SIGINT": 128 + signal.SIGINT}[how]
    checks = {
        "children started": (None not in started, ", ".join(x for x in started if x) or "none"),
        "nothing left running": (not left, ", ".join(left) or "none"),
    }
    if signum:
        checks["exit code"] = (code == want, f"{code} (expected {want})")
    return {"exit": 0 if code == want else code, "stderr": err if code != want else "", "checks": checks}


def markdown(report):
    lines = ["### Stress", "", "| Check | Result | Verdict |", "| --- | --- | --- |"]
    for part, r in report["parts"].items():
        if r["exit"] != 0:
            lines.append(f"| {part}: session | exited {r['exit']} | fail |")
        for name, (ok, detail) in r["checks"].items():
            lines.append(f"| {part}: {name} | {detail} | {'pass' if ok else 'fail'} |")
    return "\n".join(lines) + "\n"


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--bin", required=True)
    ap.add_argument("--config", default=str(CONFIG))
    ap.add_argument("--json")
    ap.add_argument("--summary-md")
    a = ap.parse_args()
    cfg = json.loads(Path(a.config).read_text())["stress"]
    binary = Path(a.bin).resolve()
    scratch = Path(tempfile.mkdtemp(prefix="chanto-stress-"))
    try:
        parts = {
            "long session": long_session(binary, cfg["long_session"], scratch),
            "resume": resume(binary, cfg["resume"], scratch),
            "large tree": large_tree(binary, cfg["large_tree"], scratch),
        }
        if shutil.which("clangd"):
            parts["small C project"] = small_project(binary, cfg["large_tree"], scratch)
        else:
            print("clangd isn't installed: the language server checks are skipped", file=sys.stderr)
        for how in ("exit", "SIGTERM", "SIGINT"):
            parts[f"cleanup on {how}"] = cleanup(binary, cfg["cleanup"], scratch, how)
    finally:
        shutil.rmtree(scratch, ignore_errors=True)
    report = {"parts": parts, "ok": all(r["exit"] == 0 and all(ok for ok, _ in r["checks"].values())
                                        for r in parts.values())}
    md = markdown(report)
    print(md)
    for part, r in parts.items():
        if r["stderr"]:
            print(f"{part} stderr:\n{r['stderr']}", file=sys.stderr)
    if a.json:
        Path(a.json).write_text(json.dumps(report, indent=1))
    if a.summary_md:
        Path(a.summary_md).write_text(md)
    sys.exit(0 if report["ok"] else 1)


if __name__ == "__main__":
    main()
