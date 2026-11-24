#!/usr/bin/env bash
# Keep this machine connected to a Chanto cloud run queue, so it takes runs with
# no terminal open: a launchd agent on macOS, a systemd user unit on Linux.
#
#   scripts/runner-service.sh install --label local --own-model
#   scripts/runner-service.sh status
#   scripts/runner-service.sh logs [-f]
#   scripts/runner-service.sh restart
#   scripts/runner-service.sh uninstall
#
# The first install registers the machine: pass --registration-token hrr_... or
# set CHANTO_RUNNER_REGISTRATION_TOKEN (the token is in the dashboard's Runners
# page). Registration is saved in ~/.chanto/runner/connections.json, so the
# service starts without it. `sudo` isn't needed or wanted: both a launchd agent
# and a systemd *user* unit run as you.
#
# The service runs the runner in a login shell, so the model key and PATH your
# shell sets up are there, as they are in a terminal. Pass --own-model to use
# that model configuration; without it, the server's model endpoint is used when
# it offers one. `--dry-run` prints the service definition instead of writing it.
#
# A runner's labels are fixed when it registers. Installing again with other
# --label values reuses the saved registration, so register once more with a
# fresh token (and remove the old runner in the dashboard) to change them.
set -euo pipefail

chanto=${CHANTO_BIN:-}
url=${CHANTO_CLOUD_URL:-https://api.chanto.sh}
name=${CHANTO_RUNNER_NAME:-$(hostname -s 2>/dev/null || hostname)}
labels=()
own_model=0
allow_scripts=0
mode=""
registration_token=${CHANTO_RUNNER_REGISTRATION_TOKEN:-}
dry=0
follow=0
label=sh.chanto.runner

usage() { sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; }

die() { echo "$*" >&2; exit 1; }

os=$(uname -s)
case "$os" in
  Darwin) unit="$HOME/Library/LaunchAgents/$label.plist" ;;
  Linux) unit="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/chanto-runner.service" ;;
  *) die "this script installs a macOS launchd agent or a Linux systemd user unit; on $os run 'chanto runner connect' under your own supervisor" ;;
esac
home=${CHANTO_HOME:-$HOME/.chanto}
log="$home/runner/service.log"

cmd=${1:-}
[ $# -gt 0 ] && shift

case "$cmd" in
  install|status|logs|restart|uninstall) ;;
  -h|--help|help|"") usage; exit 0 ;;
  *) echo "unknown command: $cmd" >&2; usage >&2; exit 2 ;;
esac

while [ $# -gt 0 ]; do
  case "$1" in
    --label) labels+=("${2:?--label needs a value}"); shift 2 ;;
    --name) name=${2:?--name needs a value}; shift 2 ;;
    --url) url=${2:?--url needs a value}; shift 2 ;;
    --mode) mode=${2:?--mode needs a value}; shift 2 ;;
    --registration-token) registration_token=${2:?--registration-token needs a value}; shift 2 ;;
    --own-model) own_model=1; shift ;;
    --allow-scripts) allow_scripts=1; shift ;;
    --dry-run) dry=1; shift ;;
    -f|--follow) follow=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

[ ${#labels[@]} -gt 0 ] || labels=(local)
url=${url%/}
chanto=${chanto:-$(command -v chanto || true)}
[ -n "$chanto" ] || die "chanto not on PATH (or set CHANTO_BIN)"
chanto=$(cd "$(dirname "$chanto")" && pwd)/$(basename "$chanto")

# The command the service runs. The login shell puts the model key and PATH the
# user's terminal has in place, since launchd and systemd start with a bare one.
runner_args=("$chanto" runner connect "$url")
for l in "${labels[@]}"; do runner_args+=(--label "$l"); done
runner_args+=(--name "$name")
[ "$own_model" = 1 ] && runner_args+=(--own-model)
[ "$allow_scripts" = 1 ] && runner_args+=(--allow-scripts)
[ -n "$mode" ] && runner_args+=(--mode "$mode")
service_cmd=$(printf '%q ' "${runner_args[@]}")
service_cmd=${service_cmd% }
# A relocated home has to travel with the service too.
[ -n "${CHANTO_HOME:-}" ] && service_cmd="CHANTO_HOME=$CHANTO_HOME $service_cmd"
shell=${SHELL:-/bin/sh}

plist() {
  cat <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$label</string>
  <key>ProgramArguments</key>
  <array>
    <string>$shell</string><string>-lic</string><string>exec $service_cmd</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>30</integer>
  <key>StandardOutPath</key><string>$log</string>
  <key>StandardErrorPath</key><string>$log</string>
</dict>
</plist>
EOF
}

systemd_unit() {
  cat <<EOF
[Unit]
Description=Chanto cloud runner
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=$shell -lic 'exec $service_cmd'
Restart=always
RestartSec=30
StandardOutput=append:$log
StandardError=append:$log

[Install]
WantedBy=default.target
EOF
}

register_if_needed() {
  # A saved token means this machine is already registered; re-registering would
  # leave the old runner behind as a duplicate.
  local saved="$home/runner/connections.json"
  if python3 - "$saved" "$url" <<'EOF' 2>/dev/null
import json, sys
try:
    servers = json.load(open(sys.argv[1])).get("servers", [])
except Exception:
    raise SystemExit(1)
raise SystemExit(0 if any(s.get("url") == sys.argv[2] for s in servers) else 1)
EOF
  then
    return 0
  fi
  [ -n "$registration_token" ] || die "this machine isn't registered with $url: pass --registration-token hrr_... (the dashboard's Runners page) or set CHANTO_RUNNER_REGISTRATION_TOKEN"
  echo "Registering $name with $url (labels: ${labels[*]})"
  local args=("$chanto" runner connect "$url" --registration-token "$registration_token" --name "$name" --register-only)
  for l in "${labels[@]}"; do args+=(--label "$l"); done
  "${args[@]}"
}

case "$cmd" in
  install)
    if [ "$dry" != 1 ] && ! "$chanto" runner connect --help 2>&1 | grep -q -- --register-only; then
      die "this chanto doesn't know --register-only: upgrade it (https://chanto.sh/install)"
    fi
    if [ "$os" = Darwin ]; then
      content=$(plist)
    else
      content=$(systemd_unit)
    fi
    if [ "$dry" = 1 ]; then
      echo "$content"
      exit 0
    fi
    register_if_needed
    mkdir -p "$(dirname "$unit")" "$(dirname "$log")"
    printf '%s\n' "$content" > "$unit"
    chmod 600 "$unit"
    if [ "$os" = Darwin ]; then
      launchctl bootout "gui/$(id -u)/$label" 2>/dev/null || true
      launchctl bootstrap "gui/$(id -u)" "$unit"
      launchctl enable "gui/$(id -u)/$label"
    else
      systemctl --user daemon-reload
      systemctl --user enable --now chanto-runner.service
    fi
    echo "Installed $unit"
    echo "Logs: $log (scripts/runner-service.sh logs -f)"
    ;;
  status)
    if [ "$os" = Darwin ]; then
      launchctl print "gui/$(id -u)/$label" 2>/dev/null | grep -E '^\s*(state|pid|path) =' || echo "not installed ($unit)"
    else
      systemctl --user status chanto-runner.service --no-pager 2>&1 | head -12 || echo "not installed ($unit)"
    fi
    ;;
  logs)
    [ -f "$log" ] || { echo "no log yet at $log" >&2; exit 1; }
    if [ "$follow" = 1 ]; then tail -f "$log"; else tail -n 50 "$log"; fi
    ;;
  restart)
    if [ "$os" = Darwin ]; then
      launchctl kickstart -k "gui/$(id -u)/$label"
    else
      systemctl --user restart chanto-runner.service
    fi
    ;;
  uninstall)
    if [ "$os" = Darwin ]; then
      launchctl bootout "gui/$(id -u)/$label" 2>/dev/null || true
    else
      systemctl --user disable --now chanto-runner.service 2>/dev/null || true
    fi
    rm -f "$unit"
    [ "$os" = Linux ] && systemctl --user daemon-reload || true
    echo "Removed $unit (the runner registration stays; it shows offline in the dashboard)"
    ;;
esac
