#!/usr/bin/env python3
"""Check release metadata before tagging: python3 packaging/check-release.py vX.Y.Z."""

import argparse
import datetime
import json
from pathlib import Path
import re
import tomllib


def check(root: Path, tag: str) -> None:
    if not re.fullmatch(r"v\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?", tag):
        raise ValueError("expected a vX.Y.Z release tag")
    version = tag[1:]

    def require(condition: bool, message: str) -> None:
        if not condition:
            raise ValueError(message)

    def toml(path: str) -> dict:
        return tomllib.loads((root / path).read_text())

    for path in ("Cargo.toml", "desktop/Cargo.toml"):
        require(toml(path)["package"]["version"] == version, f"{path}: version mismatch")
    packages = {p["name"]: p["version"] for p in toml("Cargo.lock")["package"]}
    for name in ("chanto", "chanto-desktop"):
        require(packages[name] == version, f"Cargo.lock: {name} version mismatch")
    require(
        json.loads((root / "ide/vscode/package.json").read_text())["version"] == version,
        "VS Code version mismatch",
    )
    require(
        f'version = "{version}"' in (root / "ide/jetbrains/build.gradle.kts").read_text(),
        "JetBrains version mismatch",
    )
    changelog = (root / "CHANGELOG.md").read_text()
    headings = re.findall(r"^## (.+)$", changelog, re.MULTILINE)
    require(len(headings) >= 2 and headings[0] == "Unreleased", "keep Unreleased first")
    dated = re.fullmatch(re.escape(tag) + r" \((.+)\)", headings[1])
    require(dated is not None, "the next changelog section must be this dated release")
    datetime.datetime.strptime(dated[1], "%B %d, %Y")
    require(
        sum(h.startswith(tag + " (") for h in headings) == 1,
        "duplicate release changelog section",
    )
    section = changelog.split("## " + headings[1] + "\n", 1)[1].split("\n## ", 1)[0]
    require(bool(section.strip()), "release notes must not be empty")
    sh_floor = re.search(r"^\s*min_version=(v[0-9.]+)$", (root / "install.sh").read_text(), re.MULTILINE)
    ps_floor = re.search(r"^\$minVersion = '(v[0-9.]+)'$", (root / "install.ps1").read_text(), re.MULTILINE)
    require(sh_floor is not None and ps_floor is not None, "missing installer rollback floor")
    require(sh_floor[1] == ps_floor[1], "installer rollback floors disagree")
    floor = tuple(map(int, sh_floor[1][1:].split(".")))
    current = tuple(map(int, version.split("-", 1)[0].split(".")))
    require(floor <= current, "installer rollback floor exceeds this release")
    snapshots = list((root / "src/tui/snapshots/visual").glob("*_welcome.snap"))
    require(len(snapshots) == 6, "expected six welcome snapshots")
    for snapshot in snapshots:
        require(f"chanto v{version} " in snapshot.read_text(), f"{snapshot.name}: stale welcome version")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tag")
    args = parser.parse_args()
    try:
        check(Path(__file__).resolve().parent.parent, args.tag)
    except (ValueError, KeyError) as error:
        parser.exit(1, f"release metadata: {error}\n")
    print(f"{args.tag}: release metadata is consistent")
