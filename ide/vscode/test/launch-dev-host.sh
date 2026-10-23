#!/bin/sh
# Launch an isolated VS Code (throwaway profile) with the extension loaded
# from source. Usage: test/launch-dev-host.sh <workspace> [chanto-home]
set -e
here=$(cd "$(dirname "$0")/.." && pwd)
ws=${1:?workspace folder}
home=${2:-$(mktemp -d)}
scratch=$(mktemp -d)
code="/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code"
command -v code >/dev/null 2>&1 && code=$(command -v code)
CHANTO_HOME="$home" "$code" --new-window --user-data-dir "$scratch/user" --extensions-dir "$scratch/ext" \
  --extensionDevelopmentPath="$here" "$ws"
echo "CHANTO_HOME=$home"
