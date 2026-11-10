#!/usr/bin/env python3
"""The action's `quality` mode: `chanto quality check --checks` on CI.

Runs the repository's quality profile (.chanto/quality.toml, written by
`chanto prime`) against what the event changes: a pull request's commits,
a push's commits, or the whole tree for a manual or scheduled run. No model
and no API key are involved, so it also works for pull requests from forks.
Each finding becomes an error annotation on its line, and the report goes
to the job summary.

The profile's checks are the repository's own commands (tests, linters), so
this mode refuses events that run with secrets on behalf of untrusted code
(pull_request_target, comments).
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
from pathlib import Path
from typing import Callable

# Events whose checkout is the code being checked and that hand it no
# secrets it shouldn't have.
EVENTS = {"pull_request", "push", "merge_group", "workflow_dispatch", "schedule"}
ZERO = "0" * 40
PROFILE = ".chanto/quality.toml"


def plan(event_name: str, event: dict) -> tuple[list[str] | None, str]:
    """The `chanto quality check` arguments for this event, or None and why not."""
    if event_name not in EVENTS:
        return None, (
            f"quality mode doesn't run on {event_name}: it runs the repository's own checks, "
            "so use pull_request, push, merge_group, workflow_dispatch or schedule"
        )
    if event_name == "pull_request":
        base = (event.get("pull_request") or {}).get("base", {}).get("sha", "")
        if base:
            return ["--base", base, "--head", "HEAD"], f"the pull request's changes since {base[:7]}"
    if event_name == "merge_group":
        base = (event.get("merge_group") or {}).get("base_sha", "")
        if base:
            return ["--base", base, "--head", "HEAD"], f"the merge group's changes since {base[:7]}"
    if event_name == "push":
        before = event.get("before") or ""
        if before and before != ZERO:
            return ["--base", before, "--head", "HEAD"], f"the pushed commits since {before[:7]}"
    return ["--all"], "every file"


VIOLATION = re.compile(r"^  (?P<file>[^:\s][^:]*):(?P<line>\d+): (?P<text>.*)$")
RULE = re.compile(r"^✗ (?P<rule>\S+) \((?P<title>.*)\)$")


def escape(s: str, prop: bool = False) -> str:
    s = s.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")
    if prop:
        s = s.replace(":", "%3A").replace(",", "%2C")
    return s


def annotations(output: str) -> list[str]:
    """`::error` workflow commands for each finding in a report."""
    out, rule, title = [], "", ""
    for line in output.splitlines():
        m = RULE.match(line)
        if m:
            rule, title = m["rule"], m["title"]
            continue
        m = VIOLATION.match(line)
        if m and rule:
            out.append(
                f"::error file={escape(m['file'], True)},line={m['line']},"
                f"title={escape(f'{rule}: {title}', True)}::{escape(m['text'])}"
            )
    return out


def supports_quality(help_text: str) -> bool:
    return "quality check" in help_text


def summary(reason: str, output: str, ok: bool) -> str:
    head = "✅ Quality check passed" if ok else "❌ Quality check failed"
    body = output.strip() or "(no output)"
    return f"### {head}\n\nChecked {reason}.\n\n```\n{body}\n```\n"


def trust(home: Path, workspace: Path) -> None:
    """Mark the checkout trusted in this run's own Chanto home, so the
    profile's checks run (it's the repository CI is already testing)."""
    home.mkdir(parents=True, exist_ok=True)
    (home / "trusted.json").write_text(json.dumps([str(workspace.resolve())]))


def ensure_commit(sha: str, runner: Callable[..., subprocess.CompletedProcess]) -> bool:
    ok = lambda *a: runner(["git", *a], capture_output=True, text=True).returncode == 0
    return ok("cat-file", "-e", f"{sha}^{{commit}}") or (
        ok("fetch", "--no-tags", "--depth=1", "origin", sha) and ok("cat-file", "-e", f"{sha}^{{commit}}")
    )


def main(env: dict[str, str] | None = None, runner=subprocess.run) -> int:
    env = dict(os.environ if env is None else env)
    workspace = Path(env.get("GITHUB_WORKSPACE") or ".")
    event_name = env.get("GITHUB_EVENT_NAME", "")
    event: dict = {}
    if env.get("GITHUB_EVENT_PATH"):
        try:
            event = json.loads(Path(env["GITHUB_EVENT_PATH"]).read_text())
        except (OSError, ValueError):
            event = {}
    args, reason = plan(event_name, event)
    if args is None:
        print(f"::error::{reason}")
        return 1
    if not (workspace / PROFILE).exists():
        print(f"::error::No {PROFILE} in this repository. Run `chanto prime` locally and commit the file.")
        return 1
    help_text = runner(["chanto", "--help"], capture_output=True, text=True).stdout
    if not supports_quality(help_text):
        print("::error::This Chanto release has no `chanto quality`; set `version` to a newer release or `latest`.")
        return 1
    if "--base" in args and not ensure_commit(args[1], runner):
        print(f"::error::Can't find commit {args[1]}; check out with `fetch-depth: 0`.")
        return 1
    trust(Path(env.get("CHANTO_HOME") or Path(env.get("RUNNER_TEMP", "/tmp")) / "chanto-home"), workspace)
    cmd = ["chanto", "quality", "check", *args]
    if env.get("INPUT_QUALITY_CHECKS", "true").lower() != "false":
        cmd.append("--checks")
    print(f"Checking {reason}: {' '.join(cmd)}", flush=True)
    r = runner(cmd, capture_output=True, text=True, cwd=workspace, env=env)
    output = (r.stdout or "") + (r.stderr or "")
    print(output, end="" if output.endswith("\n") else "\n")
    for a in annotations(output):
        print(a)
    if env.get("GITHUB_STEP_SUMMARY"):
        with open(env["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as f:
            f.write(summary(reason, output, r.returncode == 0))
    return 0 if r.returncode == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
