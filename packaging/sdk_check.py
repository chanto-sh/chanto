#!/usr/bin/env python3
"""Install the SDK packages somewhere clean and use them against a real server,
as a user would. A version can't be replaced once it's published, so this runs
before publishing, and with --published, against the registries after it.

    python3 packaging/sdk_check.py --binary target/debug/chanto
    python3 packaging/sdk_check.py --binary target/debug/chanto --published X.Y.Z
"""
import argparse
import base64
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import time
import urllib.request

sdk = pathlib.Path(__file__).resolve().parents[1] / "sdk"

PYTHON = """
import sys
from chanto_sdk import Chanto
with Chanto.start(binary=sys.argv[1]) as h:
    print("python", h.check_version(), h.health())
"""

TYPESCRIPT = """
import { Chanto } from "@chanto/sdk";
const h = await Chanto.start({ binary: process.argv[2] });
try {
  console.log("typescript", await h.checkVersion());
} finally {
  h.close();
}
"""


def run(*cmd, cwd):
    subprocess.run([str(c) for c in cmd], cwd=cwd, check=True)


def retrying(*cmd, cwd):
    """A new version takes a few minutes to reach a registry's index."""
    for attempt in range(20):
        if subprocess.run([str(c) for c in cmd], cwd=cwd).returncode == 0:
            return
        print(f"not available yet (attempt {attempt + 1})", flush=True)
        time.sleep(30)
    raise SystemExit(f"never became available: {' '.join(map(str, cmd))}")


def fetch(url):
    with urllib.request.urlopen(url, timeout=30) as response:
        return json.load(response)


def npm_source(attestations):
    """The repository, workflow and ref an npm package's provenance names."""
    for attestation in attestations["attestations"]:
        if attestation["predicateType"].startswith("https://slsa.dev/provenance/"):
            payload = attestation["bundle"]["dsseEnvelope"]["payload"]
            workflow = json.loads(base64.b64decode(payload))["predicate"]["buildDefinition"]["externalParameters"]["workflow"]
            return workflow["repository"], workflow["path"], workflow["ref"]
    raise SystemExit("no SLSA provenance on the npm package")


def pypi_source(provenance):
    """The repository and workflow a PyPI file's attestations name."""
    publisher = provenance["attestation_bundles"][0]["publisher"]
    return publisher["repository"], publisher["workflow"]


def check_source(version):
    """A signature only says a pipeline built something: require that it was
    ours, from this repository, workflow and tag."""
    got = npm_source(fetch(f"https://registry.npmjs.org/-/npm/v1/attestations/@chanto%2fsdk@{version}"))
    want = ("https://github.com/chanto-sh/chanto", ".github/workflows/sdk-publish.yml", f"refs/tags/v{version}")
    if got != want:
        raise SystemExit(f"npm provenance names {got}, expected {want}")
    release = fetch(f"https://pypi.org/pypi/chanto-sdk/{version}/json")
    for file in release["urls"]:
        got = pypi_source(fetch(f"https://pypi.org/integrity/chanto-sdk/{version}/{file['filename']}/provenance"))
        if got != ("chanto-sh/chanto", "sdk-publish.yml"):
            raise SystemExit(f"{file['filename']} provenance names {got}")
    print("provenance names chanto-sh/chanto, sdk-publish.yml")


def check_python(binary, published, home):
    with tempfile.TemporaryDirectory() as d:
        d = pathlib.Path(d)
        run(sys.executable, "-m", "venv", "venv", cwd=d)
        pip = d / "venv/bin/pip"
        if published:
            retrying(pip, "install", "--no-cache-dir", f"chanto-sdk=={published}", cwd=d)
        else:
            wheels = list((sdk / "python/dist").glob("*.whl"))
            if len(wheels) != 1:
                raise SystemExit(f"expected one wheel, found {wheels}")
            run(pip, "install", wheels[0], cwd=d)
        (d / "check.py").write_text(PYTHON)
        subprocess.run([d / "venv/bin/python", "-I", "check.py", binary], cwd=d, check=True, env=dict(os.environ, CHANTO_HOME=home))


def check_typescript(binary, published, home):
    with tempfile.TemporaryDirectory() as d:
        d = pathlib.Path(d)
        (d / "package.json").write_text('{"private": true, "type": "module"}\n')
        if published:
            retrying("npm", "install", "--ignore-scripts", "--prefer-online", f"@chanto/sdk@{published}", cwd=d)
            run("npm", "audit", "signatures", cwd=d)
        else:
            run("npm", "pack", "--pack-destination", d, cwd=sdk / "typescript/package")
            tarball = next(d.glob("*.tgz"))
            run("npm", "install", "--ignore-scripts", tarball, cwd=d)
        (d / "check.mjs").write_text(TYPESCRIPT)
        subprocess.run(["node", "check.mjs", binary], cwd=d, check=True, env=dict(os.environ, CHANTO_HOME=home))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", required=True)
    ap.add_argument("--published", metavar="X.Y.Z", help="install this version from npm and PyPI instead of the build")
    a = ap.parse_args()
    binary = pathlib.Path(a.binary).resolve()
    with tempfile.TemporaryDirectory() as home:
        check_python(binary, a.published, home)
        check_typescript(binary, a.published, home)
    if a.published:
        check_source(a.published)


if __name__ == "__main__":
    main()
