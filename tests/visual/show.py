#!/usr/bin/env python3
"""Display a captured Ratatui frame until Enter; no model or agent is started."""
import hashlib
import pathlib
import os
import sys


def caption(capture):
    columns = int(capture.with_suffix(".txt").read_text().splitlines()[0].split("x")[0])
    label = "fixture: " + capture.stem
    if len(label) >= columns:
        label = "fixture: " + hashlib.sha256(capture.stem.encode()).hexdigest()[:12]
    return label


def main():
    capture = pathlib.Path(sys.argv[1])
    rows = int(capture.with_suffix(".txt").read_text().splitlines()[0].split("x")[1])
    columns = int(capture.with_suffix(".txt").read_text().splitlines()[0].split("x")[0])
    actual = os.get_terminal_size()
    if actual.columns != columns or actual.lines != rows + 1:
        raise SystemExit(f"Expected {columns}x{rows + 1} terminal, got {actual.columns}x{actual.lines}")
    sys.stdout.write(capture.read_text())
    sys.stdout.write(f"\x1b[{rows + 1};1H\x1b[0m{caption(capture)}")
    sys.stdout.flush()
    sys.stdin.readline()
    sys.stdout.write("\x1b[0m\x1b[?25h")
    sys.stdout.flush()


if __name__ == "__main__":
    main()
