#!/bin/sh
# Builds the image for each platform from <context> (see prepare.sh), loads
# it and checks that it runs: the version, the tools it ships, the non-root
# user and the working directory. Needs buildx and, for the other
# architecture, QEMU.
#
#   packaging/docker/test.sh <context> <version>
set -eu

[ $# -eq 2 ] || { echo "usage: $0 <context> <version>" >&2; exit 2; }
ctx=$1 version=$2
dockerfile="$(cd "$(dirname "$0")" && pwd)/Dockerfile"

for arch in amd64 arm64; do
    image="chanto-test:$arch"
    echo "== linux/$arch" >&2
    docker buildx build --platform "linux/$arch" --load --tag "$image" \
        --build-arg VERSION="$version" --file "$dockerfile" "$ctx"
    docker run --rm --platform "linux/$arch" "$image" --version | tee /dev/stderr | grep -qF "$version"
    docker run --rm --platform "linux/$arch" --entrypoint bash "$image" -euc '
        git --version
        rg --version | head -1
        test "$(id -u)" != 0 || { echo "error: runs as root" >&2; exit 1; }
        test "$(pwd)" = /work || { echo "error: workdir is $(pwd)" >&2; exit 1; }
        test -w /work || { echo "error: /work isn'"'"'t writable" >&2; exit 1; }
    '
done
