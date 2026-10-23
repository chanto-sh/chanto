#!/usr/bin/env python3
"""Render a tap formula from the checksums published with a release."""
import pathlib
import re
import sys

version, sums, output = sys.argv[1:]
if not re.fullmatch(r"v\d+\.\d+\.\d+", version):
    raise SystemExit("expected vX.Y.Z")
checksums = dict(line.split()[::-1] for line in pathlib.Path(sums).read_text().splitlines())
formula = pathlib.Path(__file__).with_name("chanto.rb").read_text().replace("@VERSION@", version[1:])
for target in ("aarch64-apple-darwin", "x86_64-apple-darwin", "aarch64-unknown-linux-musl", "x86_64-unknown-linux-musl"):
    digest = checksums[f"chanto-{version}-{target}.tar.gz"]
    if not re.fullmatch(r"[a-f0-9]{64}", digest):
        raise SystemExit("invalid SHA256")
    formula = formula.replace("@SHA_" + target.upper().replace("-", "_") + "@", digest)
pathlib.Path(output).write_text(formula)
