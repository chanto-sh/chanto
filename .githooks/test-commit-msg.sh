#!/bin/sh
# Tests for .githooks/commit-msg; CI runs this so the rules can't regress.
#   sh .githooks/test-commit-msg.sh
set -eu
export LC_ALL=C

hook="$(dirname "$0")/commit-msg"
tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT
failures=0

check() {
    want=$1
    printf '%b' "$2" >"$tmp"
    if sh "$hook" "$tmp" >/dev/null 2>&1; then got=pass; else got=fail; fi
    if [ "$got" != "$want" ]; then
        printf 'FAIL: expected %s, got %s: %s\n' "$want" "$got" "$(head -n 1 "$tmp")"
        failures=$((failures + 1))
    fi
}

check pass 'fix(sandbox): allow writes to the cargo registry cache'
check pass 'feat!: drop the --legacy flag\n\nBREAKING CHANGE: use --mode instead.'
check pass 'build(deps): bump tokio from 1.40 to 1.41'
check pass 'docs: update README'
check pass 'refactor(tui): split TUI rendering into submodules\n\nIt grew past 3k lines.'
check pass "Merge branch 'main' into topic"
check pass 'Revert "fix(tui): wrap long lines"'
check pass 'fixup! fix(tui): wrap long lines'
check pass 'chore: tidy\n# Please enter the commit message\n# ------------------------ >8 ------------------------\ndiff --git a/x b/x'

check fail 'Fix the sandbox'
check fail 'fix: Added retries'
check fail 'fix: add retries.'
check fail 'feature: add retries'
check fail 'fix(TUI): add retries'
check fail 'fix:add retries'
check fail 'fix: '
check fail 'fix: add retries\nbody without a blank line'
check fail 'fix: this subject line is deliberately far too long to fit in seventy-two chars'
# CI's longer limit for Dependabot titles.
export COMMIT_SUBJECT_MAX=100
check pass 'build(deps): bump crc32fast from 1.5.2 to 1.5.3 in the deps group across 1 directory'
unset COMMIT_SUBJECT_MAX
check fail 'fix: add retries\n\nCo-Authored-By: Claude <noreply@anthropic.com>'
check fail 'fix: add retries\n\nGenerated with ChatGPT'

if [ "$failures" -ne 0 ]; then
    echo "$failures commit-msg case(s) failed" >&2
    exit 1
fi
echo "commit-msg: all cases pass"
