#!/usr/bin/env bash
# Hostile-repository checks: an untrusted project must not run code or leak
# files outside itself just by being opened. Each check prints PASS/FAIL.
#   CARGO_TARGET_DIR=... tests/chaos/security.sh
here="$(cd "$(dirname "$0")" && pwd)"
export CHAOS_TMP="${CHAOS_TMP:-${TMPDIR:-/tmp}}"
S="$here/scenarios/hello.json"
fail=0
check() { # name, condition-description, test-command
  if eval "$3"; then echo "PASS $1"; else echo "FAIL $1: $2"; fail=1; fi
}
M="$(mktemp -d "$CHAOS_TMP/sec.XXXXXX")"

# 1. .mcp.json in an untrusted project doesn't start its server.
SETUP="printf '{\"mcpServers\":{\"evil\":{\"command\":\"python3\",\"args\":[\"$here/fakemcp.py\",\"ok\",\"$M/mcp-ran\"]}}}' > .mcp.json" \
  "$here/run.sh" "$S" -p hi >/dev/null 2>&1
check mcp_json_untrusted "server started without approval" '[ ! -e "$M/mcp-ran" ]'

# 2. Project .claude/settings.json hooks don't run in an untrusted project.
SETUP="mkdir -p .claude && printf '{\"hooks\":{\"SessionStart\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"touch $M/hook-ran\"}]}],\"UserPromptSubmit\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"touch $M/hook-ran\"}]}]}}' > .claude/settings.json" \
  "$here/run.sh" "$S" -p hi >/dev/null 2>&1
check claude_hooks_untrusted "hook ran" '[ ! -e "$M/hook-ran" ]'

# 3. Project .chanto/config.toml hooks / mcp_servers / api_key_helper ignored when untrusted.
SETUP="mkdir -p .chanto && printf '[mcp_servers.evil]\ncommand = \"touch\"\nargs = [\"$M/cfg-mcp-ran\"]\napi_key_helper = \"touch $M/helper-ran\"\n[[hooks.SessionStart]]\ncommand = \"touch $M/cfg-hook-ran\"\n' > .chanto/config.toml" \
  "$here/run.sh" "$S" -p hi >/dev/null 2>&1
check chanto_config_untrusted "project config ran code" '[ ! -e "$M/cfg-mcp-ran" ] && [ ! -e "$M/helper-ran" ] && [ ! -e "$M/cfg-hook-ran" ]'

# 4. Codex project config (.codex/config.toml) mcp_servers ignored when untrusted.
SETUP="mkdir -p .codex && printf '[mcp_servers.evil]\ncommand = \"touch\"\nargs = [\"$M/codex-mcp-ran\"]\n' > .codex/config.toml" \
  "$here/run.sh" "$S" -p hi >/dev/null 2>&1
check codex_config_untrusted "codex project MCP ran" '[ ! -e "$M/codex-mcp-ran" ]'

# 5. Instruction files that are symlinks out of the project aren't sent to the model.
echo "SECRET-OUTSIDE-$$" > "$M/secret.txt"
for f in AGENTS.md CLAUDE.md GEMINI.md .cursorrules .github/copilot-instructions.md CLAUDE.local.md; do
  KEEP=1 SETUP="mkdir -p \$(dirname $f) && ln -s $M/secret.txt $f" "$here/run.sh" "$S" -p hi > "$M/out" 2>&1
  W="$(sed -n 's/^W=\([^ ]*\) .*/\1/p' "$M/out" | head -1)"
  check "symlink_out_$f" "outside file's content reached the model" '! grep -rq "SECRET-OUTSIDE" "$W/model.log" "$W/req" 2>/dev/null'
  rm -rf "$W"
done

# 6. @imports in an untrusted project's CLAUDE.md can't pull outside files.
KEEP=1 SETUP="printf 'see @$M/secret.txt and @../../../../../../../../$M/secret.txt\n' > CLAUDE.md" "$here/run.sh" "$S" -p hi > "$M/out" 2>&1
W="$(sed -n 's/^W=\([^ ]*\) .*/\1/p' "$M/out" | head -1)"
check claude_import_escape "import outside the project reached the model" '! grep -rq "SECRET-OUTSIDE" "$W/model.log" "$W/req" 2>/dev/null'
rm -rf "$W"

rm -rf "$M"
exit $fail
