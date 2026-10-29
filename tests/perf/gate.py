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
ceiling in `tests/perf/budgets.json`. A metric fails when it is over its
ceiling, when it is both outside `tolerance` and significantly slower than
the base by a one-sided Mann-Whitney U test at `alpha`, or when it is slower
by more than `large_change` and the test at least leans that way (p below
`large_change_p`). Requiring a tolerance keeps a lucky run of a few
milliseconds from failing a pull request; the hard cap catches a real
regression that noise spread over both groups. It only counts a difference
larger than the clock's own step, so a single tick of a coarse timer is not a
regression, and not a median that moved while the two sample sets still
overlap evenly: a few milliseconds of runner jitter on a 30 ms startup.

Runs of the two binaries are interleaved, and the binaries are measured
through the kernel, not the wall clock, so scheduling and an idle machine do
not count: rusage (`wait4`) on Unix, `GetProcessTimes` and
`GetProcessMemoryInfo` on Windows.

    python3 tests/perf/gate.py --head target/release/chanto
    python3 tests/perf/gate.py --head target/release/chanto --base /tmp/base/chanto
    python3 tests/perf/gate.py --head target/release/chanto --device

`--device` adds the same workload pinned to one core, against a small
device's ceilings: a phone or a cheap laptop has no spare cores and little
memory, and nothing above says whether Chanto still runs there.

Writes a JSON report with `--json`, a markdown table with `--summary-md`, and
the same table to `$GITHUB_STEP_SUMMARY` when that is set. Exits 1 when a gate
fails and 2 when the measurement itself is unusable.
"""
import argparse
import collections
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

RSS_METRICS = ("startup_rss_mb", "turn_rss_mb", "device_turn_rss_mb")
LABELS = {
    "startup_cpu_ms": "Startup CPU (ms)",
    "startup_rss_mb": "Startup peak RSS (MB)",
    "turn_cpu_ms": "CPU per request (ms)",
    "turn_rss_mb": "Peak RSS, 40 turns (MB)",
}
# The release binary's size: exact, so one reading each and no test. It fails
# only over its platform's ceiling; a pull request that needs more raises the
# ceiling in budgets.json, where the growth is visible in review. The change
# from the base is reported either way.
SIZE_LABELS = {
    "binary_mb": "Binary size (MB)",
}
# Measured against a ceiling only, with no base to compare, so they are not
# part of the per-platform budget every metric above has.
DEVICE_LABELS = {
    "device_startup_wall_ms": "Startup (ms)",
    "device_turn_wall_ms": "Per request (ms)",
    "device_turn_rss_mb": "Peak RSS (MB)",
}

# The kernels report CPU and peak memory the same way, but not through the
# same call; a measurement is one of these.
Usage = collections.namedtuple("Usage", "cpu_ms rss_mb wall_ms")


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
    # Windows will not start a child without these, and Chanto finds its
    # temporary and configuration directories through them.
    for k in ("SYSTEMROOT", "SYSTEMDRIVE", "WINDIR", "TEMP", "TMP", "PATHEXT",
              "USERPROFILE", "APPDATA", "LOCALAPPDATA"):
        if k in os.environ:
            env[k] = os.environ[k]
    env["TZ"] = "UTC"
    return env


def wait_with_usage(binary, args, env):
    """Run one child to completion and return its usage and exit code."""
    start = time.monotonic()
    p = subprocess.Popen(
        [binary, *args],
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    if sys.platform == "win32":
        code = p.wait()
        # Read after the exit, so the numbers are final; the handle stays
        # readable until Popen closes it.
        cpu_ms, rss_mb = windows_usage(p)
    else:
        _, status, ru = os.wait4(p.pid, 0)
        # Reaped above, so tell Popen not to wait again.
        p.returncode = os.waitstatus_to_exitcode(status)
        code = p.returncode
        cpu_ms = (ru.ru_utime + ru.ru_stime) * 1000
        rss_mb = peak_rss_mb(ru)
    return Usage(cpu_ms, rss_mb, (time.monotonic() - start) * 1000), code


def peak_rss_mb(ru):
    """`ru_maxrss` is bytes on macOS and kilobytes on Linux."""
    if sys.platform == "darwin":
        return ru.ru_maxrss / (1024 * 1024)
    return ru.ru_maxrss / 1024


def windows_usage(p):
    """CPU ms and peak working set MB of a child that has exited.

    Windows has no `wait4`. The kernel counts CPU in 100 ns units in
    `GetProcessTimes` and the peak working set in `GetProcessMemoryInfo`
    (`PROCESS_MEMORY_COUNTERS_EX`), the same numbers the counters report, and
    both stay readable on the handle after the process exits.
    """
    import ctypes

    class FileTime(ctypes.Structure):
        _fields_ = [("low", ctypes.c_uint32), ("high", ctypes.c_uint32)]

    class Counters(ctypes.Structure):
        _fields_ = [
            ("cb", ctypes.c_uint32),
            ("page_faults", ctypes.c_uint32),
            ("peak_working_set", ctypes.c_size_t),
            ("working_set", ctypes.c_size_t),
            ("quota_peak_paged", ctypes.c_size_t),
            ("quota_paged", ctypes.c_size_t),
            ("quota_peak_nonpaged", ctypes.c_size_t),
            ("quota_nonpaged", ctypes.c_size_t),
            ("pagefile", ctypes.c_size_t),
            ("peak_pagefile", ctypes.c_size_t),
            ("private", ctypes.c_size_t),
        ]

    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    psapi = ctypes.WinDLL("psapi", use_last_error=True)
    handle = ctypes.c_void_p(int(p._handle))
    created, exited, kernel, user = FileTime(), FileTime(), FileTime(), FileTime()
    if not kernel32.GetProcessTimes(
        handle,
        ctypes.byref(created),
        ctypes.byref(exited),
        ctypes.byref(kernel),
        ctypes.byref(user),
    ):
        raise GateError(f"GetProcessTimes failed: {ctypes.get_last_error()}")
    counters = Counters()
    counters.cb = ctypes.sizeof(counters)
    if not psapi.GetProcessMemoryInfo(handle, ctypes.byref(counters), counters.cb):
        raise GateError(f"GetProcessMemoryInfo failed: {ctypes.get_last_error()}")

    def ticks(ft):
        """A FILETIME as the number of 100 ns units it holds."""
        return (ft.high << 32) | ft.low

    return (
        (ticks(kernel) + ticks(user)) / 10_000,
        counters.peak_working_set / (1024 * 1024),
    )


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


def pinned(binary, cpus):
    """The command and leading arguments that keep `binary` on `cpus` cores.

    `taskset` runs the binary in place, so the child whose rusage is read is
    still Chanto itself.
    """
    taskset = shutil.which("taskset")
    if not taskset:
        raise GateError("taskset is needed to simulate a small device")
    cores = ",".join(str(i) for i in range(cpus))
    return taskset, ["-c", cores, binary]


class StartupBench:
    """One long-lived mock, so a one-shot run pays for nothing but Chanto."""

    def __init__(self, work, home):
        self.work = work
        scenario = work / "hello.json"
        scenario.write_text(json.dumps([{"text": "hello there, friend."}]))
        self.mock, port = mock_start(scenario, work / "hello.log")
        self.env = child_env(home, port)

    def run(self, binary, pin=None):
        """CPU, peak RSS and wall time of one `-p` request."""
        exe, lead = pin or (binary, [])
        usage, code = wait_with_usage(
            exe, lead + ["-p", "hello", "--cwd", str(self.work)], self.env
        )
        if code != 0:
            raise GateError(f"{binary} exited {code} on a one-shot run")
        return usage

    def close(self):
        self.mock.kill()


def turn_run(binary, work, home, project, scenario, turns, tag, pin=None):
    """CPU, peak RSS and wall time per request of a session of `turns` requests.

    Each run gets a fresh mock: the scenario counts requests, and the log says
    how many the run actually made.
    """
    exe, lead = pin or (binary, [])
    log = work / f"turns-{tag}.log"
    mock, port = mock_start(scenario, log)
    try:
        usage, code = wait_with_usage(
            exe,
            lead + ["-p", "read the files as I direct", "--cwd", str(project)],
            child_env(home, port),
        )
    finally:
        mock.kill()
    if code != 0:
        raise GateError(f"{binary} exited {code} in the {turns}-turn run")
    n = chat_requests(log)
    if n != turns + 1:
        raise GateError(f"{binary} made {n} of {turns + 1} requests in the {turns}-turn run")
    return Usage(usage.cpu_ms / n, usage.rss_mb, usage.wall_ms / n)


def device_measure(binary, work, home, project, scenario, cfg):
    """The same workload the way a small machine sees it: one core, a phone's
    worth of memory, and a wall-clock budget for each request.

    This is not compared with another binary: wall time on a shared runner
    says nothing about the change being measured, only about whether the agent
    stays usable when it cannot lean on spare cores.
    """
    device = cfg["device"]
    pin = pinned(binary, device["cpus"])
    reps = device["reps"]
    turns = cfg["workload"]["turns"]

    bench = StartupBench(work, home)
    try:
        startup_wall = [bench.run(binary, pin).wall_ms for _ in range(reps)]
    finally:
        bench.close()
    runs = [
        turn_run(binary, work, home, project, scenario, turns, f"device-{i}", pin)
        for i in range(reps)
    ]
    return {
        "device_startup_wall_ms": judge(
            "device_startup_wall_ms",
            None,
            startup_wall,
            device["startup_wall_ms"],
            0,
            cfg["alpha"],
        ),
        "device_turn_wall_ms": judge(
            "device_turn_wall_ms",
            None,
            [r.wall_ms for r in runs],
            device["turn_wall_ms"],
            0,
            cfg["alpha"],
        ),
        "device_turn_rss_mb": judge(
            "device_turn_rss_mb",
            None,
            [r.rss_mb for r in runs],
            device["rss_mb"],
            0,
            cfg["alpha"],
        ),
    }


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


def clock_step(samples):
    """The smallest positive gap between two measured values.

    Coarse clocks report a fixed amount at a time: Windows charges CPU in
    15.625 ms ticks, so a one-tick shift of the median is the smallest
    difference the platform can express. Comparing a percentage against that
    step would call one tick a large regression while the two sets overlap
    completely.
    """
    values = sorted(set(samples))
    gaps = [b - a for a, b in zip(values, values[1:]) if b > a]
    return min(gaps) if gaps else 0.0


def judge_size(base, head, budget):
    """The binary size against its ceiling, with the change from the base."""
    change = (head - base) / base if base else None
    over_budget = budget is not None and head > budget
    return {
        "base": base,
        "head": head,
        "change": change,
        "budget": budget,
        "p_value": None,
        "significant": False,
        "over_budget": over_budget,
        "too_big": False,
        # Its ceiling sits close on purpose: flag only the last 5%.
        "near_budget": budget is not None and head > budget * 0.95,
        "verdict": "fail" if over_budget else "pass",
        "base_samples": [base] if base else None,
        "head_samples": [head],
    }


def binary_mb(path):
    return Path(path).stat().st_size / 1e6


def judge(metric, base, head, budget, tolerance, alpha, large_change=None, large_change_p=1.0):
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
        # over both groups; a jump this big fails on a weak p-value too. Not on
        # none at all: a median can move this far by chance on a short metric
        # while the two sets overlap evenly. A difference the clock cannot
        # resolve is never such a jump.
        too_big = (
            large_change is not None
            and change > large_change
            and head_value - base_value > clock_step(base + head)
            and p_value is not None
            and p_value < large_change_p
        )
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
        "near_budget": budget is not None and head_value > budget * 0.75,
        "verdict": verdict,
        "base_samples": base,
        "head_samples": head,
    }


PLATFORM = {"darwin": "darwin", "win32": "windows"}.get(sys.platform, "linux")


def load_config(path):
    cfg = json.loads(Path(path).read_text())
    if PLATFORM not in cfg["budgets"]:
        raise GateError(f"{path} has no {PLATFORM} ceilings")
    cfg["platform"] = PLATFORM
    cfg["ceilings"] = cfg["budgets"][PLATFORM]
    return cfg


def run(head, base, config, device=False):
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
    device_metrics = None
    try:
        # One-shot runs share a mock, so the first few are warm-up only.
        bench = StartupBench(work, home)
        try:
            for i in range(warmup + reps["startup"]):
                for name, binary in alternating(binaries, i):
                    usage = bench.run(binary)
                    if i >= warmup:
                        samples[name]["startup_cpu_ms"].append(usage.cpu_ms)
                        samples[name]["startup_rss_mb"].append(usage.rss_mb)
        finally:
            bench.close()

        project, scenario = write_turn_scenario(work, turns, blob_kb)
        for i in range(warmup + reps["turns"]):
            for name, binary in alternating(binaries, i):
                usage = turn_run(
                    binary, work, home, project, scenario, turns, f"{name}-{i}"
                )
                if i >= warmup:
                    samples[name]["turn_cpu_ms"].append(usage.cpu_ms)
                    samples[name]["turn_rss_mb"].append(usage.rss_mb)
            print(f"turn pair {i + 1}/{warmup + reps['turns']}", flush=True)

        if device:
            device_metrics = device_measure(
                head, work, home, project, scenario, cfg
            )
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
            cfg["large_change_p"],
        )
    metrics["binary_mb"] = judge_size(
        binary_mb(base) if base else None,
        binary_mb(head),
        cfg["ceilings"].get("binary_mb"),
    )
    report = {
        "platform": cfg["platform"],
        "base": str(base) if base else None,
        "head": str(head),
        "workload": cfg["workload"],
        "reps": reps,
        "metrics": metrics,
    }
    if device_metrics is not None:
        report["device"] = {
            "cpus": cfg["device"]["cpus"],
            "reps": cfg["device"]["reps"],
            "metrics": device_metrics,
        }
    report["ok"] = all(
        m["verdict"] == "pass"
        for group in (report["metrics"], report.get("device", {}).get("metrics", {}))
        for m in group.values()
    )
    return report


def label(metric):
    return LABELS.get(metric) or SIZE_LABELS[metric]


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
            f"| {label(metric)} | {number(m['base'])} | {number(m['head'])} | "
            f"{change} | {number(m['budget'])} | {verdict} |"
        )
    passing = [label(k) for k, m in report["metrics"].items() if m["verdict"] == "pass"]
    device = report.get("device", {})
    if report["ok"]:
        lines += ["", f"All {len(passing)} metrics are within their ceiling and the base."]
    else:
        failed = [label(k) for k, m in report["metrics"].items()
                  if m["verdict"] != "pass"]
        failed_device = [DEVICE_LABELS[k] for k, m in device.get("metrics", {}).items()
                         if m["verdict"] != "pass"]
        lines += [""]
        if failed:
            lines += ["**A metric regressed:** " + ", ".join(failed)]
        if failed_device:
            lines += ["**The small device could not keep up:** "
                      + ", ".join(failed_device)]
    near = [
        label(k)
        for k, m in report["metrics"].items()
        if m["verdict"] == "pass" and m.get("near_budget")
    ]
    if near:
        lines += ["", "> Within budget but over 75% of the ceiling, so consider "
                  f"lowering it: {', '.join(near)}."]
    if device:
        lines += device_markdown(device)
    return "\n".join(lines) + "\n"


def device_markdown(device):
    """The small-device measurements, which have no base to compare with."""
    lines = [
        "",
        "### Small device",
        "",
        f"{device['cpus']} core: a ceiling rather than a comparison, because "
        "wall time on a shared runner says nothing about the change. It says "
        "whether the agent still starts, answers and stays in memory when it "
        "has neither spare cores nor a desktop's RAM.",
        "",
        "| Metric | Value | Ceiling | Verdict |",
        "| --- | ---: | ---: | --- |",
    ]
    for metric, m in device["metrics"].items():
        lines.append(
            f"| {DEVICE_LABELS[metric]} | {number(m['head'])} | "
            f"{number(m['budget'])} | {m['verdict']} |"
        )
    return lines


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--head", required=True, help="binary under test")
    ap.add_argument("--base", help="binary to compare with (the base commit)")
    ap.add_argument("--budgets", default=str(CONFIG), help="ceiling configuration")
    ap.add_argument(
        "--device",
        action="store_true",
        help="also run the same workload on one core against a small device's "
        "ceilings (Linux; needs taskset)",
    )
    ap.add_argument("--json", help="write the JSON report here")
    ap.add_argument("--summary-md", help="write the markdown table here")
    a = ap.parse_args()

    for path in filter(None, (a.head, a.base)):
        if not Path(path).is_file():
            sys.exit(f"no binary at {path}")
    try:
        report = run(a.head, a.base, a.budgets, device=a.device)
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
