#!/usr/bin/env python3
"""Sign a file with the release key as an SSH signature (SSHSIG).

Usage: sshsig.py <ed25519-private-key.pem> <file> [openssl] > <file>.sshsig
       sshsig.py --allowed-signers <ed25519-key.pem> [openssl]   (private or public)

The release key is an Ed25519 PEM key used with `openssl pkeyutl -rawin` for
SHA256SUMS.sig. Stock macOS (LibreSSL) and Windows have no OpenSSL that can
verify that, but they do ship `ssh-keygen -Y verify`, so each release also
carries the same key's signature in the SSHSIG format (OpenSSH's
PROTOCOL.sshsig). Signing still goes through OpenSSL; this script only builds
the signed blob and the armored output, so it needs no Python packages.
Verify with packaging/release-allowed-signers:

  ssh-keygen -Y verify -f release-allowed-signers -I chanto-release \
    -n chanto-release -s SHA256SUMS.sshsig < SHA256SUMS
"""

import base64
import hashlib
import os
import struct
import subprocess
import sys
import tempfile

NAMESPACE = b"chanto-release"
HASH = b"sha512"


def string(data: bytes) -> bytes:
    return struct.pack(">I", len(data)) + data


def public_blob(key: str, openssl: str) -> bytes:
    """The SSH wire form of the Ed25519 key's public half (`key` is PEM)."""
    with open(key, "rb") as f:
        pubin = b"BEGIN PUBLIC KEY" in f.read()
    der = subprocess.run(
        [openssl, "pkey", *(["-pubin"] if pubin else []), "-in", key, "-pubout", "-outform", "DER"],
        check=True,
        capture_output=True,
    ).stdout
    # An Ed25519 SubjectPublicKeyInfo is a fixed 12-byte header and the key.
    if len(der) != 44 or not der.startswith(bytes.fromhex("302a300506032b6570032100")):
        sys.exit("sshsig: not an Ed25519 key")
    return string(b"ssh-ed25519") + string(der[12:])


def main() -> int:
    args = sys.argv[1:]
    if args[:1] == ["--allowed-signers"] and len(args) in (2, 3):
        # The line for an allowed_signers file (ssh-keygen -Y verify -f).
        public = public_blob(args[1], args[2] if len(args) == 3 else "openssl")
        print("chanto-release ssh-ed25519 " + base64.b64encode(public).decode())
        return 0
    if len(args) not in (2, 3):
        print("\n".join(__doc__.strip().splitlines()[2:4]), file=sys.stderr)
        return 2
    key, path = args[0], args[1]
    openssl = args[2] if len(args) == 3 else "openssl"
    public = public_blob(key, openssl)
    with open(path, "rb") as f:
        digest = hashlib.sha512(f.read()).digest()
    signed = b"SSHSIG" + string(NAMESPACE) + string(b"") + string(HASH) + string(digest)
    fd, blob = tempfile.mkstemp()
    try:
        with os.fdopen(fd, "wb") as f:
            f.write(signed)
        raw = subprocess.run(
            [openssl, "pkeyutl", "-sign", "-inkey", key, "-rawin", "-in", blob],
            check=True,
            capture_output=True,
        ).stdout
    finally:
        os.unlink(blob)
    if len(raw) != 64:
        print("sshsig: unexpected signature length", file=sys.stderr)
        return 1
    sig = (
        b"SSHSIG"
        + struct.pack(">I", 1)
        + string(public)
        + string(NAMESPACE)
        + string(b"")
        + string(HASH)
        + string(string(b"ssh-ed25519") + string(raw))
    )
    armored = base64.b64encode(sig).decode()
    lines = [armored[i : i + 70] for i in range(0, len(armored), 70)]
    sys.stdout.write("-----BEGIN SSH SIGNATURE-----\n" + "\n".join(lines) + "\n-----END SSH SIGNATURE-----\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
