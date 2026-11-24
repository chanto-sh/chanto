#!/usr/bin/env bash
# Publish the GitHub Action to chanto-sh/chanto-action, where action.yml sits at
# the repository root as GitHub Marketplace requires:
#
#   packaging/publish-action.sh <chanto tag> <action version> [--push]
#   packaging/publish-action.sh v0.1.17 v1.1.0 --push
#
# Copies action/ and LICENSE as they are at the Chanto release tag, commits
# them, tags the action version and moves its major tag (v1). Without --push it
# stops before pushing and prints where the prepared checkout is. An action
# version that's already tagged, or a copy that changes nothing, is refused.
set -euo pipefail

usage() { echo "usage: $0 <chanto tag vX.Y.Z> <action version vA.B.C> [--push]" >&2; exit 2; }
[ $# -ge 2 ] || usage
release=$1 version=$2 push=${3:-}
[[ $release =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || usage
[[ $version =~ ^v([0-9]+)\.[0-9]+\.[0-9]+$ ]] || usage
major="v${BASH_REMATCH[1]}"
[ -z "$push" ] || [ "$push" = --push ] || usage

root=$(git rev-parse --show-toplevel)
git -C "$root" rev-parse -q --verify "refs/tags/$release^{commit}" >/dev/null \
    || { echo "error: no tag $release here; fetch tags first" >&2; exit 1; }

work=$(mktemp -d)
git clone -q https://github.com/chanto-sh/chanto-action.git "$work"
cd "$work"
if git rev-parse -q --verify "refs/tags/$version" >/dev/null; then
    echo "error: chanto-sh/chanto-action already has $version" >&2
    exit 1
fi

# Replace everything but .git with the release's action/ and LICENSE.
find . -mindepth 1 -maxdepth 1 ! -name .git -exec rm -rf {} +
git -C "$root" archive "$release" action | tar -x --strip-components=1 -C .
git -C "$root" show "$release:LICENSE" > LICENSE
git add -A
if git diff --cached --quiet; then
    echo "error: action/ at $release is what chanto-action already has; nothing to publish" >&2
    exit 1
fi
git commit -q -s -m "feat: publish the Chanto action from chanto $release"
git tag -a "$version" -m "$version: from chanto $release"
git tag -f -a "$major" -m "$major: latest $major.x.y ($version)" >/dev/null

if [ "$push" = --push ]; then
    git push -q origin main "$version"
    git push -q -f origin "$major"
    echo "Published $version (and moved $major). Next: gh release create $version -R chanto-sh/chanto-action, then tick \"Publish to Marketplace\" on its page."
else
    echo "Prepared $version in $work (not pushed). Check it, then run again with --push."
fi
