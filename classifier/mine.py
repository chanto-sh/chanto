#!/usr/bin/env python3
"""Mine public command data for the fast path's training set (src/risk.rs).

    python3 classifier/mine.py >> classifier/train.jsonl

Reads compact snapshots from classifier/sources/ (see sources/README.md for
provenance and licenses) and prints one {"cmd", "risky"} per line, the same
shape gen.py emits:

  - sources/tldr.jsonl (tldr-pages, CC BY 4.0): routine examples, but only for
    an allowlist of read-only local tools and only when the command carries no
    risky token. tldr has pages for rm, dd, sudo, git push, ...; teaching the
    model those are routine would be a safety hole, so they're filtered out.
  - sources/gtfobins.jsonl (GTFOBins, GPL-3.0): privilege-escalation and
    shell-escape snippets, labelled risky. NOT vendored here, because its
    GPL-3.0 terms would attach to the training data. It is read or written only
    with an explicit --allow-gpl, and its snapshot is gitignored; without the
    flag it is skipped.

--tldr-dir/--gtfobins-dir take a raw tldr pages/ directory or GTFOBins
_gtfobins/ directory instead of the snapshots, and --extract writes a compact
snapshot (used to build sources/*.jsonl in the first place). Reading or
writing GTFOBins output requires --allow-gpl.
"""

import argparse
import json
import pathlib
import re
import sys

# Read-only local tools whose tldr examples are routine for a coding agent.
# Anything that can write, delete, install, fetch or escalate is left out; the
# fast path should keep deferring those to the model classifier.
SAFE_PROGRAMS = {
    "awk", "base64", "basename", "b2sum", "cat", "cksum", "column", "comm",
    "cut", "date", "df", "diff", "dirname", "du", "echo", "env", "expand",
    "file", "fmt", "fold", "free", "grep", "head", "hexdump", "id", "jq",
    "join", "less", "ls", "md5sum", "nl", "od", "paste", "printf", "ps",
    "pwd", "realpath", "readlink", "rev", "rg", "seq", "sha1sum", "sha256sum",
    "sha512sum", "shuf", "sort", "split", "stat", "strings", "sum", "tac",
    "tail", "tr", "tree", "tsort", "type", "uname", "uniq", "uptime", "wc",
    "which", "whoami", "xxd", "yq",
}

# A command that carries any of these is never labelled routine, whatever
# program it starts with (find -delete, sed -i, xargs rm, ...).
RISKY_TOKEN = re.compile(
    r"(^|[\s;&|(])("
    r"sudo|doas|su|dd|mkfs|mount|umount|shutdown|reboot|halt|poweroff|"
    r"kill|pkill|killall|rm|unlink|mv|cp|ln|tee|truncate|shred|wipefs|"
    r"fdisk|parted|chmod|chown|chgrp|useradd|userdel|passwd|ssh|scp|sftp|"
    r"rsync|curl|wget|nc|ncat|socat|telnet|systemctl|launchctl|service|"
    r"apt|apt-get|yum|dnf|brew|pip|pip3|npm|pnpm|yarn|cargo|gem|docker|"
    r"kubectl|helm|terraform|aws|gcloud|az|gh|git|eval|exec|source"
    r")\b"
)
RISKY_PAT = re.compile(
    r"(-delete|-exec\b|-execdir|-ok\b|\|\s*(sh|bash|zsh)\b|\.\s*/bin/|"
    r">\s*/(etc|usr|var|bin|sbin|System)\b|>\s*~|>\s*\$HOME|\brm\b|\bdd\b|"
    r"--force\b|\bmkfs\b|:\s*\(\)\s*\{|\bchmod\b|\bsudo\b)"
)

PLACEHOLDER = {
    "path/to/file": "file.txt", "path/to/files": "file.txt", "file": "file.txt",
    "filename": "file.txt", "path/to/input-file": "file.txt", "input": "file.txt",
    "path/to/directory": "dir", "directory": "dir", "path": "file.txt",
    "hostname": "example.com", "ip": "127.0.0.1",
    "address": "127.0.0.1", "port": "8080", "url": "https://example.com",
    "username": "user", "user": "user", "pattern": "pattern", "string": "text",
    "text": "text", "name": "name", "value": "value", "variable": "VAR",
    "n": "1", "number": "1", "delimiter": ":", "sep": ",", "letter": "a",
}

# Placeholders that stand for a whole command or program: filling them with a
# literal would invent a bogus command, so the example is skipped instead.
BAD_PLACEHOLDER = {"command", "commands", "program", "script", "cmd", "shell"}


def _placeholder(inner: str) -> str:
    """Fill a {{...}} tldr placeholder with something benign and stable."""
    inner = inner.strip()
    if not inner:
        return "x"
    if "|" in inner:  # an option group like [-c|--characters]
        return inner.split("|")[0].strip().strip("[]").strip()
    if inner.isdigit():
        return inner
    if len(inner) <= 2 and not inner.isalnum():
        return inner  # a literal delimiter like : or /
    if inner[0] in "-*+=~." or ("." in inner and "path" not in inner.lower()):
        return inner  # a flag, glob or size like *.ext, +500k, -10M
    key = inner.lower()
    if key in PLACEHOLDER:
        return PLACEHOLDER[key]
    if "path" in key or "file" in key or "dir" in key:
        return "file.txt"
    if "host" in key or "domain" in key or "server" in key:
        return "example.com"
    if "url" in key:
        return "https://example.com"
    return re.sub(r"\W+", "_", inner).strip("_") or "x"


PLACE = re.compile(r"\{\{(.*?)\}\}")


def fill(cmd: str) -> str | None:
    """Replace every {{...}} placeholder, or None if one can't be filled well."""
    bad = []

    def one(m):
        inner = m.group(1).strip()
        # A nested command or angle-bracket token can't be filled with a benign
        # literal without producing nonsense (`tsort <v>`); skip the example.
        if re.search(r"\s", inner) or "<" in inner or ">" in inner or inner.lower() in BAD_PLACEHOLDER:
            bad.append(inner)
            return m.group(0)
        return _placeholder(inner)

    filled = PLACE.sub(one, cmd)
    return None if bad else filled


def parse_tldr(root: pathlib.Path):
    """Yield (cmd, program) for every code example in a tldr pages tree."""
    for page in sorted(root.rglob("*.md")):
        prog = page.stem
        for line in page.read_text(errors="replace").splitlines():
            line = line.strip()
            if len(line) > 2 and line[0] == "`" and line[-1] == "`" and "`" not in line[1:-1]:
                cmd = fill(line[1:-1])
                if cmd is not None:
                    yield cmd, prog


def parse_gtfobins(root: pathlib.Path):
    """Yield (cmd, name) for every code block in a GTFOBins tree.

    Blocks are YAML `code: |` scalars; the body is the run of lines indented
    deeper than the `code:` key, so sibling keys (comment:, contexts:, ...)
    aren't swallowed.
    """
    block = re.compile(r"^(?P<i>[ \t]*)code: [|>][-+]?\d*\s*\n(?P<b>(?:(?P=i)[ \t]+.*\n|[ \t]*\n)*)", re.M)
    for page in sorted(p for p in root.iterdir() if p.is_file() and not p.name.startswith(".")):
        for m in block.finditer(page.read_text(errors="replace")):
            cmd = "\n".join(ln.strip() for ln in m.group("b").splitlines() if ln.strip())
            if cmd:
                yield cmd, page.name


SYS_PATH = re.compile(r"(^|\s)(/etc|/usr|/var|/bin|/sbin|/System|/Library|/opt)(/|\b)|~")
PROGRAM = re.compile(r"^[A-Za-z][\w.+-]*$")


def routine(entries):
    """Keep only examples that are safe by program and by token."""
    seen = set()
    for cmd, prog in entries:
        if prog not in SAFE_PROGRAMS:
            continue
        if RISKY_TOKEN.search(cmd) or RISKY_PAT.search(cmd) or SYS_PATH.search(cmd):
            continue
        if "\n" in cmd or "[" in cmd or "]" in cmd or not cmd.strip() or cmd in seen:
            continue
        if not PROGRAM.match(cmd.split()[0]):
            continue  # an unfilled placeholder at the head (<v>, </>pattern)
        seen.add(cmd)
        yield {"cmd": cmd, "risky": 0}


def risky(entries):
    seen = set()
    for cmd, _ in entries:
        one = " ".join(cmd.split())
        if not one or one in seen:
            continue
        seen.add(one)
        yield {"cmd": one, "risky": 1}


def load(path: pathlib.Path, kind: str):
    if path.is_dir():
        return parse_tldr(path) if kind == "tldr" else parse_gtfobins(path)
    rows = []
    for line in path.read_text().splitlines():
        if line.strip():
            v = json.loads(line)
            rows.append((v["cmd"], v.get("source", "")))
    return rows


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    here = pathlib.Path(__file__).resolve().parent
    ap.add_argument("--sources", default=str(here / "sources"), help="snapshot directory")
    ap.add_argument("--tldr-dir", help="raw tldr pages/ directory (overrides the snapshot)")
    ap.add_argument("--gtfobins-dir", help="raw GTFOBins _gtfobins/ directory (licensed GPL-3.0, not vendored)")
    ap.add_argument("--extract", nargs=3, metavar=("KIND", "RAW_DIR", "OUT"), help="write a compact snapshot and exit")
    ap.add_argument("--allow-gpl", action="store_true",
                    help="permit GTFOBins (GPL-3.0) input/output; keep the result out of the repo")
    args = ap.parse_args()

    if args.extract:
        kind, raw, out = args.extract
        if kind == "gtfobins" and not args.allow_gpl:
            ap.error("--extract gtfobins writes GPL-3.0-derived data; pass --allow-gpl and keep it out of the repo")
        entries = (parse_tldr if kind == "tldr" else parse_gtfobins)(pathlib.Path(raw))
        with open(out, "w") as fh:
            for cmd, src in entries:
                fh.write(json.dumps({"cmd": cmd, "source": src}) + "\n")
        return

    src = pathlib.Path(args.sources)
    tldr_path = pathlib.Path(args.tldr_dir) if args.tldr_dir else src / "tldr.jsonl"
    for row in routine(load(tldr_path, "tldr")) if tldr_path.exists() else ():
        print(json.dumps(row))
    gtf_path = pathlib.Path(args.gtfobins_dir) if args.gtfobins_dir else src / "gtfobins.jsonl"
    if gtf_path.exists():
        if args.allow_gpl:
            for row in risky(load(gtf_path, "gtfobins")):
                print(json.dumps(row))
        else:
            print(
                f"skipping {gtf_path} (GTFOBins, GPL-3.0): pass --allow-gpl and keep the result out of the repo",
                file=sys.stderr,
            )


if __name__ == "__main__":
    sys.exit(main())
