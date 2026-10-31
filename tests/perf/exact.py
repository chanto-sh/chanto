#!/usr/bin/env python3
"""Costs that barely vary from run to run, for what timing can't see.

  instructions  what valgrind's callgrind counts for a one-request run and for
                the 40-turn workload (Linux). CPU time moves by up to 2.6x
                between runners; this doesn't depend on the runner at all, so
                it can catch a change of a percent or so, and slow creep.
  prompt bytes  the size of the first request's system prompt and tool
                definitions, which every user pays for on every request.

Compared with another binary (in CI, the base commit's) and with the ceilings
under `exact` in tests/perf/budgets.json.

    python3 tests/perf/exact.py --head target/release/chanto [--base OLD]
"""
import argparse
import json
import os
from pathlib import Path
import re
import statistics
import subprocess
import sys
import tempfile

sys.path.insert(0, str(Path(__file__).parent))
import gate  # noqa: E402

BUDGETS = Path(__file__).with_name("budgets.json")


def collected(stderr):
    """The instruction count in callgrind's report."""
    m = re.search(r"Collected\s*:\s*(\d+)", stderr)
    if not m:
        raise gate.GateError("callgrind reported no count:\n" + stderr[-800:])
    return int(m[1])


def instructions(binary, args, env, cwd):
    p = subprocess.run(
        ["valgrind", "--tool=callgrind", "--callgrind-out-file=/dev/null", "--fair-sched=yes",
         "--", binary, *args],
        env=env, cwd=cwd, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE, text=True, timeout=600,
    )
    if p.returncode != 0:
        raise gate.GateError(f"{binary} exited {p.returncode} under callgrind:\n{p.stderr[-800:]}")
    return collected(p.stderr)


def prompt_bytes(body):
    """Bytes of the system messages and tools in a chat request."""
    system = sum(len(json.dumps(m)) for m in body["messages"] if m["role"] == "system")
    return system + len(json.dumps(body.get("tools", [])))


def hello_scenario(work):
    """One reply, keeping the request it answered in `request.json`."""
    scenario = work / "hello.py"
    scenario.write_text(
        "import json\n"
        "def respond(body, n):\n"
        f"    open({str(work / 'request.json')!r}, 'w').write(json.dumps(body))\n"
        "    return {'text': 'hello there, friend.'}\n"
    )
    return scenario


def measure(binary, work, reps, turns, blob_kb):
    """Median instruction counts of each workload, and the prompt's size."""
    home = work / "home"
    home.mkdir(exist_ok=True)
    hello = hello_scenario(work)
    project, scenario = gate.write_turn_scenario(work, turns, blob_kb)
    out = {"startup": [], "turns": []}
    for _ in range(reps):
        for name, scene, prompt, cwd in (
            ("startup", hello, "hello", work),
            ("turns", scenario, "read the files as I direct", project),
        ):
            log = work / f"{name}.log"
            log.write_text("")
            mock, port = gate.mock_start(scene, log)
            try:
                out[name].append(
                    instructions(binary, ["-p", prompt, "--cwd", str(cwd)], gate.child_env(home, port), cwd)
                )
            finally:
                mock.kill()
    return {
        "startup_instructions": statistics.median(out["startup"]),
        "startup_spread": spread(out["startup"]),
        "turn_instructions": statistics.median(out["turns"]),
        "turn_spread": spread(out["turns"]),
        "prompt_bytes": prompt_bytes(json.loads((work / "request.json").read_text())),
    }


def spread(samples):
    """How far apart the runs of one binary were, as a fraction."""
    return (max(samples) - min(samples)) / min(samples)


def verdicts(head, base, cfg):
    """(metric, head, base, change, ceiling, failed) for each metric."""
    rows = []
    for metric in ("startup_instructions", "turn_instructions", "prompt_bytes"):
        value = head[metric]
        was = base[metric] if base else None
        change = (value - was) / was if was else None
        ceiling = cfg["ceilings"].get(metric)
        failed = (ceiling is not None and value > ceiling) or (
            change is not None and change > cfg["tolerance"][metric]
        )
        rows.append((metric, value, was, change, ceiling, failed))
    return rows


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--head", required=True)
    parser.add_argument("--base")
    args = parser.parse_args()
    all_cfg = json.loads(BUDGETS.read_text())
    cfg = all_cfg["exact"]
    workload = all_cfg["workload"]
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp).resolve()
        (tmp / "head").mkdir()
        head = measure(str(Path(args.head).resolve()), tmp / "head", cfg["reps"],
                       workload["turns"], workload["blob_kb"])
        base = None
        if args.base:
            (tmp / "base").mkdir()
            base = measure(str(Path(args.base).resolve()), tmp / "base", cfg["reps"],
                           workload["turns"], workload["blob_kb"])
    rows = verdicts(head, base, cfg)
    lines = ["### Exact costs", "| metric | base | head | change | ceiling | |", "|---|--:|--:|--:|--:|---|"]
    for metric, value, was, change, ceiling, failed in rows:
        lines.append(
            f"| {metric} | {was or '—'} | {value} | "
            f"{f'{change:+.2%}' if change is not None else '—'} | {ceiling or '—'} | "
            f"{'fail' if failed else 'pass'} |"
        )
    lines.append(f"Spread between runs: startup {head['startup_spread']:.3%}, turns {head['turn_spread']:.3%}")
    report = "\n".join(lines)
    print(report)
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as f:
            f.write(report + "\n")
    return 1 if any(r[5] for r in rows) else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except gate.GateError as e:
        print(f"::error::{e}")
        sys.exit(2)
