#!/usr/bin/env python3
"""Action pins: exact release tags, checked on every pull request.

A pin is `uses: owner/repo@<40-hex-sha> # <comment>`. GitHub moves the floating
major tags (v2, v7) and branches (stable) with every release, so a pin labelled
by one goes stale as soon as upstream ships: zizmor's ref-version-mismatch then
fails scorecard guard on every pull request until the pin is refreshed. An
exact release tag (v2.1.3) does not move, so label pins with one.

    python3 scripts/action-pins.py --check           # offline, in CI
    python3 scripts/action-pins.py --check --online  # also audit against GitHub
    python3 scripts/action-pins.py --update          # refresh every pin

Actions that publish no version tags (dtolnay/rust-toolchain, which has only
branches and a moving v1) are pinned to the named ref's commit and carry a
line-level `zizmor: ignore[ref-version-mismatch]` with that reason, so drift
there is refreshed weekly rather than failing someone's pull request.
"""
import argparse
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
WORKFLOWS = ROOT / ".github" / "workflows"

PIN = re.compile(
    r"^(?P<head>\s*-?\s*uses:\s*)(?P<action>[^\s@]+)@(?P<sha>[0-9a-f]{40})"
    r"(?:\s*#\s*(?P<comment>.*?))?\s*$"
)
EXACT = re.compile(r"^v?\d+\.\d+\.\d+$")
IGNORE = "zizmor: ignore[ref-version-mismatch]"


def repo_of(action):
    """owner/repo for owner/repo[/path]."""
    return "/".join(action.split("/")[:2])


def exact_key(tag):
    m = re.match(r"^v?(\d+)\.(\d+)\.(\d+)$", tag)
    return tuple(int(g) for g in m.groups()) if m else None


def pins(path):
    """(line number, match) for every pinned action in one workflow."""
    for n, line in enumerate(path.read_text().splitlines(), 1):
        m = PIN.match(line)
        if m:
            yield n, m


def all_pins():
    for p in sorted(WORKFLOWS.glob("*.yml")):
        for n, m in pins(p):
            yield p, n, m


def offline_errors():
    """Format and consistency problems, with no network."""
    errors = []
    seen = {}
    for path, n, m in all_pins():
        where = f"{path.relative_to(ROOT)}:{n}"
        comment = (m["comment"] or "").strip()
        if EXACT.match(comment):
            pass
        elif IGNORE in comment:
            pass
        else:
            errors.append(
                f"{where}: {m['action']}@{m['sha'][:10]} is labelled "
                f"{comment!r}; use an exact release tag (vX.Y.Z) or a "
                f"{IGNORE} with the reason"
            )
        key = (repo_of(m["action"]), m["sha"], comment)
        seen.setdefault(repo_of(m["action"]), set()).add((m["sha"], comment))
        _ = key
    for repo, variants in seen.items():
        if len(variants) > 1:
            errors.append(
                f"{repo} is pinned more than one way: "
                + ", ".join(f"{s[:10]} ({c})" for s, c in sorted(variants))
            )
    return errors


def ls_remote(repo):
    """refs for one action repo, as {ref: sha} (peeled commits win)."""
    url = f"https://github.com/{repo}"
    out = subprocess.run(
        ["git", "ls-remote", url, "refs/tags/*", "refs/heads/*"],
        capture_output=True, text=True, timeout=120,
    )
    if out.returncode != 0:
        raise SystemExit(f"git ls-remote {url} failed:\n{out.stderr.strip()}")
    refs = {}
    for line in out.stdout.splitlines():
        sha, ref = line.split("\t", 1)
        if ref.endswith("^{}"):
            refs[ref[:-3]] = sha  # the commit, not the tag object
        else:
            refs.setdefault(ref, sha)
    return refs


def latest_exact(refs, major=None):
    """(tag, commit) of the highest exact vX.Y.Z tag, or None.

    With `major`, stay within that major version: a pin moves to the newest
    release of the line it already tracks, and dependency bots carry the
    breaking major bumps.
    """
    best = None
    for ref, sha in refs.items():
        if not ref.startswith("refs/tags/"):
            continue
        tag = ref[len("refs/tags/"):]
        key = exact_key(tag)
        if key is None or (major is not None and key[0] != major):
            continue
        if best is None or key > best[0]:
            best = (key, tag, sha)
    return (best[1], best[2]) if best else None


def resolve_ref(refs, comment):
    """The commit a non-version label names (a branch or moving tag)."""
    for prefix in ("refs/heads/", "refs/tags/"):
        sha = refs.get(prefix + comment)
        if sha:
            return sha
    return None


def online_drift(update=False):
    """Pins that are not the current release/ref; rewrite them if asked."""
    drift = []
    cache = {}
    for repo in sorted({repo_of(m["action"]) for _, _, m in all_pins()}):
        cache[repo] = ls_remote(repo)
    for path, n, m in all_pins():
        repo = repo_of(m["action"])
        refs = cache[repo]
        comment = (m["comment"] or "").strip()
        major_match = re.match(r"^v?(\d+)(?:\.\d+\.\d+)?$", comment)
        major = int(major_match.group(1)) if major_match else None
        exact = latest_exact(refs, major) or latest_exact(refs)
        if exact:
            tag, sha = exact
            if (m["sha"], comment) == (sha, tag):
                continue
            drift.append((path, n, m, f"{tag} ({sha[:10]})"))
            if update:
                replace(path, n, m, sha, tag)
            continue
        # No version tags upstream: follow the ref the comment names, and make
        # sure the line says why zizmor is told to ignore the mismatch.
        named = resolve_ref(refs, comment)
        if named is None:
            drift.append((path, n, m, f"no ref named {comment!r} upstream"))
            continue
        label = comment if IGNORE not in comment else comment
        if IGNORE not in comment:
            label = (f"{comment} {IGNORE} upstream publishes no version tags").strip()
        if (m["sha"], label) != (named, label) or IGNORE not in (m["comment"] or ""):
            drift.append((path, n, m, f"{comment} ({named[:10]})"))
            if update:
                replace(path, n, m, named, label)
    return drift


def replace(path, line_no, match, sha, comment):
    lines = path.read_text().splitlines(keepends=True)
    newline = lines[line_no - 1].replace(
        f"{match['action']}@{match['sha']}", f"{match['action']}@{sha}", 1
    )
    if match["comment"] is None:
        newline = newline.rstrip("\n") + f" # {comment}\n"
    else:
        newline = re.sub(r"#\s*.*?(?=\n|$)", f"# {comment}", newline, count=1)
    lines[line_no - 1] = newline
    path.write_text("".join(lines))


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--update", action="store_true")
    parser.add_argument("--online", action="store_true")
    args = parser.parse_args()
    if not (args.check or args.update):
        parser.error("needs --check or --update")
    if args.check:
        errors = offline_errors()
        if args.online:
            errors += [
                f"{p.relative_to(ROOT)}:{n}: {m['action']}@{m['sha'][:10]} "
                f"({m['comment']}) is not {want}"
                for p, n, m, want in online_drift()
            ]
        for e in errors:
            print(f"error: {e}", file=sys.stderr)
        if errors:
            print(
                "run `python3 scripts/action-pins.py --update` to refresh them",
                file=sys.stderr,
            )
            return 1
        print("action pins: every pin is an exact release or documented ref")
    if args.update:
        drift = online_drift(update=True)
        for p, n, m, want in drift:
            print(f"updated {p.relative_to(ROOT)}:{n} -> {m['action']} {want}")
        if not drift:
            print("action pins: already current")
    return 0


if __name__ == "__main__":
    sys.exit(main())
