#!/bin/bash
# One small task against a real model: fix a bug so a test passes. Checks the
# outcome (the test passes, the test file is untouched), never the model's text.
#
#   tests/real-model/smoke.sh CHANTO BASE_URL MODEL [PROVIDER]
#
# BASE_URL - (and no PROVIDER) sends MODEL to one of Chanto's built-in hosted
# providers instead, as `openrouter/<model>` or `gemini/<model>`, with that
# provider's own key variable set (OPENROUTER_API_KEY, GEMINI_API_KEY, ...).
#
# CI serves Qwen3.5-4B with llama-server (see .github/workflows/real-model.yml);
# locally any OpenAI-compatible or Ollama server works. SMOKE_FLAGS adds
# Chanto flags (--no-think for a thinking model); SMOKE_PYTHON names Python
# where it isn't `python3` (Windows).
set -euo pipefail
PYTHON=${SMOKE_PYTHON:-python3}
BIN=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
BASE_URL=$2
MODEL=$3
PROVIDER=${4:-openai}
server=(--provider "$PROVIDER" --base-url "$BASE_URL")
[ "$BASE_URL" = - ] && server=()

W=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/chanto-smoke-XXXX")
mkdir -p "$W/home" "$W/repo"
cd "$W/repo"
cat > calc.py <<'EOF'
def add(a, b):
    """Return the sum of a and b."""
    return a - b
EOF
cat > test_calc.py <<'EOF'
from calc import add

assert add(2, 3) == 5, add(2, 3)
assert add(-1, 1) == 0, add(-1, 1)
print("ok")
EOF
git init -q && git add -A && git -c user.email=ci@example.com -c user.name=ci commit -qm init
test_sum=$(cksum < test_calc.py)

# --bare keeps the prompt to Chanto's own system prompt and Bash, Read and
# Edit (about 2k tokens), which a 4B model on four CPU cores reads in seconds.
limit=()
command -v timeout > /dev/null && limit=(timeout "${SMOKE_TIMEOUT:-900}")
status=0
# shellcheck disable=SC2086 # SMOKE_FLAGS is a list of flags
HOME="$W/home" CHANTO_HOME="$W/home/.chanto" CHANTO_NO_PRESENCE=1 \
  ${limit[@]+"${limit[@]}"} "$BIN" -p --bare \
  ${server[@]+"${server[@]}"} --model "$MODEL" \
  --num-ctx 16384 --max-turns 15 --mode bypassPermissions --no-session-persistence \
  ${SMOKE_FLAGS:-} \
  "$PYTHON test_calc.py fails. Fix the bug in calc.py so it passes, then run it to confirm." \
  > "$W/out.txt" 2> "$W/err.txt" || status=$?

echo "--- chanto exit $status; its reply:"
tail -c 2000 "$W/out.txt"
echo "--- stderr:"
tail -c 2000 "$W/err.txt"
echo "--- diff:"
git --no-pager diff

fail() { echo "::error::real-model smoke: $1"; exit 1; }
[ "$status" -eq 0 ] || fail "chanto exited $status"
[ "$(cksum < test_calc.py)" = "$test_sum" ] || fail "the test file was changed"
"$PYTHON" test_calc.py > /dev/null || fail "the test still fails"
echo "real-model smoke passed"
