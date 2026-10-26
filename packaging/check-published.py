#!/usr/bin/env python3
"""Wait for a release to be readable from the public package channels."""

import argparse
import base64
import hashlib
import io
import json
from pathlib import Path
import re
import time
import tarfile
import urllib.error
import urllib.request
import urllib.parse


PLATFORMS = ("darwin-arm64", "darwin-x64", "linux-arm64", "linux-x64", "win32-x64")


def read(url):
    request = urllib.request.Request(url, headers={"User-Agent": "chanto-release-verification"})
    with urllib.request.urlopen(request, timeout=20) as response:
        return response.read().decode()


def verify_payload(archive, integrity, expected):
    actual = "sha512-" + base64.b64encode(hashlib.sha512(archive).digest()).decode()
    if actual != integrity:
        raise ValueError("npm tarball integrity mismatch")
    with tarfile.open(fileobj=io.BytesIO(archive), mode="r:gz") as package:
        member = package.getmember("package/bin/chanto")
        if not member.isfile() or member.size > 256 * 1024 * 1024:
            raise ValueError("invalid npm binary entry")
        with package.extractfile(member) as binary:
            if binary.read() != expected:
                raise ValueError("npm binary differs from the signed release")


def verify_npm_binary(tag, expected):
    metadata = json.loads(read(f"https://registry.npmjs.org/@chanto%2fcli-linux-x64/{tag[1:]}"))
    url = metadata["dist"]["tarball"]
    parsed = urllib.parse.urlsplit(url)
    if parsed.scheme != "https" or parsed.netloc != "registry.npmjs.org":
        raise ValueError("unexpected npm tarball host")
    with urllib.request.urlopen(url, timeout=20) as response:
        archive = response.read(64 * 1024 * 1024 + 1)
    if len(archive) > 64 * 1024 * 1024:
        raise ValueError("npm tarball exceeds verification size limit")
    verify_payload(archive, metadata["dist"]["integrity"], expected.read_bytes())


def check(tag, checksums, fetch=read):
    version = tag[1:]
    failures = []

    def require(condition, label):
        if not condition:
            failures.append(label)

    def get(url):
        try:
            return fetch(url)
        except (urllib.error.URLError, TimeoutError) as error:
            failures.append(f"{url}: {error}")
            return None

    def document(url):
        body = get(url)
        if body is None:
            return None
        try:
            return json.loads(body)
        except ValueError:
            failures.append(f"{url}: invalid JSON")
            return None

    for suffix in ("", *("-" + platform for platform in PLATFORMS)):
        name = "@chanto/cli" + suffix
        metadata = document(f"https://registry.npmjs.org/@chanto%2fcli{suffix}/{version}")
        if metadata is not None:
            require(metadata.get("name") == name and metadata.get("version") == version,
                    f"npm {name}: version mismatch")
            require(bool(metadata.get("dist", {}).get("attestations")),
                    f"npm {name}: missing provenance metadata")
            if not suffix:
                dependencies = metadata.get("optionalDependencies", {})
                require(all(dependencies.get("@chanto/cli-" + p) == version for p in PLATFORMS),
                        "npm wrapper: platform dependency mismatch")
    if "-" not in version:
        latest = document("https://registry.npmjs.org/@chanto%2fcli/latest")
        if latest is not None:
            require(latest.get("version") == version, "npm latest has not advanced")
    crate = document(f"https://crates.io/api/v1/crates/chanto/{version}")
    if crate is not None:
        record = crate.get("version", {})
        require(record.get("num") == version and record.get("yanked") is False,
                "crates.io: version unavailable or yanked")
    formula = get("https://raw.githubusercontent.com/chanto-sh/homebrew-tap/HEAD/Formula/chanto.rb")
    if formula is not None:
        require(f'version "{version}"' in formula, "Homebrew version mismatch")
        for line in checksums.splitlines():
            digest, filename = line.split()
            if filename.endswith(".tar.gz"):
                require(digest in formula, f"Homebrew checksum mismatch: {filename}")
    token = document("https://ghcr.io/token?service=ghcr.io&scope=repository:chanto-sh/chanto:pull")
    if token is not None:
        require(bool(token.get("token")), "container is not anonymously readable")
    return failures


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tag")
    parser.add_argument("checksums", type=Path)
    parser.add_argument("--wait-seconds", type=int, default=600)
    parser.add_argument("--npm-binary", type=Path, help="compare npm's Linux payload with this signed binary")
    args = parser.parse_args()
    if not re.fullmatch(r"v\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?", args.tag):
        parser.error("expected vX.Y.Z")
    sums = args.checksums.read_text()
    deadline = time.monotonic() + args.wait_seconds
    while True:
        failures = check(args.tag, sums)
        if not failures:
            if args.npm_binary:
                verify_npm_binary(args.tag, args.npm_binary)
            print(f"{args.tag}: npm, crates.io, Homebrew and anonymous container access verified", flush=True)
            break
        print("\n".join(failures), flush=True)
        if time.monotonic() >= deadline:
            parser.exit(1, "public package verification failed\n")
        time.sleep(min(20, max(0, deadline - time.monotonic())))
