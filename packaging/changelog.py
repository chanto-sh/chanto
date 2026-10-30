#!/usr/bin/env python3
"""Changelog fragments: each pull request adds changelog.d/<name>.md instead of
editing CHANGELOG.md, so open pull requests never conflict over its top lines.

    python3 packaging/changelog.py --check          validate the fragments (CI)
    python3 packaging/changelog.py --require --base SHA --head SHA
        fail a pull request whose title says it is user-visible (feat, fix,
        perf) but which adds no fragment, unless its description opts out
        with a `Changelog: none (reason)` line; the title and description
        come from PR_TITLE and PR_BODY (CI)
    python3 packaging/changelog.py vX.Y.Z [--date "October 28, 2026"]
        move the Unreleased entries and every fragment into a dated
        `## vX.Y.Z (...)` section and delete the fragments (release PR)
"""

import argparse
import datetime
import os
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parent.parent


def fragments(root: Path) -> list[Path]:
    """Fragments in the order they were added (git history), then by name."""
    files = sorted(p for p in (root / "changelog.d").glob("*.md") if p.name != "README.md")

    def added(path: Path) -> int:
        out = subprocess.run(
            ["git", "log", "--diff-filter=A", "--format=%ct", "--", path.name],
            cwd=path.parent, capture_output=True, text=True,
        ).stdout.split()
        # Not committed yet (or no git): after everything that is.
        return int(out[-1]) if out else 1 << 62

    return sorted(files, key=lambda p: (added(p), p.name))


def check_fragment(path: Path) -> str:
    text = path.read_text().strip()
    if not text.startswith("- "):
        raise ValueError(f"{path.name}: a fragment is one or more `- ` list items")
    if re.search(r"^#", text, re.MULTILINE):
        raise ValueError(f"{path.name}: no headings in a fragment")
    return text


# Conventional Commit types users can notice: a pull request of one of these
# carries a fragment, or says why not.
USER_VISIBLE = ("feat", "fix", "perf")


def require_fragment(title: str, body: str, changed: list[str]) -> str | None:
    """Why a pull request needs a fragment it doesn't have, or None.

    `changed` is the paths the pull request adds or modifies. Opting out takes
    a `Changelog: none` line with a reason, in the description outside the
    template's comments, so the reason lands in the squash commit.
    """
    m = re.match(r"([a-z]+)(\([^)]*\))?!?:", title.strip())
    if not m or m.group(1) not in USER_VISIBLE:
        return None
    if any(
        p.startswith("changelog.d/") and p.endswith(".md") and p != "changelog.d/README.md"
        for p in changed
    ):
        return None
    text = re.sub(r"<!--.*?-->", "", body or "", flags=re.S)
    if re.search(r"^\s*changelog:\s*none\b[\s:(—-]*\w", text, re.I | re.M):
        return None
    return (
        f"a `{m.group(1)}` pull request is user-visible: add a changelog.d/<name>.md "
        "fragment (changelog.d/README.md), or, if users can't notice it, a line "
        "`Changelog: none (<why>)` to the description"
    )


def changed_paths(base: str, head: str) -> list[str]:
    """The paths added or modified between `base` and `head`."""
    out = subprocess.run(
        ["git", "diff", "--name-only", "--diff-filter=AM", f"{base}...{head}"],
        cwd=ROOT, capture_output=True, text=True, check=True,
    ).stdout
    return [line for line in out.splitlines() if line]


def release(root: Path, tag: str, date: str) -> list[Path]:
    if not re.fullmatch(r"v\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?", tag):
        raise ValueError("expected a vX.Y.Z release tag")
    datetime.datetime.strptime(date, "%B %d, %Y")
    path = root / "CHANGELOG.md"
    changelog = path.read_text()
    if re.search(r"^## " + re.escape(tag) + " ", changelog, re.MULTILINE):
        raise ValueError(f"CHANGELOG.md already has a {tag} section")
    head, sep, rest = changelog.partition("## Unreleased\n")
    if not sep:
        raise ValueError("CHANGELOG.md has no Unreleased section")
    unreleased, nxt, tail = rest.partition("\n## ")
    files = fragments(root)
    entries = [e for e in [unreleased.strip()] + [check_fragment(f) for f in files] if e]
    if not entries:
        raise ValueError("nothing to release: no Unreleased entries or fragments")
    section = f"## {tag} ({date})\n\n" + "\n".join(entries) + "\n"
    path.write_text(head + "## Unreleased\n\n" + section + ("\n## " + tail if nxt else ""))
    for f in files:
        f.unlink()
    return files


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("tag", nargs="?")
    today = datetime.date.today()
    parser.add_argument("--date", default=f"{today:%B} {today.day}, {today.year}")
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--require", action="store_true")
    parser.add_argument("--base")
    parser.add_argument("--head")
    args = parser.parse_args()
    try:
        if args.require:
            if not (args.base and args.head):
                parser.error("--require needs --base and --head")
            why = require_fragment(
                os.environ.get("PR_TITLE", ""),
                os.environ.get("PR_BODY", ""),
                changed_paths(args.base, args.head),
            )
            if why:
                parser.exit(1, f"changelog: {why}\n")
            print("changelog: ok")
        elif args.check:
            files = fragments(ROOT)
            for f in files:
                check_fragment(f)
            print(f"{len(files)} changelog fragment(s) ok")
        elif args.tag:
            files = release(ROOT, args.tag, args.date)
            print(f"{args.tag}: collected {len(files)} fragment(s) into CHANGELOG.md")
        else:
            parser.error("give a release tag or --check")
    except ValueError as error:
        parser.exit(1, f"changelog: {error}\n")


if __name__ == "__main__":
    main()
