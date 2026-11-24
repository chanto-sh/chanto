#!/usr/bin/env python3
"""Fail when a product source file holds an inline test module.

    python3 scripts/inline_tests.py

Tests live in their own files (`foo/tests.rs`, declared in `foo.rs` with
`#[cfg(test)] mod tests;`), so coverage, which leaves out `tests.rs` and
`*_tests.rs`, counts only product code, and product files stay short. A
`#[cfg(test)]` module written inline as `mod tests { ... }` is refused; a
`mod tests;` declaration is fine.
"""

from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
CFG_TEST = re.compile(r"#\[cfg\((?!.*\bnot\(\s*test\s*\)).*\btest\b.*\)\]")
INLINE_MOD = re.compile(r"(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*\{")


def is_product(path: str) -> bool:
    """Test files, integration tests and fuzz targets may hold test modules."""
    parts = path.split("/")
    name = parts[-1]
    return not (
        parts[0] == "fuzz"
        or "tests" in parts[:-1]
        or name == "tests.rs"
        or name.endswith("_tests.rs")
    )


def test_file_for(path: str, module: str) -> str:
    """Where `module`'s body goes: beside mod.rs, lib.rs or main.rs, else in foo/."""
    p = Path(path)
    folder = p.parent if p.name in ("mod.rs", "lib.rs", "main.rs") else p.with_suffix("")
    return (folder / f"{module}.rs").as_posix()


def inline_test_modules(source: str):
    """(line number, module name) of each `#[cfg(test)]` module written inline."""
    test_attr = False
    for number, line in enumerate(source.splitlines(), 1):
        stripped = line.strip()
        if stripped.startswith("#[") or stripped.startswith("///"):
            test_attr = test_attr or bool(CFG_TEST.match(stripped))
            continue
        if test_attr and (m := INLINE_MOD.match(stripped)):
            yield number, m[1]
        test_attr = False


def main() -> int:
    files = subprocess.run(
        ["git", "ls-files", "*.rs"], cwd=ROOT, capture_output=True, text=True, check=True
    ).stdout.split()
    found = 0
    for path in filter(is_product, files):
        for line, module in inline_test_modules((ROOT / path).read_text()):
            found += 1
            print(
                f"::error file={path},line={line}::{path}:{line} has an inline test module "
                f"`mod {module} {{ ... }}`. Move its body to {test_file_for(path, module)} "
                f"and declare it in {path} with `#[cfg(test)] mod {module};`, so coverage "
                "counts only product code."
            )
    if not found:
        print("inline test modules: none")
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main())
