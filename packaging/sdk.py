#!/usr/bin/env python3
"""Build the SDK packages: sdk/typescript/package and sdk/python/dist.

    python3 packaging/sdk.py X.Y.Z
"""
import json
import pathlib
import re
import shutil
import subprocess
import sys

version = sys.argv[1]
if not re.fullmatch(r"\d+\.\d+\.\d+", version):
    raise SystemExit("expected X.Y.Z")
sdk = pathlib.Path(__file__).resolve().parents[1] / "sdk"


def typescript():
    root = sdk / "typescript"
    subprocess.run(["npm", "ci", "--ignore-scripts"], cwd=root, check=True)
    subprocess.run(["npx", "tsc", "-p", "tsconfig.publish.json"], cwd=root, check=True)
    package = root / "package"
    shutil.rmtree(package, ignore_errors=True)
    shutil.copytree(root / "dist", package / "dist")
    shutil.copy(root / "README.md", package)
    shutil.copy(sdk / "LICENSE", package)
    (package / "package.json").write_text(json.dumps({
        "name": "@chanto/sdk",
        "version": version,
        "description": "TypeScript client for `chanto serve`, the Chanto coding agent's server",
        "license": "Apache-2.0",
        "type": "module",
        "main": "dist/index.js",
        "types": "dist/index.d.ts",
        "exports": {".": {"types": "./dist/index.d.ts", "default": "./dist/index.js"}},
        "repository": {"type": "git", "url": "git+https://github.com/chanto-sh/chanto.git", "directory": "sdk/typescript"},
        "engines": {"node": ">=18"},
    }, indent=2) + "\n")
    # What a user does first: a package that can't be imported can't be unpublished.
    subprocess.run(
        ["node", "--input-type=module", "-e",
         'const {Chanto} = await import("./package/dist/index.js");'
         'if (typeof Chanto !== "function") throw new Error("no Chanto export");'],
        cwd=root, check=True,
    )


def python():
    root = sdk / "python"
    project = root / "pyproject.toml"
    original = project.read_text()
    project.write_text(original.replace('version = "0.0.0"', f'version = "{version}"'))
    shutil.copy(sdk / "LICENSE", root / "LICENSE")
    try:
        shutil.rmtree(root / "dist", ignore_errors=True)
        subprocess.run(
            [sys.executable, "-m", "pip", "wheel", "--no-deps", "--wheel-dir", "dist", "."],
            cwd=root, check=True,
        )
    finally:
        project.write_text(original)
        (root / "LICENSE").unlink(missing_ok=True)


typescript()
python()
