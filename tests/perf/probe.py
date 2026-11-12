#!/usr/bin/env python3
"""Compare builds of chanto, and break down where a one-shot start spends CPU.

    python3 tests/perf/probe.py NAME=BINARY [NAME=BINARY ...] [--floor BINARY]

For each build: startup CPU and peak memory of a one-shot run, CPU per request
and peak memory of a 40-turn session (the perf gate's own workload and mock),
and the median CPU spent in each CHANTO_TIMING phase of a start. Builds are
measured in alternating rounds, so a runner that speeds up or slows down
mid-run affects them alike. `--floor` names a trivial program whose startup
CPU is the platform's floor for any process.
"""
import re
import statistics
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import gate  # noqa: E402

ROUNDS = 15
TURNS = 40


def phases(binary, bench):
    """CPU at each CHANTO_TIMING checkpoint of one start, as {label: ms}."""
    env = dict(bench.env, CHANTO_TIMING="1")
    p = subprocess.run(
        [binary, "-p", "hello", "--cwd", str(bench.work)],
        env=env, stdin=subprocess.DEVNULL, capture_output=True, text=True,
    )
    out = {}
    for m in re.finditer(r"\[timing\]\s+[\d.]+ ms\s+(.+?)\s+\(cpu ([\d.]+) ms\)", p.stderr):
        out.setdefault(m.group(1), float(m.group(2)))
    return out


def main():
    args = sys.argv[1:]
    floor = None
    if "--floor" in args:
        i = args.index("--floor")
        floor = args[i + 1]
        del args[i:i + 2]
    builds = dict(a.split("=", 1) for a in args)
    work = Path(tempfile.mkdtemp(prefix="probe-"))
    home = work / "home"
    home.mkdir()
    bench = gate.StartupBench(work, home)
    turn_work = work / "turns"
    turn_work.mkdir()
    project, scenario = gate.write_turn_scenario(turn_work, TURNS, 40)

    start, turn, timing = {}, {}, {}
    floor_cpu = []
    for r in range(ROUNDS):
        for name, binary in builds.items():
            start.setdefault(name, []).append(bench.run(binary))
            turn.setdefault(name, []).append(
                gate.turn_run(binary, turn_work, home, project, scenario, TURNS, f"{name}-{r}"))
            for label, ms in phases(binary, bench).items():
                timing.setdefault(name, {}).setdefault(label, []).append(ms)
        if floor:
            usage, _ = gate.wait_with_usage(floor, [], bench.env)
            floor_cpu.append(usage.cpu_ms)
    bench.close()

    med = statistics.median
    print(f"## {sys.platform}: {ROUNDS} rounds\n")
    if floor_cpu:
        print(f"Floor (an empty Rust program): {med(floor_cpu):.2f} ms CPU\n")
    print("| build | startup CPU | startup peak MB | CPU per request | peak MB, 40 turns |")
    print("| --- | ---: | ---: | ---: | ---: |")
    for name in builds:
        s, t = start[name], turn[name]
        print(f"| {name} | {med(u.cpu_ms for u in s):.2f} | {med(u.rss_mb for u in s):.1f} "
              f"| {med(u.cpu_ms for u in t):.3f} | {med(u.rss_mb for u in t):.1f} |")
    for name in builds:
        print(f"\n### {name}: CPU by startup phase (median ms)\n")
        print("| phase | at | spent |")
        print("| --- | ---: | ---: |")
        prev = 0.0
        for label, values in timing.get(name, {}).items():
            at = med(values)
            print(f"| {label} | {at:.2f} | {at - prev:+.2f} |")
            prev = at


if __name__ == "__main__":
    main()
