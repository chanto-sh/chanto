#!/usr/bin/env python3
"""Check a pull request description against the required sections.

The commits workflow runs this for every opened or edited pull request, and
scripts/pr.sh runs it before it calls gh, so a missing section or an AI
attribution is caught before the pull request exists. Reads the description
from --body-file, or stdin.
"""
import argparse
import re
import sys
from pathlib import Path

REQUIRED = ("What this changes", "Testing")
_AI = "claude|anthropic|openai|gpt|copilot|gemini|cursor|codex|devin"


def problems(raw):
    """Why `raw` can't be used as a pull request description."""
    out = []
    # A squash merge copies the description into the commit on main, so it
    # follows the commit message rule on AI attribution (.githooks/commit-msg).
    credits = (
        re.search(rf"co-authored-by:.*({_AI})", raw, re.I)
        or re.search(rf"(generated|written|created|made) with .*({_AI}|chatgpt)", raw, re.I)
        or re.search(r"Generated with \[Claude Code\]|\U0001F916", raw)
    )
    if credits:
        out.append(
            f"the description credits an AI assistant ({credits.group(0)!r}); remove "
            "the attribution, since a squash merge copies it into the commit on main"
        )
    body = re.sub(r"<!--.*?-->", "", raw, flags=re.S)
    sections = {}
    for m in re.finditer(r"^##\s+(.+?)\s*$\n(.*?)(?=^##\s|\Z)", body, re.M | re.S):
        sections[m.group(1).strip().lower()] = m.group(2).strip()
    for section in REQUIRED:
        if not sections.get(section.lower()):
            out.append(
                f"the description needs a filled-in '## {section}' section "
                "(see .github/pull_request_template.md)"
            )
    return out


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--body-file",
        help="read the description from this file instead of stdin",
    )
    args = parser.parse_args(argv)
    raw = (
        sys.stdin.read()
        if not args.body_file or args.body_file == "-"
        else Path(args.body_file).read_text()
    )
    found = problems(raw)
    for problem in found:
        print(f"::error::{problem}", file=sys.stderr)
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main())
