#!/usr/bin/env bash
# Run Chanto against the fake model in a throwaway HOME/CHANTO_HOME/project.
#
#   tests/chaos/run.sh SCENARIO [chanto args...]
#
# Env:
#   CHANTO_BIN   binary (default: $CARGO_TARGET_DIR or ./target, debug/chanto)
#   PROTO         openai (default) or ollama
#   PROJ          project dir to run in (default: a fresh temp dir with a git repo)
#   KEEP=1        keep the temp dirs (printed on stderr)
#   CHAOS_TMP     where temp dirs go (default: $TMPDIR)
#   HH            reuse this CHANTO_HOME (e.g. a TUI run's, to resume it)
#   CONFIG        text for $CHANTO_HOME/config.toml
#   TIMEOUT       seconds before Chanto is killed (default 60)
# The fake server's request log is at $W/model.log; exit code is Chanto's.
set -u
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
bin="${CHANTO_BIN:-${CARGO_TARGET_DIR:-$repo/target}/debug/chanto}"
scen="$1"; shift
W="$(mktemp -d "${CHAOS_TMP:-${TMPDIR:-/tmp}}/chaos.XXXXXX")"
mkdir -p "$W/home" "$W/hh"
# CONFIG: extra config.toml text, e.g. CONFIG='mode = "acceptEdits"'
[ -n "${CONFIG:-}" ] && printf '%s\n' "$CONFIG" > "$W/hh/config.toml"
if [ -z "${PROJ:-}" ]; then
  PROJ="$W/proj"; mkdir -p "$PROJ"
  (cd "$PROJ" && git init -q && printf 'hello\n' > a.txt && git add . && git -c user.email=t@t -c user.name=t commit -qm init)
fi
# SETUP: shell code run in the project first (W, PROJ, HERE are set)
[ -n "${SETUP:-}" ] && (cd "$PROJ" && W="$W" PROJ="$PROJ" HERE="$here" bash -c "$SETUP")
# TRUST=1: trust the project folder
[ -n "${TRUST:-}" ] && python3 -c 'import json,os,sys; print(json.dumps([os.path.realpath(sys.argv[1])]))' "$PROJ" > "$W/hh/trusted.json"
FAKE_DUMP="$W/req" python3 "$here/fakemodel.py" "$scen" --log "$W/model.log" ${CTX:+--ctx $CTX} > "$W/port" 2>"$W/server.err" &
spid=$!
for _ in $(seq 50); do grep -q PORT "$W/port" 2>/dev/null && break; sleep 0.1; done
port="$(awk '/PORT/{print $2}' "$W/port")"
if [ "${PROTO:-openai}" = ollama ]; then base="http://127.0.0.1:$port"; else base="http://127.0.0.1:$port/v1"; fi
export HOME="$W/home" CHANTO_HOME="${HH:-$W/hh}" CHANTO_PROVIDER="${PROTO:-openai}" CHANTO_BASE_URL="$base" CHANTO_MODEL=fake \
  CHANTO_NO_UPDATE_CHECK=1 CHANTO_NO_CLAUDE_CONFIG=1 CHANTO_NO_CODEX_CONFIG=1 CI=1 XDG_CONFIG_HOME="$W/home/.config"
echo "W=$W PROJ=$PROJ port=$port" >&2
cd "$PROJ"
perl -e 'alarm shift; exec @ARGV or die' "${TIMEOUT:-60}" "$bin" "$@"
rc=$?
kill $spid 2>/dev/null; wait $spid 2>/dev/null
echo "[exit $rc]" >&2
[ -n "${KEEP:-}" ] || rm -rf "$W"
exit $rc
