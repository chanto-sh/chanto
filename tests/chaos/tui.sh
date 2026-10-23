#!/usr/bin/env bash
# Start Chanto's TUI in a private tmux server against the fake model.
#   tests/chaos/tui.sh start SCENARIO [WxH] [chanto args...]   -> prints W=<dir>
#   tests/chaos/tui.sh keys W <tmux send-keys args...>
#   tests/chaos/tui.sh shot W            (capture the pane)
#   tests/chaos/tui.sh resize W COLS ROWS
#   tests/chaos/tui.sh stop W
# Env as run.sh: PROTO, CONFIG, SETUP, TRUST, CTX, CHAOS_TMP, CHANTO_BIN.
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
T="tmux -L chanto-chaos"
cmd="$1"; shift
case "$cmd" in
start)
  scen="$1"; size="${2:-120x40}"; shift 2 || shift $#
  W="$(mktemp -d "${CHAOS_TMP:-${TMPDIR:-/tmp}}/tui.XXXXXX")"
  mkdir -p "$W/home" "$W/hh"
  [ -n "${CONFIG:-}" ] && printf '%s\n' "$CONFIG" > "$W/hh/config.toml"
  PROJ="${PROJ:-$W/proj}"; mkdir -p "$PROJ"
  [ -d "$PROJ/.git" ] || (cd "$PROJ" && git init -q && printf 'hello\n' > a.txt && git add . && git -c user.email=t@t -c user.name=t commit -qm init)
  [ -n "${SETUP:-}" ] && (cd "$PROJ" && W="$W" PROJ="$PROJ" HERE="$here" bash -c "$SETUP")
  [ -n "${TRUST:-}" ] && python3 -c 'import json,os,sys; print(json.dumps([os.path.realpath(sys.argv[1])]))' "$PROJ" > "$W/hh/trusted.json"
  FAKE_DUMP="$W/req" nohup python3 "$here/fakemodel.py" "$scen" --log "$W/model.log" ${CTX:+--ctx $CTX} > "$W/port" 2>"$W/server.err" &
  echo $! > "$W/server.pid"
  for _ in $(seq 50); do grep -q PORT "$W/port" 2>/dev/null && break; sleep 0.1; done
  port="$(awk '/PORT/{print $2}' "$W/port")"
  if [ "${PROTO:-openai}" = ollama ]; then base="http://127.0.0.1:$port"; else base="http://127.0.0.1:$port/v1"; fi
  bin="${CHANTO_BIN:-${CARGO_TARGET_DIR:-$repo/target}/debug/chanto}"
  name="$(basename "$W" | tr . _)"
  envs="HOME='$W/home' CHANTO_HOME='$W/hh' CHANTO_PROVIDER='${PROTO:-openai}' CHANTO_BASE_URL='$base' CHANTO_MODEL=fake CHANTO_NO_UPDATE_CHECK=1 CHANTO_NO_CLAUDE_CONFIG=1 CHANTO_NO_CODEX_CONFIG=1 XDG_CONFIG_HOME='$W/home/.config' TERM=xterm-256color"
  args=""; for a in "$@"; do args="$args '$a'"; done
  $T new-session -d -s "$name" -x "${size%x*}" -y "${size#*x}" "cd '$PROJ' && env $envs '$bin' $args; echo \"[chanto exited \$?]\"; sleep 3600"
  echo "$PROJ" > "$W/proj.path"
  echo "W=$W"
  ;;
keys) W="$1"; shift; $T send-keys -t "$(basename "$W" | tr . _)" "$@" ;;
shot) $T capture-pane -p -t "$(basename "$1" | tr . _)" ${2:+-S $2} ;;
pid) # Chanto's pid (the pane's shell runs it as a child)
  pp="$($T display -p -t "$(basename "$1" | tr . _)" '#{pane_pid}')"
  pgrep -P "$pp" | head -1 ;;
resize) $T resize-window -t "$(basename "$1" | tr . _)" -x "$2" -y "$3" ;;
stop)
  W="$1"; $T kill-session -t "$(basename "$W" | tr . _)" 2>/dev/null
  kill "$(cat "$W/server.pid")" 2>/dev/null
  [ -n "${KEEP:-}" ] || rm -rf "$W"
  ;;
esac
