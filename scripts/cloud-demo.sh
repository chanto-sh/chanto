#!/usr/bin/env bash
# Apply the quality and security agents in .chanto/agents/ to Chanto cloud, and
# optionally start one. Needs `chanto cloud login`; runs are placed on
# self-hosted runners with this org's own model keys, so code stays on the
# machine that runs it and the model key is the one in your `chanto` login.
#
#   scripts/cloud-demo.sh                  create or update every agent
#   scripts/cloud-demo.sh --run review     ...and start it, waiting for it
#   scripts/cloud-demo.sh --run review --input "look at PR #123"
#   scripts/cloud-demo.sh --runner         ...then take runs on this machine
#
# The agents ask for the `gha` label, which GitHub Actions offers for free
# (.github/workflows/quality-runner.yml). `--runner` takes them here instead.
# Either way the runner needs a registration token from the dashboard's
# Runners page (or CHANTO_RUNNER_REGISTRATION_TOKEN). `--dry-run` only prints.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
chanto=${CHANTO_BIN:-chanto}
run=""
input=""
with_runner=0
dry=0
url=${CHANTO_CLOUD_URL:-https://api.chanto.sh}

usage() { sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'; }

while [ $# -gt 0 ]; do
  case "$1" in
    --run) run=${2:?--run needs an agent name}; shift 2 ;;
    --input) input=${2:?--input needs text}; shift 2 ;;
    --runner) with_runner=1; shift ;;
    --dry-run) dry=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

command -v "$chanto" >/dev/null || { echo "chanto not on PATH (or set CHANTO_BIN)" >&2; exit 1; }

# Checked before the sign-in so a typo is caught without a cloud account (the
# server rejects unknown fields, but only after the upload). The file name is
# the agent name: `chanto agents edit` takes it separately.
check_spec() {
  local file=$1 base=$2
  if python3 -c "import tomllib" 2>/dev/null; then
    python3 - "$file" "$base" <<'PY'
import sys, tomllib
path, base = sys.argv[1], sys.argv[2]
keys = {"name", "description", "instructions", "repo", "git_ref", "placement",
        "labels", "schedule", "triggers", "enabled", "model_keys",
        "gateway_model", "script", "model_runs", "checks"}
triggers = {"issue_opened", "issue_labeled", "pull_request_opened",
            "pull_request_updated", "mention"}
with open(path, "rb") as f:
    spec = tomllib.load(f)
name = spec.get("name", "")
if name != base:
    sys.exit(f'{path}: name "{name}" must match its file name ("{base}")')
unknown = set(spec) - keys
if unknown:
    sys.exit(f"{path}: not an agent field: {', '.join(sorted(unknown))}")
for field in ("instructions", "repo"):
    if not spec.get(field):
        sys.exit(f"{path}: {field} is required")
if spec.get("placement") not in (None, "auto", "hosted", "self-hosted"):
    sys.exit(f'{path}: placement "{spec["placement"]}" is not auto, hosted or self-hosted')
if spec.get("model_keys") not in (None, "own", "hosted", "endpoint"):
    sys.exit(f'{path}: model_keys "{spec["model_keys"]}" is not own, hosted or endpoint')
for trigger in spec.get("triggers", []):
    if trigger.get("on") not in triggers:
        sys.exit(f'{path}: trigger "{trigger.get("on")}" is not one of {", ".join(sorted(triggers))}')
PY
  else
    local name
    name=$(sed -n 's/^name *= *"\([^"]*\)".*/\1/p' "$file" | head -1)
    [ -n "$name" ] || { echo "$file: no name" >&2; exit 1; }
    [ "$name" = "$base" ] || { echo "$file: name \"$name\" must match its file name" >&2; exit 1; }
  fi
}

# `cloud status` exits 0 either way, and prints the sign-up blurb under the
# first line: read that line. `--dry-run` prints without signing in.
if [ "$dry" = 0 ]; then
  status=$("$chanto" cloud status 2>&1 || true)
  case "$status" in
    "Not signed in"*)
      echo "not signed in to Chanto cloud: run \`chanto cloud login\` first" >&2
      exit 1 ;;
  esac
fi

apply() {
  local file=$1 name=$2
  if "$chanto" agents show "$name" >/dev/null 2>&1; then
    echo "update $name"
    [ "$dry" = 1 ] || "$chanto" agents edit "$name" --file "$file"
  else
    echo "create $name"
    [ "$dry" = 1 ] || "$chanto" agents create --file "$file"
  fi
}

applied=0
files=()
for file in "$root"/.chanto/agents/*.toml; do
  [ -e "$file" ] || { echo "no agent files in .chanto/agents/" >&2; exit 1; }
  base=$(basename "$file" .toml)
  check_spec "$file" "$base"
  files+=("$file")
done

for file in "${files[@]}"; do
  name=$(basename "$file" .toml)
  apply "$file" "$name"
  applied=$((applied + 1))
done
echo "$applied agent(s) in the current org; see \`chanto agents runs <name>\`."

if [ "$with_runner" = 1 ]; then
  echo "taking runs on this machine (ctrl-c to stop):"
  exec "$chanto" runner connect "$url" --own-model --name "$(hostname -s)" \
    --label gha --mode bypassPermissions
fi

if [ -z "$run" ]; then
  cat <<'EOF'

Runs are placed on self-hosted runners. GitHub Actions offers one for free in a
public repository: `.github/workflows/quality-runner.yml` keeps it for 24 minutes
at a time. For that, register a runner once with the token from the dashboard's
Runners page, and give the jobs its saved sign-in:

  chanto runner connect https://api.chanto.sh --registration-token hrr_... \
    --label gha --name gha
  gh secret set CHANTO_RUNNER_CONNECTIONS < ~/.chanto/runner/connections.json

The workflow thinks with OpenCode Go (`CHANTO_MODEL`); set the repository's
OPENCODE_GO_API_KEY secret to a Console key from that subscription.

Or take them on this machine (`--runner`). To keep taking them after the
terminal closes, on a laptop or a VPS, install it as a service instead:

  scripts/runner-service.sh install --label local --own-model

Then start an agent by hand, from a terminal or the dashboard:

  scripts/cloud-demo.sh --run review --input "look at PR #123"
EOF
  exit 0
fi

args=(agents run "$run" --wait)
[ -n "$input" ] && args+=(--input "$input")
exec "$chanto" "${args[@]}"
