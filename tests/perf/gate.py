#!/usr/bin/env python3
"""Keep Chanto's own startup and per-turn cost from regressing.

Two measurements are taken against the instant mock model in
`tests/chaos/fakemodel.py`, so no real model is involved and the model's own
time is zero:

  startup   one `-p` request: process CPU and peak RSS of a one-shot run.
  turns     40 requests that each read a 40 KB file, the same workload the
            published benchmark uses: CPU per request and peak RSS.

Each one is compared with the same measurement of another binary (in CI, the
pull request's base commit built from its own source) and with a fixed
ceiling in `tests/perf/budgets.json`. A metric fails when either it is over
its ceiling, or it is both outside `tolerance` and significantly slower than
the base by a one-sided Mann-Whitney U test at `alpha`. Requiring both keeps
a lucky run of a few milliseconds from failing a pull request, while still
catching a real regression that is smaller than the ceiling allows.

Runs of the two binaries are interleaved, and the binaries are measured
through the kernel's rusage (`wait4`), not the wall clock, so scheduling and
an idle machine do not count.

    python3 tests/perf/gate.py --head target/release/chanto
    python3 tests/perf/gate.py --head target/release/chanto --base /tmp/base/chanto

Writes a JSON report with `--json`, a markdown table with `--summary-md`, and
the same table to `$GITHUB_STEP_SUMMARY` when that is set. Exits 1 when a gate
fails and 2 when the measurement itself is unusable.
"""
import argparse
import json
import math
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent.parent
MOCK = REPO / "tests/chaos/fakemodel.py"
CONFIG = REPO / "tests/perf/budgets.json"

RSS_METRICS = ("startup_rss_mb", "turn_rss_mb")
LABELS = {
    "startup_cpu_ms": "Startup CPU (ms)",
    "startup_rss_mb": "Startup peak RSS (MB)",
    "turn_cpu_ms": "CPU per request (ms)",
    "turn_rss_mb": "Peak RSS, 40 turns (MB)",
}


class GateError(Exception):
    """The measurement could not be taken; not a regression."""


def mock_start(scenario, log):
    """Start the fake model and return (process, port)."""
    p = subprocess.Popen(
        [sys.executable, str(MOCK), str(scenario), "--log", str(log)],
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
    )
    deadline = time.time() + 15
    while time.time() < deadline:
        line = p.stdout.readline()
        if line.startswith("PORT"):
            return p, line.split()[1]
        if not line and p.poll() is not None:
            break
    p.kill()
    raise GateError("the mock model did not start")


def child_env(home, port):
    env = {
        "PATH": os.environ.get("PATH", "/usr/local/bin:/usr/bin:/bin"),
        "HOME": str(home),
        "CHANTO_HOME": str(home / "chanto"),
        "CHANTO_COMPAT_HOME": str(home / "compat"),
        "CHANTO_PROVIDER": "openai",
        "CHANTO_BASE_URL": f"http://127.0.0.1:{port}/v1",
        "CHANTO_MODEL": "fake",
        "CHANTO_NO_UPDATE_CHECK": "1",
    }
    # The caller's locale, so output width does not change between runs.
    for k in ("LANG", "LC_ALL"):
        if k in os.environ:
            env[k] = os.environ[k]
    env["TZ"] = "UTC"
    return env


def wait_with_rusage(binary, args, env):
    """Run one child to completion and return its rusage and exit code."""
    p = subprocess.Popen(
        [binary, *args],
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    _, status, ru = os.wait4(p.pid, 0)
    # Reaped above, so tell Popen not to wait again.
    p.returncode = os.waitstatus_to_exitcode(status)
    return ru, p.returncode


def peak_rss_mb(ru):
    """`ru_maxrss` is bytes on macOS and kilobytes on Linux."""
    if sys.platform == "darwin":
        return ru.ru_maxrss / (1024 * 1024)
    return ru.ru_maxrss / 1024


def chat_requests(log):
    if not log.exists():
        return 0
    n = 0
    for line in log.read_text().splitlines():
        try:
            if json.loads(line).get("kind") == "chat":
                n += 1
        except ValueError:
            continue
    return n


def write_turn_scenario(work, turns, blob_kb):
    project = work / "project"
    project.mkdir()
    line = "lorem ipsum dolor sit amet consectetur adipiscing elit " * 12 + "\n"
    blob = line * max(1, blob_kb * 1024 // len(line))
    for i in range(turns):
        (project / f"f{i}.txt").write_text(f"file {i}\n{blob}")

    scenario = work / "turns.py"
    scenario.write_text(
        "import json\n"
        "def respond(body, n):\n"
        "    pt = max(1, len(json.dumps(body)) // 4)\n"
        "    u = {'usage': {'prompt_tokens': pt, 'completion_tokens': 5}}\n"
        f"    if n < {turns}:\n"
        "        return {**u,\n"
        "                'tool_calls': [{'name': 'Read',\n"
        "                                'args': {'file_path': f'f{n}.txt'}}],\n"
        "                'finish_reason': 'tool_calls'}\n"
        "    return {**u, 'text': 'done.'}\n"
    )
    return project, scenario


class StartupBench:
    """One long-lived mock, so a one-shot run pays for nothing but Chanto."""

    def __init__(self, work, home):
        self.work = work
        scenario = work / "hello.json"
        scenario.write_text(json.dumps([{"text": "hello there, friend."}]))
        self.mock, port = mock_start(scenario, work / "hello.log")
        self.env = child_env(home, port)

    def run(self, binary):
        """CPU ms and peak RSS MB of one `-p` request."""
        ru, code = wait_with_rusage(
            binary, ["-p", "hello", "--cwd", str(self.work)], self.env
        )
        if code != 0:
            raise GateError(f"{binary} exited {code} on a one-shot run")
        return (ru.ru_utime + ru.ru_stime) * 1000, peak_rss_mb(ru)

    def close(self):
        self.mock.kill()


def turn_run(binary, work, home, project, scenario, turns, tag):
    """CPU per request and peak RSS MB of one session of `turns` requests.

    Each run gets a fresh mock: the scenario counts requests, and the log says
    how many the run actually made.
    """
    log = work / f"turns-{tag}.log"
    mock, port = mock_start(scenario, log)
    try:
        ru, code = wait_with_rusage(
            binary,
            ["-p", "read the files as I direct", "--cwd", str(project)],
            child_env(home, port),
        )
    finally:
        mock.kill()
    if code != 0:
        raise GateError(f"{binary} exited {code} in the {turns}-turn run")
    n = chat_requests(log)
    if n != turns + 1:
        raise GateError(f"{binary} made {n} of {turns + 1} requests in the {turns}-turn run")
    return (ru.ru_utime + ru.ru_stime) * 1000 / n, peak_rss_mb(ru)


def alternating(names, i):
    """Interleave the binaries so a drift in the machine's load or clocks
    reaches both, not just the one measured second."""
    return names if i % 2 == 0 else list(reversed(names))


def aggregate(metric, samples):
    """CPU is a median: one slow rep should not speak for the run. Peak RSS is
    a maximum: the gate exists to catch a blow-up, not an average."""
    return max(samples) if metric in RSS_METRICS else statistics.median(samples)


def average_ranks(values):
    """Ranks with ties sharing their mean rank (1-based)."""
    order = sorted(range(len(values)), key=lambda i: values[i])
    ranks = [0.0] * len(values)
    i = 0
    while i < len(order):
        j = i
        while j + 1 < len(order) and values[order[j + 1]] == values[order[i]]:
            j += 1
        rank = (i + j) / 2 + 1
        for k in range(i, j + 1):
            ranks[order[k]] = rank
        i = j + 1
    return ranks


def mann_whitney_greater(base, head):
    """One-sided p that `head` values are stochastically larger than `base`.

    Normal approximation with tie correction and a continuity correction;
    the sample sizes here (15 startup runs, 10 sessions) are large enough for
    it, and it needs no dependency beyond the standard library.
    """
    n1, n2 = len(base), len(head)
    if n1 < 2 or n2 < 2:
        return 1.0
    values = list(base) + list(head)
    ranks = average_ranks(values)
    u_head = sum(ranks[n1:]) - n2 * (n2 + 1) / 2

    _, counts = zip(*((v, values.count(v)) for v in set(values)))
    ties = sum(c**3 - c for c in counts) / (len(values) * (len(values) - 1))
    sigma = math.sqrt(n1 * n2 / 12 * (len(values) + 1 - ties))
    if sigma == 0:
        return 1.0
    z = (u_head - n1 * n2 / 2 - 0.5) / sigma
    return 0.5 * math.erfc(z / math.sqrt(2))


def judge(metric, base, head, budget, tolerance, alpha, large_change=None):
    """Apply the ceiling and the significance test to one metric."""
    head_value = aggregate(metric, head)
    base_value = aggregate(metric, base) if base else None
    over_budget = budget is not None and head_value > budget
    change = None
    p_value = None
    significant = False
    too_big = False
    if base_value is not None and base_value > 0:
        change = (head_value - base_value) / base_value
        if head_value > base_value * (1 + tolerance):
            p_value = mann_whitney_greater(base, head)
            significant = p_value < alpha
        # A small-sample test can miss a large regression that noise spreads
        # over both groups; a jump this big fails without asking for a p-value.
        too_big = large_change is not None and change > large_change
    if over_budget or significant or too_big:
        verdict = "fail"
    else:
        verdict = "pass"
    return {
        "base": base_value,
        "head": head_value,
        "change": change,
        "budget": budget,
        "p_value": p_value,
        "significant": significant,
        "over_budget": over_budget,
        "too_big": too_big,
        "near_budget": budget is not None and head_value > budget * 0.6,
        "verdict": verdict,
        "base_samples": base,
        "head_samples": head,
    }


def load_config(path):
    cfg = json.loads(Path(path).read_text())
    platform = "darwin" if sys.platform == "darwin" else "linux"
    if platform not in cfg["budgets"]:
        raise GateError(f"{path} has no {platform} ceilings")
    cfg["platform"] = platform
    cfg["ceilings"] = cfg["budgets"][platform]
    return cfg


def run(head, base, config):
    cfg = load_config(config)
    reps = cfg["reps"]
    warmup = cfg.get("warmup", 1)
    turns = cfg["workload"]["turns"]
    blob_kb = cfg["workload"]["blob_kb"]
    work = Path(tempfile.mkdtemp(prefix="chanto-perf-"))
    home = work / "home"
    home.mkdir()

    binaries = [("head", head)]
    if base:
        binaries.append(("base", base))
    names = [name for name, _ in binaries]
    samples = {name: {"startup_cpu_ms": [], "startup_rss_mb": [],
                      "turn_cpu_ms": [], "turn_rss_mb": []} for name in names}
    try:
        # One-shot runs share a mock, so the first few are warm-up only.
        bench = StartupBench(work, home)
        try:
            for i in range(warmup + reps["startup"]):
                for name, binary in alternating(binaries, i):
                    cpu, rss = bench.run(binary)
                    if i >= warmup:
                        samples[name]["startup_cpu_ms"].append(cpu)
                        samples[name]["startup_rss_mb"].append(rss)
        finally:
            bench.close()

        project, scenario = write_turn_scenario(work, turns, blob_kb)
        for i in range(warmup + reps["turns"]):
            for name, binary in alternating(binaries, i):
                cpu, rss = turn_run(
                    binary, work, home, project, scenario, turns, f"{name}-{i}"
                )
                if i >= warmup:
                    samples[name]["turn_cpu_ms"].append(cpu)
                    samples[name]["turn_rss_mb"].append(rss)
            print(f"turn pair {i + 1}/{warmup + reps['turns']}", flush=True)
    finally:
        shutil.rmtree(work, ignore_errors=True)

    metrics = {}
    for metric in LABELS:
        metrics[metric] = judge(
            metric,
            samples.get("base", {}).get(metric),
            samples["head"][metric],
            cfg["ceilings"].get(metric),
            cfg["tolerance"][metric],
            cfg["alpha"],
            cfg["large_change"],
        )
    report = {
        "platform": cfg["platform"],
        "base": str(base) if base else None,
        "head": str(head),
        "workload": cfg["workload"],
        "reps": reps,
        "metrics": metrics,
        "ok": all(m["verdict"] == "pass" for m in metrics.values()),
    }
    return report


def number(value, unit=""):
    if value is None:
        return "—"
    return f"{value:.1f}{unit}" if unit else f"{value:.1f}"


def markdown(report):
    lines = [
        f"### Performance gate ({report['platform']})",
        "",
        f"Workload: one `-p` request, and {report['workload']['turns']} requests "
        f"reading {report['workload']['blob_kb']} KB files. "
        f"Base: `{report['base'] or 'not built (ceilings only)'}`.",
        "",
        "| Metric | Base | Head | Change | Ceiling | Verdict |",
        "| --- | ---: | ---: | ---: | ---: | --- |",
    ]
    for metric, m in report["metrics"].items():
        change = "—" if m["change"] is None else f"{m['change'] * 100:+.1f}%"
        if m["significant"]:
            change += f" (p={m['p_value']:.3f})"
        verdict = m["verdict"]
        if verdict == "fail":
            if m["over_budget"]:
                verdict = "fail (over ceiling)"
            elif m["too_big"]:
                verdict = "fail (much slower)"
            else:
                verdict = "fail (significantly slower)"
        lines.append(
            f"| {LABELS[metric]} | {number(m['base'])} | {number(m['head'])} | "
            f"{change} | {number(m['budget'])} | {verdict} |"
        )
    passing = [LABELS[k] for k, m in report["metrics"].items() if m["verdict"] == "pass"]
    if report["ok"]:
        lines += ["", f"All {len(passing)} metrics are within their ceiling and the base."]
    else:
        lines += ["", "**A metric regressed:** " + ", ".join(
            LABELS[k] for k, m in report["metrics"].items() if m["verdict"] != "pass"
        )]
    near = [
        LABELS[k]
        for k, m in report["metrics"].items()
        if m["verdict"] == "pass" and m.get("near_budget")
    ]
    if near:
        lines += ["", "> Within budget but over 60% of the ceiling, so consider "
                  f"lowering it: {', '.join(near)}."]
    return "\n".join(lines) + "\n"


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--head", required=True, help="binary under test")
    ap.add_argument("--base", help="binary to compare with (the base commit)")
    ap.add_argument("--budgets", default=str(CONFIG), help="ceiling configuration")
    ap.add_argument("--json", help="write the JSON report here")
    ap.add_argument("--summary-md", help="write the markdown table here")
    a = ap.parse_args()

    for path in filter(None, (a.head, a.base)):
        if not Path(path).is_file():
            sys.exit(f"no binary at {path}")
    try:
        report = run(a.head, a.base, a.budgets)
    except GateError as e:
        print(f"perf gate could not measure: {e}", file=sys.stderr)
        return 2

    text = markdown(report)
    print(text)
    if a.json:
        Path(a.json).write_text(json.dumps(report, indent=2) + "\n")
    if a.summary_md:
        Path(a.summary_md).write_text(text)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a") as f:
            f.write(text)
    return 0 if report["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
