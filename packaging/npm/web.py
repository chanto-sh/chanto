#!/usr/bin/env python3
"""Turn wasm-bindgen's output for chanto-web into the @chanto/web npm package.

    python3 packaging/npm/web.py vX.Y.Z PKG

PKG holds `wasm-bindgen --target web` output (chanto_web.js, its .d.ts files
and chanto_web_bg.wasm). This adds package.json, the README and the license
notices, so `npm pack PKG` makes the package the release attaches and npm.yml
publishes.
"""
import json
import pathlib
import re
import shutil
import sys

tag, pkg = sys.argv[1], pathlib.Path(sys.argv[2])
if not re.fullmatch(r"v\d+\.\d+\.\d+(-[0-9A-Za-z.]+)?", tag):
    raise SystemExit("expected vX.Y.Z")
root = pathlib.Path(__file__).resolve().parents[2]
built = ["chanto_web.js", "chanto_web.d.ts", "chanto_web_bg.wasm", "chanto_web_bg.wasm.d.ts"]
missing = [name for name in built if not (pkg / name).is_file()]
if missing:
    raise SystemExit(f"{pkg} is missing {', '.join(missing)}: run wasm-bindgen first")
notices = ["LICENSE", "NOTICE", "THIRD_PARTY_LICENSES.txt"]
for name in notices:
    shutil.copyfile(root / name, pkg / name)
shutil.copyfile(pathlib.Path(__file__).parent / "web" / "README.md", pkg / "README.md")
(pkg / ".gitignore").unlink(missing_ok=True)
(pkg / "package.json").write_text(json.dumps({
    "name": "@chanto/web",
    "version": tag[1:],
    "description": "Chanto's agent in the browser: its agent loop and file tools as WebAssembly",
    "license": "Apache-2.0",
    "homepage": "https://chanto.sh",
    "repository": {"type": "git", "url": "git+https://github.com/chanto-sh/chanto.git", "directory": "web"},
    "type": "module",
    "main": "chanto_web.js",
    "module": "chanto_web.js",
    "types": "chanto_web.d.ts",
    "files": built + notices + ["README.md"],
    "sideEffects": False,
    "keywords": ["chanto", "coding-agent", "webassembly"],
}, indent=2) + "\n")
