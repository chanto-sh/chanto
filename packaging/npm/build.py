#!/usr/bin/env python3
"""Build the npm packages for a release from its CLI archives.

    build.py vX.Y.Z DIST OUT

DIST holds the release archives (chanto-vX.Y.Z-<target>.tar.gz / .zip). OUT
gets one directory per platform package plus chanto/, ready for `npm publish`.
"""
import json
import pathlib
import re
import shutil
import sys
import tarfile
import zipfile

TARGETS = {
    "darwin-arm64": ("aarch64-apple-darwin", "darwin", "arm64"),
    "darwin-x64": ("x86_64-apple-darwin", "darwin", "x64"),
    "linux-arm64": ("aarch64-unknown-linux-musl", "linux", "arm64"),
    "linux-x64": ("x86_64-unknown-linux-musl", "linux", "x64"),
    "win32-x64": ("x86_64-pc-windows-msvc", "win32", "x64"),
}
COMMON = {
    "license": "Apache-2.0",
    "homepage": "https://chanto.sh",
    "repository": {"type": "git", "url": "git+https://github.com/chanto-sh/chanto.git"},
}

tag, dist, out = sys.argv[1], pathlib.Path(sys.argv[2]), pathlib.Path(sys.argv[3])
if not re.fullmatch(r"v\d+\.\d+\.\d+(-[0-9A-Za-z.]+)?", tag):
    raise SystemExit("expected vX.Y.Z")
version = tag[1:]
here = pathlib.Path(__file__).parent
shutil.rmtree(out, ignore_errors=True)

for key, (target, os_name, cpu) in TARGETS.items():
    pkg = out / key
    (pkg / "bin").mkdir(parents=True)
    exe = "chanto.exe" if os_name == "win32" else "chanto"
    member = f"chanto-{tag}-{target}/{exe}"
    if os_name == "win32":
        with zipfile.ZipFile(dist / f"chanto-{tag}-{target}.zip") as z:
            (pkg / "bin" / exe).write_bytes(z.read(member))
    else:
        with tarfile.open(dist / f"chanto-{tag}-{target}.tar.gz") as t:
            (pkg / "bin" / exe).write_bytes(t.extractfile(member).read())
        (pkg / "bin" / exe).chmod(0o755)
    shutil.copy(here / "../../LICENSE", pkg / "LICENSE")
    (pkg / "README.md").write_text(f"The `chanto` binary for {os_name} {cpu}. Install [`chanto`](https://www.npmjs.com/package/chanto) instead.\n")
    (pkg / "package.json").write_text(json.dumps({
        "name": f"@chanto/cli-{key}",
        "version": version,
        "description": f"The chanto binary for {os_name} {cpu}",
        **COMMON,
        "os": [os_name],
        "cpu": [cpu],
        "files": ["bin", "LICENSE"],
    }, indent=2) + "\n")

main = out / "chanto"
shutil.copytree(here / "chanto", main)
shutil.copy(here / "../../LICENSE", main / "LICENSE")
shutil.copy(here / "../../README.md", main / "README.md")
(main / "package.json").write_text(json.dumps({
    "name": "chanto",
    "version": version,
    "description": "A fast, lean coding agent for your terminal",
    **COMMON,
    "bin": {"chanto": "bin/chanto.js"},
    "files": ["bin", "LICENSE"],
    "engines": {"node": ">=18"},
    "optionalDependencies": {f"@chanto/cli-{k}": version for k in TARGETS},
}, indent=2) + "\n")
print("\n".join(sorted(p.name for p in out.iterdir())))
