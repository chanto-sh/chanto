#!/usr/bin/env bash
# Create or edit a pull request after checking its title and description the
# way CI does. Every argument is passed through to gh.
#
#   scripts/pr.sh create --title "fix(tui): keep the cursor visible" \
#       --body-file /tmp/body.md
#   scripts/pr.sh edit 299 --title "fix(tui): keep the cursor visible" \
#       --body-file /tmp/body.md
#
# The description must come from --body-file (start from
# .github/pull_request_template.md); an empty or template-only body fails.
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
title=
body=
args=()
while (($#)); do
    case "$1" in
        --title)
            title=${2:-}
            args+=("$1" "${2:-}")
            shift 2
            ;;
        --title=*)
            title=${1#--title=}
            args+=("$1")
            shift
            ;;
        --body-file)
            body=${2:-}
            args+=("$1" "${2:-}")
            shift 2
            ;;
        --body-file=*)
            body=${1#--body-file=}
            args+=("$1")
            shift
            ;;
        *)
            args+=("$1")
            shift
            ;;
    esac
done

if ! command -v gh >/dev/null 2>&1; then
    echo "pr: gh is not installed; see https://cli.github.com" >&2
    exit 1
fi
if [[ -z $body ]]; then
    echo "pr: pass --body-file <file> (start from .github/pull_request_template.md) so the description can be checked" >&2
    exit 1
fi

cleanup=()
cleanup+=("$(mktemp)")
trap 'rm -f "${cleanup[@]}"' EXIT

if [[ -n $title ]]; then
    printf '%s\n' "$title" >"${cleanup[0]}"
    (
        cd "$root"
        # A title has no sign-off; the branch's own commits are checked by DCO.
        REQUIRE_SIGNOFF=0 COMMIT_SUBJECT_MAX=72 sh .githooks/commit-msg "${cleanup[0]}"
    )
fi

python3 "$root/scripts/check-pr.py" --body-file "$body"

exec gh pr "${args[@]}"
