#!/usr/bin/env python3
"""Keep the block-buffer advisory exception limited to reviewed versions."""
import re
import sys
import tomllib
from pathlib import Path

ADVISORY = "GHSA-qwgh-2vcv-g2f7"
CHECKSUM_0104 = "3078c7629b62d3f0439517fa394996acacc5cbc91c5a20d8c658e77abd503a71"


def problems(root):
    errors = []
    for directory in (root, root / "fuzz"):
        config = directory / "osv-scanner.toml"
        if not config.exists():
            continue
        settings = tomllib.loads(config.read_text())
        if not any(item["id"] == ADVISORY for item in settings.get("IgnoredVulns", [])):
            continue
        lock = directory / "Cargo.lock"
        for package in tomllib.loads(lock.read_text()).get("package", []):
            if package["name"] != "block-buffer":
                continue
            version = package["version"]
            fixed = re.fullmatch(r"(\d+)\.(\d+)\.(\d+)", version)
            if fixed and tuple(map(int, fixed.groups())) >= (0, 12, 1):
                continue
            if (version == "0.10.4" and package.get("checksum") == CHECKSUM_0104
                    and package.get("source") == "registry+https://github.com/rust-lang/crates.io-index"):
                continue
            errors.append(f"{lock}: block-buffer {version} is not covered by the reviewed {ADVISORY} exception; reassess it before changing the lockfile")
    return errors


if __name__ == "__main__":
    found = problems(Path(__file__).resolve().parent.parent)
    for error in found:
        print(error, file=sys.stderr)
    sys.exit(bool(found))
