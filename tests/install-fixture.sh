#!/bin/sh
# Build the release fixture the installer tests serve: <dir>/releases holds
# signed releases v9.8.7 (also as latest) and v0.1.3 (below the installers'
# rollback floor), signed like the release workflow does (SHA256SUMS.sig for
# OpenSSL, SHA256SUMS.sshsig for ssh-keygen) with a throwaway key:
# <dir>/test-release.pem, its public half test-release.pub and, for
# CHANTO_RELEASE_SSH_KEY, test-release.ssh. <dir>/openssl names the OpenSSL
# used. Used by tests/install.sh and the Windows CI job.
# Usage: install-fixture.sh <dir>   (PYTHON overrides python3)
set -eu
root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
dir=$1
python=${PYTHON:-python3}
ossl=
for o in openssl /opt/homebrew/opt/openssl@3/bin/openssl /usr/local/opt/openssl@3/bin/openssl; do
    command -v "$o" >/dev/null 2>&1 || continue
    case "$("$o" pkeyutl -help 2>&1)" in *-rawin*) ossl=$o; break ;; esac
done
[ -n "$ossl" ] || { echo 'the installer tests need OpenSSL 3 (macOS: brew install openssl@3)' >&2; exit 1; }
printf '%s\n' "$ossl" > "$dir/openssl"
if command -v sha256sum >/dev/null 2>&1; then sha() { sha256sum -b "$@"; }; else sha() { shasum -a 256 -b "$@"; }; fi
"$ossl" genpkey -algorithm ed25519 -out "$dir/test-release.pem" 2>/dev/null
"$ossl" pkey -in "$dir/test-release.pem" -pubout -out "$dir/test-release.pub"
"$python" "$root/packaging/sshsig.py" --allowed-signers "$dir/test-release.pem" "$ossl" | cut -d' ' -f2- > "$dir/test-release.ssh"
for version in v9.8.7 v0.1.3; do
    out=$dir/releases/download/$version
    mkdir -p "$out"
    for target in aarch64-apple-darwin x86_64-apple-darwin aarch64-unknown-linux-musl x86_64-unknown-linux-musl; do
        name=chanto-$version-$target
        mkdir -p "$dir/package/$name"
        printf '#!/bin/sh\necho %s\n' "$target" > "$dir/package/$name/chanto"
        tar -czf "$out/$name.tar.gz" -C "$dir/package" "$name"
    done
    "$python" -c 'import sys, zipfile; zipfile.ZipFile(sys.argv[1], "w").writestr(sys.argv[2] + "/chanto.exe", sys.argv[3])' \
        "$out/chanto-$version-x86_64-pc-windows-msvc.zip" "chanto-$version-x86_64-pc-windows-msvc" "windows $version"
    (cd "$out" && sha ./*.tar.gz ./*.zip | sed 's| [*]\./|  |' > SHA256SUMS)
    "$ossl" pkeyutl -sign -inkey "$dir/test-release.pem" -rawin -in "$out/SHA256SUMS" | "$ossl" base64 -A > "$out/SHA256SUMS.sig"
    "$python" "$root/packaging/sshsig.py" "$dir/test-release.pem" "$out/SHA256SUMS" "$ossl" > "$out/SHA256SUMS.sshsig"
done
mkdir -p "$dir/releases/latest/download"
cp "$dir/releases/download/v9.8.7/SHA256SUMS"* "$dir/releases/latest/download/"
