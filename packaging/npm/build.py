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
# The license notices every release archive carries next to the binary.
NOTICES = ["LICENSE", "NOTICE", "THIRD_PARTY_LICENSES.txt"]
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
    # The binary, with the license notices its archive carries.
    names = {f"chanto-{tag}-{target}/{n}": n for n in NOTICES}
    if os_name == "win32":
        with zipfile.ZipFile(dist / f"chanto-{tag}-{target}.zip") as z:
            (pkg / "bin" / exe).write_bytes(z.read(member))
            for m, n in names.items():
                (pkg / n).write_bytes(z.read(m))
    else:
        with tarfile.open(dist / f"chanto-{tag}-{target}.tar.gz") as t:
            (pkg / "bin" / exe).write_bytes(t.extractfile(member).read())
            for m, n in names.items():
                (pkg / n).write_bytes(t.extractfile(m).read())
        (pkg / "bin" / exe).chmod(0o755)
    (pkg / "README.md").write_text(f"The `chanto` binary for {os_name} {cpu}. Install [`@chanto/cli`](https://www.npmjs.com/package/@chanto/cli) instead.\n")
    (pkg / "package.json").write_text(json.dumps({
        "name": f"@chanto/cli-{key}",
        "version": version,
        "description": f"The chanto binary for {os_name} {cpu}",
        **COMMON,
        "os": [os_name],
        "cpu": [cpu],
        "files": ["bin", *NOTICES],
    }, indent=2) + "\n")

main = out / "chanto"
shutil.copytree(here / "chanto", main)
for n in NOTICES:
    shutil.copy(out / "linux-x64" / n, main / n)
shutil.copy(here / "../../README.md", main / "README.md")
(main / "package.json").write_text(json.dumps({
    "name": "@chanto/cli",
    "version": version,
    "description": "A fast, lean coding agent for your terminal",
    **COMMON,
    "bin": {"chanto": "bin/chanto.js"},
    "files": ["bin", *NOTICES],
    "engines": {"node": ">=18"},
    "optionalDependencies": {f"@chanto/cli-{k}": version for k in TARGETS},
}, indent=2) + "\n")
print("\n".join(sorted(p.name for p in out.iterdir())))
