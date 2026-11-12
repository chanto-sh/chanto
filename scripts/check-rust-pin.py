#!/usr/bin/env python3
"""Check that every workflow builds with the Rust version rust-toolchain.toml pins.

    python3 scripts/check-rust-pin.py

Each `dtolnay/rust-toolchain` step must name that version as its `toolchain`
(release.yml through RELEASE_RUST_VERSION, set to the same version). Only the
rust-next workflow, which tries the newest stable on purpose, may differ, and
a step whose `toolchain:` line says `rust-pin: exempt`, with the reason in a
comment above it.
"""
import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
# Tries the newest stable ahead of a bump; it's meant to differ.
EXEMPT = {"rust-next.yml"}


def main():
    pin = tomllib.loads((ROOT / "rust-toolchain.toml").read_text())["toolchain"]["channel"]
    problems = []
    for wf in sorted((ROOT / ".github/workflows").glob("*.yml")):
        if wf.name in EXEMPT:
            continue
        lines = wf.read_text().split("\n")
        env = re.search(r"^\s*RELEASE_RUST_VERSION:\s*\"?([^\"\s]+)", "\n".join(lines), re.M)
        for i, line in enumerate(lines):
            m = re.match(r"^(\s*)- uses: dtolnay/rust-toolchain@", line)
            if not m:
                continue
            body = m.group(1) + "    "
            toolchain = None
            for nxt in lines[i + 1:]:
                if not nxt.startswith(m.group(1) + "  "):
                    break
                # A deliberate exception says so on its line, with its
                # reason in a comment above (and RUSTUP_TOOLCHAIN set to match,
                # since rust-toolchain.toml would otherwise win).
                if "rust-pin: exempt" in nxt:
                    toolchain = pin
                    break
                t = re.match(r"^\s*toolchain:\s*(.+?)\s*$", nxt)
                if nxt.startswith(body) and t:
                    toolchain = t.group(1).strip("\"'")
            where = f"{wf.relative_to(ROOT)}:{i + 1}"
            if toolchain == "${{ env.RELEASE_RUST_VERSION }}":
                toolchain = env.group(1) if env else None
            if toolchain != pin:
                problems.append(f"{where}: builds with {toolchain or 'the floating stable'}, not {pin}")
    if problems:
        print(f"rust-toolchain.toml pins {pin}, but:", *problems, sep="\n  ")
        print("Set each step's `toolchain:` (and release.yml's RELEASE_RUST_VERSION) to the pin.")
        sys.exit(1)
    print(f"every workflow builds with Rust {pin}")


if __name__ == "__main__":
    main()
