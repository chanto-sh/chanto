#!/usr/bin/env bash
# Install the previous release through $CHANNEL, swap in this build, run
# `chanto upgrade`, and check the latest release is what's left. Run by
# .github/workflows/upgrade-channels.yml; it changes the machine it runs on.
set -euo pipefail
head="$PWD/target/debug/chanto"
[ "${RUNNER_OS:-}" = Windows ] && head="$head.exe"
tags=$(gh release list --limit 2 --exclude-drafts --exclude-pre-releases --json tagName --jq '.[].tagName')
latest=$(sed -n 1p <<<"$tags")
prev=$(sed -n 2p <<<"$tags")
echo "upgrading $prev -> $latest"

check() { # binary
  got=$("$1" --version)
  [ "$got" = "chanto ${latest#v}" ] || { echo "::error::$CHANNEL: $1 is '$got', expected $latest"; exit 1; }
  ! cmp -s "$1" "$head" || { echo "::error::$CHANNEL: $1 is still this build: nothing was installed"; exit 1; }
  echo "$CHANNEL: $got"
}

case "$CHANNEL" in
installer)
  prefix="$RUNNER_TEMP/chanto"
  if [ "$RUNNER_OS" = Windows ]; then
    powershell -NoProfile -File install.ps1 -Version "$prev" -Prefix "$(cygpath -w "$prefix")"
    bin="$prefix/bin/chanto.exe"
  else
    sh install.sh --version "$prev" --prefix "$prefix"
    bin="$prefix/bin/chanto"
  fi
  "$bin" --version
  cp "$head" "$bin"
  "$bin" upgrade --yes --force
  check "$bin"
  ;;
homebrew)
  export HOMEBREW_NO_INSTALL_FROM_API=1 HOMEBREW_NO_ENV_HINTS=1
  brew tap chanto-sh/tap
  tap=$(brew --repo chanto-sh/tap)
  # The tap clone as a user who last updated at the previous release has it.
  old=""
  for c in $(git -C "$tap" log --format=%H -- Formula/chanto.rb); do
    if git -C "$tap" show "$c:Formula/chanto.rb" | grep -q "version \"${prev#v}\""; then old=$c; break; fi
  done
  [ -n "$old" ] || { echo "::error::no formula for $prev in the tap"; exit 1; }
  git -C "$tap" reset -q --hard "$old"
  HOMEBREW_NO_AUTO_UPDATE=1 brew install chanto-sh/tap/chanto
  bin="$(brew --prefix chanto)/bin/chanto"
  "$bin" --version
  # Into the Cellar, so this build detects Homebrew as a real install does.
  real=$(python3 -c 'import os,sys;print(os.path.realpath(sys.argv[1]))' "$bin")
  chmod u+w "$real"
  cp "$head" "$real"
  "$bin" upgrade --yes --force
  check "$(brew --prefix chanto)/bin/chanto"
  ;;
npm)
  # Where npm puts the binary; `upgrade` must recognise it and give npm's command.
  # Not a real install: that would be an unpinned dependency, and release.yml's
  # verify job already installs and checks the published package.
  bin="$RUNNER_TEMP/lib/node_modules/@chanto/cli-linux-x64/bin/chanto"
  mkdir -p "$(dirname "$bin")"
  cp "$head" "$bin"
  out=$("$bin" upgrade --force)
  echo "$out"
  grep -q 'npm install -g @chanto/cli@latest' <<<"$out" || { echo "::error::npm: upgrade doesn't give the npm command"; exit 1; }
  ! grep -q 'install.sh' <<<"$out" || { echo "::error::npm: upgrade offers the install script"; exit 1; }
  echo "npm: gives the npm command"
  ;;
*) echo "unknown channel $CHANNEL"; exit 2 ;;
esac
