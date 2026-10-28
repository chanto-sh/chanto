#!/bin/sh
# Lays out a release's Linux binaries as the container image's build context,
# after checking them the way the installers do: the release key's signature
# over SHA256SUMS first, then each archive against SHA256SUMS.
#
#   packaging/docker/prepare.sh <tag> <dist> <context>
#
# <dist> holds SHA256SUMS, SHA256SUMS.sig and <tag>'s two musl archives
# (chanto-<tag>-{x86_64,aarch64}-unknown-linux-musl.tar.gz). <context> gets
# bin/amd64/chanto and bin/arm64/chanto, named for Docker's TARGETARCH, and
# licenses/ with the archives' LICENSE, NOTICE and THIRD_PARTY_LICENSES.txt.
# Needs OpenSSL 3 for the Ed25519 check.
set -eu

[ $# -eq 3 ] || { echo "usage: $0 <tag> <dist> <context>" >&2; exit 2; }
tag=$1 dist=$2 ctx=$3
root=$(cd "$(dirname "$0")/../.." && pwd)

sig=$(mktemp)
trap 'rm -f "$sig"' EXIT
openssl base64 -d -A -in "$dist/SHA256SUMS.sig" -out "$sig"
openssl pkeyutl -verify -pubin -inkey "$root/packaging/release-signing-key.pub" -rawin \
    -in "$dist/SHA256SUMS" -sigfile "$sig"

for pair in amd64:x86_64-unknown-linux-musl arm64:aarch64-unknown-linux-musl; do
    arch=${pair%%:*}
    name="chanto-$tag-${pair#*:}"
    line=$(awk -v f="$name.tar.gz" '$2 == f' "$dist/SHA256SUMS")
    if [ -z "$line" ]; then
        echo "error: $name.tar.gz isn't in SHA256SUMS" >&2
        exit 1
    fi
    (cd "$dist" && printf '%s\n' "$line" | sha256sum --check --strict -)
    rm -rf "${ctx:?}/bin/$arch" "${dist:?}/$name"
    mkdir -p "$ctx/bin/$arch"
    tar -xzf "$dist/$name.tar.gz" -C "$dist"
    install -m 755 "$dist/$name/chanto" "$ctx/bin/$arch/chanto"
done
# The license notices from the (identical) amd64 archive. Releases before
# v0.1.8 shipped none; the pull request check builds from those, so it takes
# the repository's own LICENSE and NOTICE instead.
rm -rf "${ctx:?}/licenses"
mkdir -p "$ctx/licenses"
notices="$dist/chanto-$tag-x86_64-unknown-linux-musl"
if [ -f "$notices/THIRD_PARTY_LICENSES.txt" ]; then
    cp "$notices/LICENSE" "$notices/NOTICE" "$notices/THIRD_PARTY_LICENSES.txt" "$ctx/licenses/"
else
    cp "$root/LICENSE" "$root/NOTICE" "$ctx/licenses/"
fi
