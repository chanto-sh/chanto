#!/usr/bin/env python3
"""Keep the site's prose concrete.

The homepage and docs sync from this repository, so a sentence that reads
generated ships to the site. These rules catch the shapes that do it — a
benefit tail, a banner phrase, a stacked aside — without judging taste.
"""
from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parent.parent
DOCS = ROOT / "docs"
MAX_WORDS = 40
MAX_EM_DASHES = 1
# The reference enumerates features; a long sentence there is a list, not a
# pitch. It still has to avoid the banner phrases.
LONG_SENTENCE_EXEMPT = {"reference.md"}

# Phrases that make a sentence read like a pitch deck. Matched on whole words,
# outside code.
BANNED = [
    ("so it", "cut the benefit tail; end at the fact"),
    ("not just", "say what it is, not what it is more than"),
    ("simply", "say the action, not that it is simple"),
    ("seamless", "name the thing that needs no work"),
    ("effortless", "name the thing that needs no work"),
    ("and more", "finish the list, or say 'such as'"),
]

FENCE = re.compile(r"^\s*```")
INLINE = re.compile(r"`[^`]*`")


def prose_lines(path):
    """(line number, text) for markdown prose: no fences, no code, no tables."""
    fenced = False
    for n, line in enumerate(path.read_text().splitlines(), 1):
        if FENCE.match(line):
            fenced = not fenced
            continue
        if fenced or line.lstrip().startswith("|"):
            continue
        yield n, INLINE.sub("", line)


def findings(path):
    for n, text in prose_lines(path):
        low = text.lower()
        for phrase, why in BANNED:
            if re.search(rf"\b{re.escape(phrase)}\b", low):
                yield n, f"{phrase!r}: {why}"
        if text.count("—") > MAX_EM_DASHES:
            yield n, f"{text.count('—')} em dashes: use one aside at most"
        if path.name in LONG_SENTENCE_EXEMPT:
            continue
        # Sentence length, on the lines that are prose rather than a list label.
        body = re.sub(r"^\s*[-*]\s+(\*\*[^*]+\*\*\.?\s*)?", "", text)
        for sentence in re.split(r"(?<=[.!?])\s+", body.strip()):
            words = len(sentence.split())
            if words > MAX_WORDS:
                yield n, f"{words}-word sentence: split it (>{MAX_WORDS})"


def main():
    paths = sorted(DOCS.rglob("*.md"))
    bad = 0
    for path in paths:
        # style.md quotes the patterns on purpose.
        if path.name == "style.md":
            continue
        for n, why in findings(path):
            print(f"{path.relative_to(ROOT)}:{n}: {why}", file=sys.stderr)
            bad += 1
    if bad:
        print(f"docs style: {bad} finding(s); see docs/style.md", file=sys.stderr)
        return 1
    print(f"docs style: {len(paths)} files clean")
    return 0


if __name__ == "__main__":
    sys.exit(main())
