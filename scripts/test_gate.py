#!/usr/bin/env python3
"""Checks that a change comes with tests.

    python3 scripts/test_gate.py require --base SHA --head SHA
        fail a `feat` or `fix` pull request that changes code without touching
        a test, unless its description has a `Tests: none (reason)` line; the
        title and description come from PR_TITLE and PR_BODY (CI)
    python3 scripts/test_gate.py coverage [--base origin/main] [--lcov FILE]
        how much of the Rust lines changed since `base` the tests run, with the
        ones they miss: Codecov's patch check (codecov.yml), before pushing.
        Runs the tests under cargo-llvm-cov unless given a report (local)
"""

import argparse
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parent.parent
# Conventional Commit types that change behaviour.
NEEDS_TESTS = ("feat", "fix")
PATCH_TARGET = 80.0
CODE = re.compile(r"\.(rs|py|ts|js|mjs|kt|sh|ps1)$")
TEST_PATH = re.compile(
    r"(^|/)(tests?|fixtures?)/|(^|/)tests?\.rs$|_tests?\.(rs|py)$|(^|/)test_[^/]*$|\.test\.[a-z]+$"
)
# Code that nothing tests by design, or that isn't the product.
NOT_PRODUCT = re.compile(r"^(\.github|fuzz|benches|examples|scripts|packaging)/")
INLINE_TEST = re.compile(r"^\+\s*#\[(tokio::)?test\b", re.M)


def is_test(path: str) -> bool:
    return bool(TEST_PATH.search(path))


def is_code(path: str) -> bool:
    return bool(CODE.search(path)) and not is_test(path) and not NOT_PRODUCT.match(path)


def require_tests(title: str, body: str, changed: list[str], diff: str) -> str | None:
    """Why a pull request needs a test it doesn't have, or None.

    `changed` is the paths it adds or modifies and `diff` its Rust diff, for
    tests written next to the code. Any test counts: this is a reminder with a
    written way out, and `mutants.yml` is what asks whether a test checks much.
    """
    m = re.match(r"([a-z]+)(\([^)]*\))?!?:", title.strip())
    if not m or m.group(1) not in NEEDS_TESTS:
        return None
    if not any(is_code(p) for p in changed):
        return None
    if any(is_test(p) for p in changed) or INLINE_TEST.search(diff):
        return None
    text = re.sub(r"<!--.*?-->", "", body or "", flags=re.S)
    if re.search(r"^\s*tests:\s*none\b[\s:(—-]*\w", text, re.I | re.M):
        return None
    return (
        f"a `{m.group(1)}` pull request changes behaviour: add or update a test, or, "
        "if none can check it, add a line `Tests: none (<why>)` to the description"
    )


def changed_lines(diff: str) -> dict[str, set[int]]:
    """The lines a `git diff -U0` adds, by file."""
    lines: dict[str, set[int]] = {}
    path = None
    for line in diff.splitlines():
        if line.startswith("+++ "):
            path = line[6:] if line.startswith("+++ b/") else None
        elif path and (m := re.match(r"@@ -\S+ \+(\d+)(?:,(\d+))? @@", line)):
            start, count = int(m[1]), int(m[2] or 1)
            lines.setdefault(path, set()).update(range(start, start + count))
    return lines


def covered_lines(lcov: str, root: Path) -> dict[str, dict[int, int]]:
    """Hit counts of the executable lines in an lcov report, by relative path."""
    hits: dict[str, dict[int, int]] = {}
    current: dict[int, int] = {}
    for line in lcov.splitlines():
        if line.startswith("SF:"):
            path = Path(line[3:])
            name = str(path.relative_to(root)) if path.is_relative_to(root) else line[3:]
            current = hits.setdefault(name, {})
        elif line.startswith("DA:"):
            number, count = line[3:].split(",")[:2]
            current[int(number)] = current.get(int(number), 0) + int(count)
    return hits


def patch_report(
    changed: dict[str, set[int]], hits: dict[str, dict[int, int]]
) -> tuple[int, int, dict[str, list[int]]]:
    """(lines run, executable lines changed, the missed lines by file)."""
    run = total = 0
    missed: dict[str, list[int]] = {}
    for path, lines in sorted(changed.items()):
        if not is_code(path):
            continue
        counts = hits.get(path, {})
        for number in sorted(lines):
            if number not in counts:
                continue  # Comments, blank lines, code the report doesn't reach.
            total += 1
            if counts[number]:
                run += 1
            else:
                missed.setdefault(path, []).append(number)
    return run, total, missed


def git(*args: str) -> str:
    return subprocess.run(
        ["git", *args], cwd=ROOT, capture_output=True, text=True, check=True
    ).stdout


def coverage(base: str, report: str | None) -> int:
    if report:
        lcov = Path(report).read_text()
    else:
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "lcov.info"
            # The same run as CI's coverage job; a test that fails on this
            # machine shouldn't hide the report.
            subprocess.run(
                ["cargo", "llvm-cov", "--locked", "--quiet", "--ignore-run-fail",
                 "-p", "chanto", "-p", "chanto-core", "--lcov", "--output-path", str(out)],
                cwd=ROOT,
            )
            lcov = out.read_text()
    changed = changed_lines(git("diff", "-U0", f"{base}...HEAD", "--", "*.rs"))
    run, total, missed = patch_report(changed, covered_lines(lcov, ROOT))
    if not total:
        print("no executable Rust lines changed")
        return 0
    percent = 100 * run / total
    print(f"{run} of {total} changed lines run in tests: {percent:.1f}% (needs {PATCH_TARGET:.0f}%)")
    for path, numbers in missed.items():
        print(f"  {path}: {', '.join(map(str, numbers))}")
    return 0 if percent >= PATCH_TARGET else 1


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)
    req = sub.add_parser("require")
    req.add_argument("--base", required=True)
    req.add_argument("--head", required=True)
    cov = sub.add_parser("coverage")
    cov.add_argument("--base", default="origin/main")
    cov.add_argument("--lcov")
    args = parser.parse_args()
    if args.command == "coverage":
        return coverage(args.base, args.lcov)
    span = f"{args.base}...{args.head}"
    why = require_tests(
        os.environ.get("PR_TITLE", ""),
        os.environ.get("PR_BODY", ""),
        git("diff", "--name-only", "--diff-filter=AM", span).splitlines(),
        git("diff", span, "--", "*.rs"),
    )
    if why:
        print(f"::error::{why}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
